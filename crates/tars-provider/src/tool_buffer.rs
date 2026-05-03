//! Streaming tool call accumulator. See Doc 01 §8.1.
//!
//! Critical invariant: **never** attempt to parse the args mid-stream.
//! Increment buffers only on `args_delta`; parse + repair at `finalize`.

use std::collections::HashMap;

use serde_json::Value;
use tars_types::ProviderError;

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
}

impl ToolCallBuffer {
    pub fn new() -> Self {
        Self::default()
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
        entry.id = id;
        entry.name = name;
        entry.args.clear();
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
        let acc = self
            .inflight
            .remove(&index)
            .ok_or_else(|| ProviderError::Parse(format!("tool call index {index} not started")))?;

        if !acc.started {
            return Err(ProviderError::Parse(format!(
                "tool call index {index} has no start"
            )));
        }

        let value = serde_json::from_str::<Value>(&acc.args).or_else(|_first_err| {
            // Cheap repair: many provider streams emit `null` for empty
            // arg sets and otherwise valid JSON. Empty string -> {}.
            let trimmed = acc.args.trim();
            if trimmed.is_empty() {
                return Ok(Value::Object(Default::default()));
            }
            // Try wrapping naked key:value into braces (rare).
            let braced = format!("{{{trimmed}}}");
            serde_json::from_str::<Value>(&braced)
        })?;

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
