//! Folds a provider's [`ChatEvent`] stream into one [`ChatResponse`], and unfolds a response
//! back into events for a layer that replays it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::providers::cache::CacheHitInfo;
use crate::providers::chat::Degradation;
use crate::providers::events::{ChatEvent, StopReason};
use crate::providers::tools::ToolCall;
use crate::providers::usage::Usage;
use crate::validation::ValidationSummary;

/// The answer of one finished stream: only a stream that said why the turn stopped makes one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatResponse {
    /// The model that answered, as the provider reported it; `None` when it did not say.
    pub actual_model: Option<String>,
    /// The concatenated text, without thinking.
    pub text: String,
    /// The concatenated thinking; empty when there was none.
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    /// What the provider reported spending; `None` when it reported nothing, which is not zero.
    pub usage: Option<Usage>,
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
    /// Returns the response as the events of a stream, so a replayed response reads like a fresh
    /// call to the middleware above: `Started` (carrying the given `cache_hit`), thinking and text
    /// when not empty, each tool call's start and end, and `Finished`. The validation summary,
    /// `degraded` and `created` are dropped; each is made again when the replayed stream passes
    /// the dispatch check and finishes.
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
        out.push(ChatEvent::Finished { stop_reason, usage });
        out
    }
}

/// A stream that broke the event protocol: the provider's adapter sent tool-call events in an
/// order no response can be folded from.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StreamViolation {
    #[error("a second ToolCallStart for open tool call {index} (id {id})")]
    SecondStart { index: usize, id: String },
    #[error("ToolCallArgsDelta for tool call {index}, which never started")]
    ArgsWithoutStart { index: usize },
    #[error("ToolCallEnd for tool call {index} (id {id}), which never started")]
    EndWithoutStart { index: usize, id: String },
    #[error("ToolCallEnd for tool call {index} has id {end_id}, its start had {start_id}")]
    EndIdMismatch {
        index: usize,
        start_id: String,
        end_id: String,
    },
}

/// A stream that ended without becoming a response.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unfinished {
    /// No `Finished` came: the stream never said why the turn stopped.
    #[error("the stream ended without saying why the turn stopped")]
    NoStopReason,
    /// `Finished` came while these tool calls had started and not ended.
    #[error("the stream finished with tool calls {indices:?} started and never ended")]
    OpenToolCalls { indices: Vec<usize> },
}

/// Accumulates [`ChatEvent`]s into a [`ChatResponse`].
#[derive(Debug, Default)]
pub struct ChatResponseBuilder {
    actual_model: Option<String>,
    text: String,
    thinking: String,
    tool_calls: Vec<ToolCall>,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
    cache_hit: CacheHitInfo,
    degraded: Vec<Degradation>,
    validation_summary: ValidationSummary,
    tool_args_buffer: HashMap<usize, ToolAccum>,
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

    /// Returns the text the stream has delivered so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the tool calls the stream has closed so far, in the order they closed.
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }

    /// Returns the usage the stream has reported so far; `None` until it reports any.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Returns the validators' outcomes the stream has carried so far.
    pub fn validation_summary(&self) -> &ValidationSummary {
        &self.validation_summary
    }

    /// Folds one event into the response.
    ///
    /// # Errors
    ///
    /// Returns the [`StreamViolation`] when the event breaks the tool-call protocol: a second
    /// start for an open index, arguments or an end with no start, an end whose id differs from
    /// its start's. The stream it came from is broken; nothing more is folded from it.
    pub fn apply(&mut self, event: ChatEvent) -> Result<(), StreamViolation> {
        match event {
            ChatEvent::Degraded { unsent } => self.degraded = unsent,
            ChatEvent::Started {
                actual_model,
                cache_hit,
            } => {
                self.actual_model = actual_model;
                self.cache_hit = cache_hit;
            }
            ChatEvent::Delta { text } => self.text.push_str(&text),
            ChatEvent::ThinkingDelta { text } => self.thinking.push_str(&text),
            ChatEvent::ToolCallStart { index, id, name } => {
                if self.tool_args_buffer.contains_key(&index) {
                    return Err(StreamViolation::SecondStart { index, id });
                }
                self.tool_args_buffer.insert(
                    index,
                    ToolAccum {
                        id,
                        name,
                        args: String::new(),
                    },
                );
            }
            ChatEvent::ToolCallArgsDelta { index, args_delta } => {
                let entry = self
                    .tool_args_buffer
                    .get_mut(&index)
                    .ok_or(StreamViolation::ArgsWithoutStart { index })?;
                entry.args.push_str(&args_delta);
            }
            ChatEvent::ToolCallEnd {
                index,
                id,
                parsed_args,
                thought_signature,
            } => {
                let Some(started) = self.tool_args_buffer.remove(&index) else {
                    return Err(StreamViolation::EndWithoutStart { index, id });
                };
                // A start that carried no id (the id came only with the end) is not a mismatch.
                if !started.id.is_empty() && started.id != id {
                    return Err(StreamViolation::EndIdMismatch {
                        index,
                        start_id: started.id,
                        end_id: id,
                    });
                }
                self.tool_calls.push(
                    ToolCall::new(id, started.name, parsed_args)
                        .with_thought_signature(thought_signature),
                );
            }
            ChatEvent::UsageProgress { usage } => {
                // `Finished` carries the authoritative figure; until then keep the high-water mark.
                self.usage = Some(self.usage.map_or(usage, |u| u.max_each(usage)));
            }
            // A measurement is the call's, not the response's; a caller that wants it folds it
            // apart.
            ChatEvent::Measured { .. } => {}
            ChatEvent::Validated { summary } => self.validation_summary = summary,
            ChatEvent::Finished { stop_reason, usage } => {
                self.stop_reason = Some(stop_reason);
                if usage.is_some() {
                    self.usage = usage;
                }
            }
        }
        Ok(())
    }

    /// Returns the response, stamped with the current time.
    ///
    /// # Errors
    ///
    /// Returns [`Unfinished`] when no `Finished` came, or when tool calls started and never
    /// ended.
    pub fn finish(self) -> Result<ChatResponse, Unfinished> {
        let Some(stop_reason) = self.stop_reason else {
            return Err(Unfinished::NoStopReason);
        };
        if !self.tool_args_buffer.is_empty() {
            let mut indices: Vec<usize> = self.tool_args_buffer.into_keys().collect();
            indices.sort_unstable();
            return Err(Unfinished::OpenToolCalls { indices });
        }
        Ok(ChatResponse {
            actual_model: self.actual_model,
            text: self.text,
            thinking: self.thinking,
            tool_calls: self.tool_calls,
            stop_reason,
            usage: self.usage,
            cache_hit: self.cache_hit,
            degraded: self.degraded,
            validation_summary: self.validation_summary,
            created: now_unix_secs(),
        })
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
            usage: Some(Usage::default()),
        };
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::Degraded {
            unsent: vec![Degradation::StructuredOutputNotEnforced],
        })
        .unwrap();
        b.apply(ChatEvent::started("m")).unwrap();
        b.apply(finished.clone()).unwrap();
        assert_eq!(
            b.finish().unwrap().degraded,
            [Degradation::StructuredOutputNotEnforced]
        );

        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::started("m")).unwrap();
        b.apply(finished).unwrap();
        assert!(b.finish().unwrap().degraded.is_empty());
    }

    #[test]
    fn builder_concatenates_deltas() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::started("gpt-4o")).unwrap();
        b.apply(ChatEvent::Delta {
            text: "Hello, ".into(),
        })
        .unwrap();
        b.apply(ChatEvent::Delta {
            text: "world!".into(),
        })
        .unwrap();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::default()),
        })
        .unwrap();
        let r = b.finish().unwrap();
        assert_eq!(r.text, "Hello, world!");
    }

    #[test]
    fn into_events_round_trips_through_the_builder() {
        let original = ChatResponse {
            actual_model: Some("gpt-4o".into()),
            text: "hello world".into(),
            thinking: "ponder".into(),
            tool_calls: vec![ToolCall::new(
                "call_1",
                "search",
                args(json!({"q": "rust"})),
            )],
            stop_reason: StopReason::ToolUse,
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 4,
                ..Default::default()
            }),
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
            b.apply(ev).unwrap();
        }
        let back = b.finish().unwrap();
        assert_eq!(back.text, original.text);
        assert_eq!(back.thinking, original.thinking);
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].arguments, args(json!({"q": "rust"})));
        assert_eq!(back.stop_reason, original.stop_reason);
        assert_eq!(back.usage.unwrap().input_tokens, 10);
        assert_eq!(back.cache_hit.cached_input_tokens, 8);
    }

    #[test]
    fn into_events_skips_empty_text_and_thinking_blocks() {
        let r = ChatResponse {
            actual_model: Some("m".into()),
            text: String::new(),
            thinking: String::new(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::default()),
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
    fn a_stream_that_never_says_why_it_stopped_is_unfinished() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::started("m")).unwrap();
        b.apply(ChatEvent::Delta {
            text: "partial".into(),
        })
        .unwrap();
        assert_eq!(b.finish().unwrap_err(), Unfinished::NoStopReason);
    }

    #[test]
    fn a_finished_stream_with_a_tool_call_left_open_is_unfinished() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::ToolCallStart {
            index: 2,
            id: "c".into(),
            name: "t".into(),
        })
        .unwrap();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: None,
        })
        .unwrap();
        assert_eq!(
            b.finish().unwrap_err(),
            Unfinished::OpenToolCalls { indices: vec![2] }
        );
    }

    #[test]
    fn each_tool_call_protocol_violation_is_an_error() {
        let start = |index, id: &str| ChatEvent::ToolCallStart {
            index,
            id: id.into(),
            name: "t".into(),
        };
        let end = |index, id: &str| ChatEvent::ToolCallEnd {
            index,
            id: id.into(),
            parsed_args: serde_json::Map::new(),
            thought_signature: None,
        };

        let mut b = ChatResponseBuilder::new();
        b.apply(start(0, "a")).unwrap();
        assert_eq!(
            b.apply(start(0, "b")),
            Err(StreamViolation::SecondStart {
                index: 0,
                id: "b".into()
            })
        );
        assert_eq!(
            ChatResponseBuilder::new().apply(ChatEvent::ToolCallArgsDelta {
                index: 1,
                args_delta: "{".into()
            }),
            Err(StreamViolation::ArgsWithoutStart { index: 1 })
        );
        assert_eq!(
            ChatResponseBuilder::new().apply(end(1, "x")),
            Err(StreamViolation::EndWithoutStart {
                index: 1,
                id: "x".into()
            })
        );
        assert_eq!(
            b.apply(end(0, "z")),
            Err(StreamViolation::EndIdMismatch {
                index: 0,
                start_id: "a".into(),
                end_id: "z".into()
            })
        );
    }

    #[test]
    fn a_start_without_an_id_takes_the_id_its_end_brings() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::ToolCallStart {
            index: 0,
            id: String::new(),
            name: "t".into(),
        })
        .unwrap();
        b.apply(ChatEvent::ToolCallEnd {
            index: 0,
            id: "late".into(),
            parsed_args: serde_json::Map::new(),
            thought_signature: None,
        })
        .unwrap();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: None,
        })
        .unwrap();
        assert_eq!(b.finish().unwrap().tool_calls[0].id, "late");
    }

    #[test]
    fn usage_the_provider_did_not_report_is_none_not_zero() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: None,
        })
        .unwrap();
        assert!(b.finish().unwrap().usage.is_none());

        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::UsageProgress {
            usage: Usage {
                input_tokens: 7,
                ..Default::default()
            },
        })
        .unwrap();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: None,
        })
        .unwrap();
        assert_eq!(b.finish().unwrap().usage.map(|u| u.input_tokens), Some(7));
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
        b.apply(report(100, 80)).unwrap();
        b.apply(report(90, 0)).unwrap();
        let progress = b.usage.unwrap();
        assert_eq!(
            (progress.input_tokens, progress.cached_input_tokens),
            (100, 80)
        );
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage {
                input_tokens: 50,
                ..Default::default()
            }),
        })
        .unwrap();
        let done = b.finish().unwrap().usage.unwrap();
        assert_eq!((done.input_tokens, done.cached_input_tokens), (50, 0));
    }

    #[test]
    fn a_stream_that_never_named_its_model_has_no_model() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: None,
        })
        .unwrap();
        assert_eq!(b.finish().unwrap().actual_model, None);
    }

    #[test]
    fn finish_stamps_created_so_every_response_self_reports_when_it_answered() {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::Started {
            actual_model: Some("m".into()),
            cache_hit: CacheHitInfo::default(),
        })
        .unwrap();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::default()),
        })
        .unwrap();
        let r = b.finish().unwrap();
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
        })
        .unwrap();
        b.apply(ChatEvent::ToolCallArgsDelta {
            index: 0,
            args_delta: "{\"q\":\"rust".into(),
        })
        .unwrap();
        b.apply(ChatEvent::ToolCallArgsDelta {
            index: 0,
            args_delta: "\"}".into(),
        })
        .unwrap();
        b.apply(ChatEvent::ToolCallEnd {
            index: 0,
            id: "c1".into(),
            parsed_args: args(json!({"q": "rust"})),
            thought_signature: None,
        })
        .unwrap();
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Some(Usage::default()),
        })
        .unwrap();
        let r = b.finish().unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "search");
        assert_eq!(r.tool_calls[0].arguments, args(json!({"q": "rust"})));
    }
}
