//! Streaming tool call accumulator. See Doc 01 §8.1.
//!
//! Critical invariant: **never** attempt to parse the args mid-stream.
//! Increment buffers only on `args_delta`; parse + repair at `finalize`.

use std::collections::HashMap;

use serde_json::Value;
use tars_types::{ProviderError, StopReason};

/// Per-call slot for accumulating streaming tool args.
#[derive(Debug, Default)]
struct CallAccum {
    id: String,
    name: String,
    args: String,
    /// Set true once we observed `start`; without it the buffer is
    /// mid-stream and we shouldn't finalize.
    started: bool,
}

/// Accumulates streaming tool call deltas keyed by `index`.
///
/// OpenAI / Anthropic both interleave parallel tool calls by `index`;
/// you cannot just concatenate everything you see.
///
/// Also carries a few non-tool stream-level flags (e.g. whether
/// [`tars_types::ChatEvent::Started`] has been emitted) so adapter
/// `parse_event` impls can stay stateless.
#[derive(Debug, Default)]
pub struct ToolCallBuffer {
    inflight: HashMap<usize, CallAccum>,
    /// Whether the adapter has emitted a `Started` event yet for the
    /// stream this buffer belongs to. Lets adapters call
    /// [`Self::take_started`] once and skip the rest.
    started_emitted: bool,
    /// Stop reason captured from a `finish_reason` chunk that didn't
    /// carry usage data; will be paired with a later usage-only chunk.
    /// Audit `tars-provider-src-backends-openai-{7,22}` — previously a
    /// usage-only chunk emitted Finished with a hardcoded EndTurn,
    /// silently overriding the real reason (ToolUse, MaxTokens, …).
    pending_stop_reason: Option<StopReason>,
}

impl ToolCallBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stash a stop reason captured from a `finish_reason` chunk so
    /// the adapter can pair it with a usage-only follow-up chunk.
    pub fn record_pending_stop(&mut self, reason: StopReason) {
        self.pending_stop_reason = Some(reason);
    }

    /// Take (and clear) the previously stashed stop reason. Returns
    /// `None` if nothing was stashed.
    pub fn take_pending_stop(&mut self) -> Option<StopReason> {
        self.pending_stop_reason.take()
    }

    /// Atomic test-and-set for "have we already emitted Started?".
    /// Returns `true` the first time it's called per stream; `false`
    /// thereafter. Adapters use this to emit at most one Started
    /// event without carrying state in the adapter struct itself.
    pub fn take_started(&mut self) -> bool {
        if self.started_emitted {
            false
        } else {
            self.started_emitted = true;
            true
        }
    }

    pub fn on_start(&mut self, index: usize, id: String, name: String) {
        let entry = self.inflight.entry(index).or_default();
        // Audit `tars-provider-src-tool-buffer-{2,17}`: previously this
        // unconditionally cleared `args`, dropping any deltas that
        // arrived before the start event (some adapters emit deltas
        // first) AND silently overwriting an in-flight call if a
        // provider erroneously sent a duplicate start. Two policies:
        // 1. If args have already accumulated, KEEP them — the deltas
        //    belong to this same logical call (id/name catch up later).
        // 2. If we're seeing a re-start on an already-started entry,
        //    log a warning and keep existing args (don't lose data).
        if entry.started {
            tracing::warn!(
                index,
                old_id = %entry.id,
                new_id = %id,
                "duplicate ToolCallStart for index; preserving accumulated args",
            );
        }
        // Only adopt id/name if they're non-empty — partial start chunks
        // shouldn't blank out values we already had.
        if !id.is_empty() {
            entry.id = id;
        }
        if !name.is_empty() {
            entry.name = name;
        }
        entry.started = true;
    }

    pub fn on_delta(&mut self, index: usize, delta: &str) {
        // Spec allows deltas before/without an explicit `start` event
        // (some providers omit it). Tolerate by upserting.
        let entry = self.inflight.entry(index).or_default();
        entry.args.push_str(delta);
        entry.started = true;
    }

    /// Finalize the call at `index`. Three-stage parse:
    ///
    /// 1. `serde_json::from_str` strict
    /// 2. fall back to repair for trailing-comma / unclosed-string cases
    /// 3. otherwise propagate `ProviderError::Parse`
    pub fn finalize(&mut self, index: usize) -> Result<(String, String, Value), ProviderError> {
        // Audit `tars-provider-src-tool-buffer-9`: peek before remove —
        // if parsing fails, leave the buffer intact so the caller can
        // wait for more deltas and retry instead of losing accumulated
        // partial args.
        let acc = self
            .inflight
            .get(&index)
            .ok_or_else(|| ProviderError::Parse(format!("tool call index {index} not started")))?;

        if !acc.started {
            return Err(ProviderError::Parse(format!(
                "tool call index {index} has no start"
            )));
        }

        // Audit `tars-provider-src-tool-buffer-7`: a finalized tool call
        // must have a non-empty id and name. Without them, downstream
        // (Pipeline, Agent loop) cannot route the result back, and most
        // providers will reject the follow-up tool result message.
        if acc.id.is_empty() || acc.name.is_empty() {
            return Err(ProviderError::Parse(format!(
                "tool call index {index} finalized with empty id or name (id=`{}`, name=`{}`)",
                acc.id, acc.name,
            )));
        }

        let parse_attempt = serde_json::from_str::<Value>(&acc.args);
        let value = match parse_attempt {
            Ok(v) => v,
            Err(first_err) => {
                // Cheap repair: many provider streams emit empty for empty
                // arg sets and otherwise valid JSON. Empty string -> {}.
                let trimmed = acc.args.trim();
                if trimmed.is_empty() {
                    Value::Object(Default::default())
                } else {
                    // Try wrapping naked key:value into braces (rare).
                    let braced = format!("{{{trimmed}}}");
                    serde_json::from_str::<Value>(&braced).map_err(|repair_err| {
                        // Audit `tars-provider-src-tool-buffer-1`: keep
                        // both errors so debugging malformed provider
                        // JSON has full context.
                        ProviderError::Parse(format!(
                            "tool call args parse failed (first: {first_err}; repair: {repair_err}; raw: {})",
                            crate::http_base::truncate_utf8(&acc.args, 200)
                        ))
                    })?
                }
            }
        };

        // Parsing succeeded — now consume the buffer.
        let acc = self.inflight.remove(&index).expect("we just confirmed presence");
        Ok((acc.id, acc.name, value))
    }

    /// Whether any tool call is currently in-flight.
    pub fn has_inflight(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// Drain all remaining buffers without finalizing — used on stream
    /// abort to avoid leaking partial state.
    pub fn discard(&mut self) {
        self.inflight.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accumulates_split_args_then_parses() {
        let mut b = ToolCallBuffer::new();
        b.on_start(0, "c1".into(), "search".into());
        b.on_delta(0, "{\"q\":\"r");
        b.on_delta(0, "ust\"}");
        let (id, name, args) = b.finalize(0).unwrap();
        assert_eq!(id, "c1");
        assert_eq!(name, "search");
        assert_eq!(args, json!({"q": "rust"}));
    }

    #[test]
    fn empty_args_become_empty_object() {
        let mut b = ToolCallBuffer::new();
        b.on_start(0, "c1".into(), "ping".into());
        let (_, _, args) = b.finalize(0).unwrap();
        assert_eq!(args, json!({}));
    }

    #[test]
    fn parallel_calls_keyed_by_index() {
        let mut b = ToolCallBuffer::new();
        b.on_start(0, "a".into(), "f1".into());
        b.on_start(1, "b".into(), "f2".into());
        b.on_delta(0, "{\"x\":1}");
        b.on_delta(1, "{\"y\":2}");
        let (_, name0, args0) = b.finalize(0).unwrap();
        let (_, name1, args1) = b.finalize(1).unwrap();
        assert_eq!(name0, "f1");
        assert_eq!(args0, json!({"x":1}));
        assert_eq!(name1, "f2");
        assert_eq!(args1, json!({"y":2}));
    }

    #[test]
    fn finalize_unknown_index_errors() {
        let mut b = ToolCallBuffer::new();
        let err = b.finalize(99).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn malformed_json_propagates_parse_error() {
        let mut b = ToolCallBuffer::new();
        b.on_start(0, "c1".into(), "f".into());
        b.on_delta(0, "not json at all");
        let err = b.finalize(0).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn discard_clears_state() {
        let mut b = ToolCallBuffer::new();
        b.on_start(0, "c1".into(), "f".into());
        assert!(b.has_inflight());
        b.discard();
        assert!(!b.has_inflight());
    }
}
