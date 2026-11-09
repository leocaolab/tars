//! Folds a provider's [`ChatEvent`] stream into one [`ChatResponse`], and unfolds a response
//! back into events for a layer that replays it.

use serde::{Deserialize, Serialize};

use crate::providers::cache::CacheHitInfo;
use crate::providers::chat::Degradation;
use crate::providers::events::{ChatEvent, StopReason};
use crate::providers::tools::ToolCall;
use crate::providers::usage::Usage;
use crate::validation::ValidationSummary;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatResponse {
    /// The model that actually answered.
    pub actual_model: String,
    /// The concatenated text, without thinking.
    pub text: String,
    /// The concatenated thinking; empty when there was none.
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
    pub cache_hit: CacheHitInfo,
    /// What the provider served the request without, from the dispatch check; empty when it
    /// served all of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<Degradation>,
    /// Empty when the pipeline has no `ValidationMiddleware`; event-log dumps written without it
    /// still load.
    #[serde(default, skip_serializing_if = "is_empty_validation_summary")]
    pub validation_summary: ValidationSummary,
    /// When the stream ended and the answer was in hand, in Unix seconds (negative before 1970),
    /// stamped by [`ChatResponseBuilder::finish`]. Anything derived from this response is dated by
    /// it, not by when a consumer later stores it. Dumps written without it read it as 0.
    #[serde(default)]
    pub created: i64,
}

fn is_empty_validation_summary(s: &ValidationSummary) -> bool {
    s.outcomes.is_empty() && s.validators_run.is_empty() && s.total_wall_ms == 0
}

/// Returns the wall-clock time in Unix seconds, negative for a clock set before 1970.
fn now_unix_secs() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

impl ChatResponse {
    /// Returns whether the stream reached its terminal event.
    pub fn is_finished(&self) -> bool {
        self.stop_reason.is_some()
    }

    /// Returns the response as the events of a stream, so a replayed response reads like a fresh
    /// call to the middleware above: `Started` (carrying the given `cache_hit`), thinking and text
    /// when not empty, each tool call's start and end, and the end [`ChatEvent::stream_end`] makes:
    /// `Finished`, or only the usage for a response whose stream never said why the turn stopped.
    /// The validation summary, `degraded` and `created` are dropped; each is made again when the
    /// replayed stream passes the dispatch check and finishes.
    pub fn into_events(self, cache_hit: CacheHitInfo) -> Vec<ChatEvent> {
        let Self {
            actual_model,
            text,
            thinking,
            tool_calls,
            stop_reason,
            usage,
            cache_hit: _,
            degraded: _,
            validation_summary: _,
            created: _,
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
        out.extend(ChatEvent::stream_end(stop_reason, Some(usage)));
        out
    }
}

/// Accumulates [`ChatEvent`]s into a [`ChatResponse`].
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

    /// Folds one event into the response. A protocol violation (a second start for one tool
    /// index, arguments or an end with no start, an end whose id differs from its start) is logged
    /// and does not fail.
    pub fn apply(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::Degraded { unsent } => {
                self.inner.degraded = unsent;
            }
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
                // An entry made here would be a tool call with no id or name.
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
                self.inner.tool_calls.push(
                    ToolCall::new(id, name, parsed_args).with_thought_signature(thought_signature),
                );
            }
            ChatEvent::UsageProgress { usage } => {
                // `Finished` carries the authoritative figure; until then keep the high-water mark.
                self.inner.usage = self.inner.usage.max_each(usage);
            }
            // A measurement is the call's, not the response's; a caller that wants it folds it
            // apart.
            ChatEvent::Measured { .. } => {}
            ChatEvent::Validated { summary } => {
                self.inner.validation_summary = summary;
            }
            ChatEvent::Finished { stop_reason, usage } => {
                self.inner.stop_reason = Some(stop_reason);
                self.inner.usage = usage;
            }
        }
    }

    /// Returns the accumulated response stamped with the current time. An incomplete stream (no
    /// `Finished`, or tool calls started and never ended, which are dropped) only logs a warning.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        match v {
            serde_json::Value::Object(m) => m,
            other => panic!("test arguments must be an object, got {other}"),
        }
    }

    #[test]
    fn a_degraded_event_lands_on_the_response_and_its_absence_leaves_it_empty() {
        let finished = ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        };
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::Degraded {
            unsent: vec![Degradation::StructuredOutputNotEnforced],
        });
        b.apply(ChatEvent::started("m"));
        b.apply(finished.clone());
        assert_eq!(
            b.finish().degraded,
            [Degradation::StructuredOutputNotEnforced]
        );

        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::started("m"));
        b.apply(finished);
        assert!(b.finish().degraded.is_empty());
    }

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
        let original = ChatResponse {
            actual_model: "gpt-4o".into(),
            text: "hello world".into(),
            thinking: "ponder".into(),
            tool_calls: vec![ToolCall::new(
                "call_1",
                "search",
                args(json!({"q": "rust"})),
            )],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 4,
                ..Default::default()
            },
            cache_hit: CacheHitInfo::default(),
            degraded: Vec::new(),
            validation_summary: Default::default(),
            created: 0,
        };
        let events = original.clone().into_events(CacheHitInfo {
            cached_input_tokens: 8,
            used_explicit_handle: false,
        });
        let mut b = ChatResponseBuilder::new();
        for ev in events {
            b.apply(ev);
        }
        let back = b.finish();
        assert_eq!(back.text, original.text);
        assert_eq!(back.thinking, original.thinking);
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].arguments, args(json!({"q": "rust"})));
        assert_eq!(back.stop_reason, original.stop_reason);
        assert_eq!(back.usage.input_tokens, 10);
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
            degraded: Vec::new(),
            validation_summary: Default::default(),
            created: 0,
        };
        let events = r.into_events(CacheHitInfo::default());
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ChatEvent::Started { .. }));
        assert!(matches!(events[1], ChatEvent::Finished { .. }));
    }

    #[test]
    fn a_response_with_no_stop_reason_replays_its_usage_without_finished() {
        let r = ChatResponse {
            actual_model: "m".into(),
            text: "partial".into(),
            thinking: String::new(),
            tool_calls: vec![],
            stop_reason: None,
            usage: Usage {
                input_tokens: 1000,
                output_tokens: 500,
                ..Default::default()
            },
            cache_hit: CacheHitInfo::default(),
            degraded: Vec::new(),
            validation_summary: Default::default(),
            created: 0,
        };
        let events = r.into_events(CacheHitInfo::default());
        assert!(!events.iter().any(ChatEvent::is_terminal), "{events:?}");
        let mut b = ChatResponseBuilder::new();
        for ev in events {
            b.apply(ev);
        }
        let back = b.finish();
        assert_eq!(back.stop_reason, None);
        assert_eq!(
            (back.usage.input_tokens, back.usage.output_tokens),
            (1000, 500)
        );
    }

    #[test]
    fn usage_progress_keeps_each_figures_highest_until_finished_replaces_them() {
        let report = |input_tokens, cached_input_tokens| ChatEvent::UsageProgress {
            usage: Usage {
                input_tokens,
                cached_input_tokens,
                ..Default::default()
            },
        };
        let mut b = ChatResponseBuilder::new();
        b.apply(report(100, 80));
        b.apply(report(90, 0));
        let progress = b.inner.usage;
        assert_eq!(
            (progress.input_tokens, progress.cached_input_tokens),
            (100, 80)
        );
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 50,
                ..Default::default()
            },
        });
        let done = b.finish().usage;
        assert_eq!((done.input_tokens, done.cached_input_tokens), (50, 0));
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
        assert!(
            r.created > 0,
            "finish() must stamp `created` with the finalize wall-clock"
        );
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
            parsed_args: args(json!({"q": "rust"})),
            thought_signature: None,
        });
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        let r = b.finish();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "search");
        assert_eq!(r.tool_calls[0].arguments, args(json!({"q": "rust"})));
    }
}
