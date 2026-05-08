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
    /// pre-validation EventStore dumps still deserialize cleanly.
    #[serde(default, skip_serializing_if = "is_empty_validation_summary")]
    pub validation_summary: ValidationSummary,
}

fn is_empty_validation_summary(s: &ValidationSummary) -> bool {
    s.outcomes.is_empty() && s.validators_run.is_empty() && s.total_wall_ms == 0
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
            });
        }
        // A cached response that lacks a stop_reason is incoherent (we
        // refuse to cache mid-stream failures), but preserve EndTurn as
        // a sane default so consumers always see a terminal event.
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
                let entry = self.tool_args_buffer.entry(index).or_default();
                entry.id = id;
                entry.name = name;
                entry.args.clear();
            }
            ChatEvent::ToolCallArgsDelta { index, args_delta } => {
                let entry = self.tool_args_buffer.entry(index).or_default();
                entry.args.push_str(&args_delta);
            }
            ChatEvent::ToolCallEnd {
                index,
                id,
                parsed_args,
            } => {
                // Prefer the provider's parsed value; if `index` is
                // missing we still record the call.
                let name = self
                    .tool_args_buffer
                    .remove(&index)
                    .map(|a| a.name)
                    .unwrap_or_default();
                self.inner
                    .tool_calls
                    .push(ToolCall::new(id, name, parsed_args));
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
    pub fn finish(self) -> ChatResponse {
        self.inner
    }
}

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
        };
        let events = r.into_events(CacheHitInfo::default());
        // Just Started + Finished — no empty Delta to confuse consumers.
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ChatEvent::Started { .. }));
        assert!(matches!(events[1], ChatEvent::Finished { .. }));
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
