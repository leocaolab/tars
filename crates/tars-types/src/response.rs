//! Aggregated response — what `Provider::complete` returns by default
//! after consuming the streaming `ChatEvent`s.

use serde::{Deserialize, Serialize};

use crate::cache::CacheHitInfo;
use crate::events::{ChatEvent, StopReason};
use crate::tools::ToolCall;
use crate::usage::Usage;
use crate::validation::ValidationSummary;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Resolved model that actually answered.
    pub actual_model: String,
    /// Concatenated text content (excluding thinking).
    pub text: String,
    /// Concatenated thinking content, if any.
    pub thinking: String,
    /// Tool calls the model emitted in this turn.
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
    pub cache_hit: CacheHitInfo,
    /// Per-call validator outcomes (Doc 15). Empty when the pipeline
    /// didn't include `ValidationMiddleware`. `#[serde(default)]` so
    /// pre-validation AgentEventLog dumps still deserialize cleanly.
    #[serde(default, skip_serializing_if = "is_empty_validation_summary")]
    pub validation_summary: ValidationSummary,
    /// Unix-seconds wall-clock when this response was finalized — i.e. when
    /// the stream completed and the model's answer was in hand. Set once by
    /// [`ChatResponseBuilder::finish`] / `finish_checked`, so EVERY response
    /// self-reports "when did the model answer" regardless of provider. This
    /// is the call's DISCOVERY time: observability/debugging, and the honest
    /// `at` for anything a consumer derives from this response (decoupled from
    /// when that consumer later persists it). `#[serde(default)]` so older
    /// AgentEventLog dumps (no `created`) still deserialize.
    #[serde(default)]
    pub created: i64,
}

fn is_empty_validation_summary(s: &ValidationSummary) -> bool {
    s.outcomes.is_empty() && s.validators_run.is_empty() && s.total_wall_ms == 0
}

/// Unix-seconds wall-clock now, as a SIGNED offset from the epoch — total, no
/// information lost: a clock set before 1970 yields a negative value (honest)
/// rather than being clamped to a fake `0` or panicking. Stamped onto every
/// [`ChatResponse`] at finalize.
fn now_unix_secs() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // `now` is before the epoch: `e.duration()` is how far before.
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

impl ChatResponse {
    pub fn is_finished(&self) -> bool {
        self.stop_reason.is_some()
    }

    /// Replay this response as a sequence of [`ChatEvent`]s. Used by
    /// the cache layer to short-circuit a hit back into the streaming
    /// contract — middleware above the cache can't tell the difference
    /// between a fresh response and a replayed one.
    ///
    /// `cache_hit` is overwritten on the emitted Started event so
    /// observers can flag the replay; the rest of the fields are
    /// reconstituted in their original order:
    /// Started → ThinkingDelta? → Delta? → tool calls → Finished.
    pub fn into_events(self, cache_hit: CacheHitInfo) -> Vec<ChatEvent> {
        let Self {
            actual_model,
            text,
            thinking,
            tool_calls,
            stop_reason,
            usage,
            cache_hit: _,          // overridden by argument
            validation_summary: _, // discard: validators rerun on hit (Doc 15 §4)
            created: _,            // re-stamped when the replayed stream re-finishes
        } = self;

        let mut out = Vec::with_capacity(3 + tool_calls.len() * 2);
        out.push(ChatEvent::Started {
            actual_model,
            cache_hit,
        });
        if !thinking.is_empty() {
            out.push(ChatEvent::ThinkingDelta { text: thinking });
        }
        if !text.is_empty() {
            out.push(ChatEvent::Delta { text });
        }
        for (index, tc) in tool_calls.into_iter().enumerate() {
            out.push(ChatEvent::ToolCallStart {
                index,
                id: tc.id.clone(),
                name: tc.name.clone(),
            });
            out.push(ChatEvent::ToolCallEnd {
                index,
                id: tc.id,
                parsed_args: tc.arguments,
                thought_signature: tc.thought_signature,
            });
        }
        // A cached response that lacks a stop_reason is incoherent (we
        // refuse to cache mid-stream failures). Enforce that invariant
        // loudly in CI; in release, fall back to EndTurn so consumers
        // still see a terminal event rather than a truncated stream.
        debug_assert!(
            stop_reason.is_some(),
            "into_events called on a response with no stop_reason — \
             a mid-stream/incomplete response must never reach the cache-replay path",
        );
        out.push(ChatEvent::Finished {
            stop_reason: stop_reason.unwrap_or(StopReason::EndTurn),
            usage,
        });
        out
    }
}

/// Stateful builder that consumes [`ChatEvent`]s and produces a
/// [`ChatResponse`]. Adapters use this both for `complete()`'s default
/// implementation and for tests.
#[derive(Debug, Default)]
pub struct ChatResponseBuilder {
    inner: ChatResponse,
    tool_args_buffer: std::collections::HashMap<usize, ToolAccum>,
}

#[derive(Debug, Default)]
struct ToolAccum {
    id: String,
    name: String,
    args: String,
}

impl ChatResponseBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event. Borrows by reference so callers don't have to
    /// move the event out of their own state.
    pub fn apply(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::Started {
                actual_model,
                cache_hit,
            } => {
                self.inner.actual_model = actual_model;
                self.inner.cache_hit = cache_hit;
            }
            ChatEvent::Delta { text } => {
                self.inner.text.push_str(&text);
            }
            ChatEvent::ThinkingDelta { text } => {
                self.inner.thinking.push_str(&text);
            }
            ChatEvent::ToolCallStart { index, id, name } => {
                // A second Start for the same index is a provider
                // protocol violation — the first call's accumulated
                // args would be silently dropped. Log it rather than
                // overwrite blindly.
                if self.tool_args_buffer.contains_key(&index) {
                    tracing::warn!(
                        index,
                        new_id = %id,
                        "duplicate ToolCallStart for index — overwriting prior partial tool call"
                    );
                }
                let entry = self.tool_args_buffer.entry(index).or_default();
                entry.id = id;
                entry.name = name;
                entry.args.clear();
            }
            ChatEvent::ToolCallArgsDelta { index, args_delta } => {
                // An args delta for an index we never saw a Start for
                // would create an entry with an empty id/name — a
                // malformed tool call. Skip it and log; don't fabricate.
                match self.tool_args_buffer.get_mut(&index) {
                    Some(entry) => entry.args.push_str(&args_delta),
                    None => tracing::warn!(
                        index,
                        "ToolCallArgsDelta for index with no prior ToolCallStart — dropping delta"
                    ),
                }
            }
            ChatEvent::ToolCallEnd {
                index,
                id,
                parsed_args,
                thought_signature,
            } => {
                // Prefer the provider's parsed value. Correlate against
                // the buffered Start: a missing Start or an id mismatch
                // is a protocol violation we surface rather than paper
                // over with an empty name / wrong id.
                let started = self.tool_args_buffer.remove(&index);
                let name = match &started {
                    Some(a) => {
                        if !a.id.is_empty() && a.id != id {
                            tracing::warn!(
                                index,
                                start_id = %a.id,
                                end_id = %id,
                                "ToolCallEnd id does not match the ToolCallStart id for this index"
                            );
                        }
                        a.name.clone()
                    }
                    None => {
                        tracing::warn!(
                            index,
                            id = %id,
                            "ToolCallEnd with no prior ToolCallStart — tool call will have an empty name"
                        );
                        String::new()
                    }
                };
                self.inner
                    .tool_calls
                    .push(ToolCall::new(id, name, parsed_args).with_thought_signature(thought_signature));
            }
            ChatEvent::UsageProgress { partial } => {
                // Don't overwrite — we'll get the authoritative figure
                // in Finished. Just track high-water.
                self.inner.usage.input_tokens =
                    self.inner.usage.input_tokens.max(partial.input_tokens);
                self.inner.usage.output_tokens =
                    self.inner.usage.output_tokens.max(partial.output_tokens);
            }
            ChatEvent::Finished { stop_reason, usage } => {
                self.inner.stop_reason = Some(stop_reason);
                self.inner.usage = usage;
            }
        }
    }

    /// Finalize the response. After this you get the accumulated
    /// `ChatResponse`. If the stream was terminated abnormally
    /// (`stop_reason == None`), the caller decides whether to treat
    /// it as an error.
    ///
    /// Logs a warning when the stream looks incomplete — no terminal
    /// `Finished` event (`stop_reason == None`) and/or tool-call Starts
    /// that never got a matching End (their buffered args are dropped).
    /// Use [`finish_checked`](Self::finish_checked) when you want that
    /// surfaced as a hard error instead of a log line.
    pub fn finish(mut self) -> ChatResponse {
        self.inner.created = now_unix_secs();
        if self.inner.stop_reason.is_none() {
            tracing::warn!(
                buffered_tool_calls = self.tool_args_buffer.len(),
                "ChatResponseBuilder::finish on a stream with no Finished event (stop_reason=None)"
            );
        } else if !self.tool_args_buffer.is_empty() {
            tracing::warn!(
                buffered_tool_calls = self.tool_args_buffer.len(),
                "ChatResponseBuilder::finish dropping tool-call Start(s) that never got a matching End"
            );
        }
        self.inner
    }

    /// Like [`finish`](Self::finish) but rejects an incomplete stream:
    /// `Err` carries the partially-built response so the caller can
    /// still inspect/log it. Incomplete means either no terminal
    /// `Finished` event, or one or more tool-call Starts that never
    /// received a matching End (whose buffered args would be lost).
    pub fn finish_checked(mut self) -> Result<ChatResponse, Box<IncompleteStream>> {
        self.inner.created = now_unix_secs();
        // `IncompleteStream` embeds a full `ChatResponse`, so the Err
        // variant is large; box it to keep `Result` cheap to move on the
        // (common) Ok path — the allocation only happens on the rare
        // incomplete-stream error. Satisfies `clippy::result_large_err`.
        let no_terminal = self.inner.stop_reason.is_none();
        let unfinished_tool_calls = self.tool_args_buffer.len();
        if no_terminal || unfinished_tool_calls > 0 {
            Err(Box::new(IncompleteStream {
                no_terminal,
                unfinished_tool_calls,
                partial: self.inner,
            }))
        } else {
            Ok(self.inner)
        }
    }
}

/// Returned by [`ChatResponseBuilder::finish_checked`] when the event
/// stream ended in a non-terminal state.
#[derive(Debug)]
pub struct IncompleteStream {
    /// True iff no `Finished` event was applied (`stop_reason == None`).
    pub no_terminal: bool,
    /// Count of tool-call Starts left in the buffer with no matching End.
    pub unfinished_tool_calls: usize,
    /// The partially-accumulated response, so callers can still inspect
    /// or log what arrived before the stream broke.
    pub partial: ChatResponse,
}

impl std::fmt::Display for IncompleteStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "incomplete chat stream (no_terminal={}, unfinished_tool_calls={})",
            self.no_terminal, self.unfinished_tool_calls
        )
    }
}

impl std::error::Error for IncompleteStream {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builder_concatenates_deltas() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::started("gpt-4o"));
        b.apply(ChatEvent::Delta {
            text: "Hello, ".into(),
        });
        b.apply(ChatEvent::Delta {
            text: "world!".into(),
        });
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        });
        let r = b.finish();
        assert_eq!(r.text, "Hello, world!");
        assert!(r.is_finished());
    }

    #[test]
    fn into_events_round_trips_through_the_builder() {
        // A response → events → builder → response cycle should be
        // structurally lossless for everything cache replay needs.
        let original = ChatResponse {
            actual_model: "gpt-4o".into(),
            text: "hello world".into(),
            thinking: "ponder".into(),
            tool_calls: vec![ToolCall::new("call_1", "search", json!({"q": "rust"}))],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 4,
                ..Default::default()
            },
            cache_hit: CacheHitInfo::default(),
            validation_summary: Default::default(),
            created: 0,
        };
        let events = original.clone().into_events(CacheHitInfo {
            cached_input_tokens: 8,
            used_explicit_handle: false,
            replayed_from_cache: true,
        });
        let mut b = ChatResponseBuilder::new();
        for ev in events {
            b.apply(ev);
        }
        let back = b.finish();
        assert_eq!(back.text, original.text);
        assert_eq!(back.thinking, original.thinking);
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].arguments, json!({"q": "rust"}));
        assert_eq!(back.stop_reason, original.stop_reason);
        assert_eq!(back.usage.input_tokens, 10);
        // cache_hit was overwritten — verify it carried through.
        assert_eq!(back.cache_hit.cached_input_tokens, 8);
    }

    #[test]
    fn into_events_skips_empty_text_and_thinking_blocks() {
        let r = ChatResponse {
            actual_model: "m".into(),
            text: String::new(),
            thinking: String::new(),
            tool_calls: vec![],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
            cache_hit: CacheHitInfo::default(),
            validation_summary: Default::default(),
            created: 0,
        };
        let events = r.into_events(CacheHitInfo::default());
        // Just Started + Finished — no empty Delta to confuse consumers.
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ChatEvent::Started { .. }));
        assert!(matches!(events[1], ChatEvent::Finished { .. }));
    }

    #[test]
    fn finish_stamps_created_so_every_response_self_reports_when_it_answered() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::Started {
            actual_model: "m".into(),
            cache_hit: CacheHitInfo::default(),
        });
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        });
        let r = b.finish();
        // The whole point: a finalized response carries its DISCOVERY time —
        // common infra, every consumer can read it instead of guessing.
        assert!(r.created > 0, "finish() must stamp `created` with the finalize wall-clock");
        // finish_checked takes the same path.
        let mut b2 = ChatResponseBuilder::new();
        b2.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        });
        assert!(b2.finish_checked().unwrap().created > 0);
    }

    #[test]
    fn builder_collects_tool_calls() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::ToolCallStart {
            index: 0,
            id: "c1".into(),
            name: "search".into(),
        });
        b.apply(ChatEvent::ToolCallArgsDelta {
            index: 0,
            args_delta: "{\"q\":\"rust".into(),
        });
        b.apply(ChatEvent::ToolCallArgsDelta {
            index: 0,
            args_delta: "\"}".into(),
        });
        b.apply(ChatEvent::ToolCallEnd {
            index: 0,
            id: "c1".into(),
            parsed_args: json!({"q": "rust"}),
            thought_signature: None,
        });
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        let r = b.finish();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "search");
        assert_eq!(r.tool_calls[0].arguments, json!({"q": "rust"}));
    }
}
