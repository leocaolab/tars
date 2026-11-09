//! Defines the events a provider's response stream yields, one per thing the provider reported.

use serde::{Deserialize, Serialize};

use crate::providers::cache::CacheHitInfo;
use crate::providers::usage::Usage;
use crate::telemetry::CallTelemetry;
use crate::validation::ValidationSummary;

/// One event of a provider's response stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// What the provider serves the request without, as its profile says. Only the dispatch
    /// check in `LlmService::call` sends it, first and only when it names something; it is added
    /// outside every middleware layer, so no layer sees it.
    Degraded {
        unsent: Vec<crate::providers::chat::Degradation>,
    },

    /// The provider's first event: the model the adapter called, and what it found in the cache.
    Started {
        actual_model: String,
        cache_hit: CacheHitInfo,
    },

    Delta {
        text: String,
    },

    /// A reasoning increment (Anthropic thinking, o1 reasoning), apart from `Delta` so a caller
    /// can show, hide or drop it.
    ThinkingDelta {
        text: String,
    },

    /// A tool call has begun. `index` is its 0-based parallel-call slot; several calls can
    /// interleave.
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },

    /// A fragment of a tool call's arguments, not parseable on its own; accumulate by `index` and
    /// read the parsed arguments from `ToolCallEnd`.
    ToolCallArgsDelta {
        index: usize,
        args_delta: String,
    },

    /// A tool call is complete, its arguments parsed.
    ToolCallEnd {
        index: usize,
        id: String,
        #[serde(deserialize_with = "crate::providers::tools::object_arguments")]
        parsed_args: serde_json::Map<String, serde_json::Value>,
        /// Opaque provider token to echo on replay (Gemini `thoughtSignature`);
        /// `None` for providers that don't use it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },

    /// Usage the provider reported apart from a stop reason: a snapshot mid-stream, or the last
    /// report of a stream that never said why the turn stopped. A consumer keeps each figure's
    /// highest; a later `Finished` replaces them.
    UsageProgress {
        usage: Usage,
    },

    /// One middleware layer's measurement of this call, sent before the stream's terminal event
    /// (or its error) so a consumer that stops at `Finished` has it. A provider never sends it;
    /// a consumer folds every one with [`CallTelemetry::merge`].
    Measured {
        telemetry: CallTelemetry,
    },

    /// What the validation layer's validators did, sent once before `Finished` when the chain has
    /// a validation layer. A provider never sends it.
    Validated {
        summary: ValidationSummary,
    },

    /// The terminal event, emitted exactly once by a successful stream.
    Finished {
        stop_reason: StopReason,
        usage: Usage,
    },
}

/// Why the model stopped emitting tokens.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    /// The output reached `max_output_tokens`; the caller may continue with another turn.
    MaxTokens,
    StopSequence,
    /// The model wants the caller to run tools and then continue.
    ToolUse,
    ContentFilter,
    /// The caller cancelled.
    Cancelled,
    /// Any other reason the provider gave.
    Other,
}

impl StopReason {
    /// Returns the snake_case word serde writes, for a log field or a span attribute.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::StopSequence => "stop_sequence",
            Self::ToolUse => "tool_use",
            Self::ContentFilter => "content_filter",
            Self::Cancelled => "cancelled",
            Self::Other => "other",
        }
    }
}

impl ChatEvent {
    /// Returns a `Started` event with no cache hit.
    pub fn started(actual_model: impl Into<String>) -> Self {
        Self::Started {
            actual_model: actual_model.into(),
            cache_hit: CacheHitInfo::default(),
        }
    }

    /// Returns the last event of a stream that said `stop_reason` and reported `usage`:
    /// `Finished` when it said why the turn stopped (with zero usage when it reported none),
    /// `UsageProgress` when it only reported usage, so the tokens spent are not lost, and none
    /// when it said neither.
    pub fn stream_end(stop_reason: Option<StopReason>, usage: Option<Usage>) -> Option<Self> {
        match (stop_reason, usage) {
            (Some(stop_reason), usage) => Some(Self::Finished {
                stop_reason,
                usage: usage.unwrap_or_default(),
            }),
            (None, Some(usage)) => Some(Self::UsageProgress { usage }),
            (None, None) => None,
        }
    }

    /// Returns whether this is the terminal event.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }

    /// Returns what the stream has spent once this event has passed, given `spent` before it,
    /// `None` meaning no usage reported yet. `Finished` states the call's usage and replaces every
    /// earlier report; `UsageProgress` raises each figure to the highest reported, since reports
    /// are snapshots of one response, not increments. A stream that ends in an error has spent
    /// what its reports reached by then.
    pub fn spent_after(&self, spent: Option<Usage>) -> Option<Usage> {
        match self {
            Self::Finished { usage, .. } => Some(*usage),
            Self::UsageProgress { usage } => Some(spent.map_or(*usage, |s| s.max_each(*usage))),
            _ => spent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_reason_word_is_the_word_serde_writes() {
        use StopReason::*;
        for reason in [
            EndTurn,
            MaxTokens,
            StopSequence,
            ToolUse,
            ContentFilter,
            Cancelled,
            Other,
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::from(reason.as_str())
            );
        }
    }

    #[test]
    fn a_recorded_tool_call_end_whose_arguments_are_not_an_object_does_not_load() {
        let end = |args: serde_json::Value| serde_json::json!({"type": "tool_call_end", "index": 0, "id": "c", "parsed_args": args});
        assert!(serde_json::from_value::<ChatEvent>(end(serde_json::json!({"q": 1}))).is_ok());
        for bad in [serde_json::json!("{\"q\":1}"), serde_json::json!([1])] {
            assert!(
                serde_json::from_value::<ChatEvent>(end(bad.clone())).is_err(),
                "{bad}"
            );
        }
    }

    fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
        Usage {
            input_tokens,
            output_tokens,
            ..Usage::default()
        }
    }

    fn progress(input_tokens: u64, output_tokens: u64) -> ChatEvent {
        ChatEvent::UsageProgress {
            usage: usage(input_tokens, output_tokens),
        }
    }

    fn finished(input_tokens: u64, output_tokens: u64) -> ChatEvent {
        ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: usage(input_tokens, output_tokens),
        }
    }

    fn spent(events: &[ChatEvent]) -> Option<(u64, u64)> {
        events
            .iter()
            .fold(None, |spent, ev| ev.spent_after(spent))
            .map(|u| (u.input_tokens, u.output_tokens))
    }

    #[test]
    fn a_stream_with_no_usage_report_has_spent_nothing_known() {
        assert_eq!(
            spent(&[
                ChatEvent::started("m"),
                ChatEvent::Delta { text: "x".into() }
            ]),
            None
        );
    }

    #[test]
    fn a_stream_with_only_progress_has_spent_each_figures_highest() {
        assert_eq!(spent(&[progress(100, 0), progress(90, 7)]), Some((100, 7)));
    }

    #[test]
    fn a_stream_with_only_finished_has_spent_what_finished_states() {
        assert_eq!(spent(&[finished(100, 7)]), Some((100, 7)));
    }

    #[test]
    fn finished_replaces_progress_even_when_it_reports_less() {
        assert_eq!(spent(&[progress(100, 9), finished(80, 7)]), Some((80, 7)));
    }

    #[test]
    fn events_after_a_report_keep_what_was_spent() {
        assert_eq!(
            spent(&[progress(100, 7), ChatEvent::Delta { text: "x".into() }]),
            Some((100, 7))
        );
    }

    #[test]
    fn finished_is_terminal() {
        let e = ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        };
        assert!(e.is_terminal());
    }

    #[test]
    fn delta_is_not_terminal() {
        assert!(!ChatEvent::Delta { text: "x".into() }.is_terminal());
    }

    #[test]
    fn every_variant_round_trips_through_json() {
        let cases: Vec<ChatEvent> = vec![
            ChatEvent::Degraded {
                unsent: vec![crate::providers::chat::Degradation::ChatTemplateThinkingNotSent],
            },
            ChatEvent::started("gpt-4o"),
            ChatEvent::Delta { text: "hi".into() },
            ChatEvent::ThinkingDelta { text: "...".into() },
            ChatEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "search".into(),
            },
            ChatEvent::ToolCallArgsDelta {
                index: 0,
                args_delta: "{\"q\":".into(),
            },
            ChatEvent::ToolCallEnd {
                index: 0,
                id: "call_1".into(),
                parsed_args: serde_json::Map::from_iter([(
                    "q".to_string(),
                    serde_json::json!("rust"),
                )]),
                thought_signature: None,
            },
            ChatEvent::UsageProgress {
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    ..Usage::default()
                },
            },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ];
        for original in cases {
            let v = serde_json::to_value(&original).unwrap();
            assert!(v.get("type").is_some(), "missing type tag: {v}");
            let back: ChatEvent = serde_json::from_value(v.clone()).unwrap();
            // ChatEvent has no PartialEq, so the round trip compares JSON.
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                v,
                "round-trip mismatch for {v}",
            );
        }
    }

    #[test]
    fn type_tag_uses_snake_case() {
        let v = serde_json::to_value(ChatEvent::ToolCallStart {
            index: 0,
            id: "x".into(),
            name: "y".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "tool_call_start");
    }

    #[test]
    fn a_stream_end_is_finished_only_when_the_stream_said_why_it_stopped() {
        let spent = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Usage::default()
        };
        assert!(matches!(
            ChatEvent::stream_end(Some(StopReason::ToolUse), Some(spent)),
            Some(ChatEvent::Finished { stop_reason: StopReason::ToolUse, usage })
                if usage.output_tokens == 500
        ));
        assert!(matches!(
            ChatEvent::stream_end(Some(StopReason::EndTurn), None),
            Some(ChatEvent::Finished { usage, .. }) if usage.total_tokens() == 0
        ));
        assert!(matches!(
            ChatEvent::stream_end(None, Some(spent)),
            Some(ChatEvent::UsageProgress { usage }) if usage.input_tokens == 1000
        ));
        assert!(ChatEvent::stream_end(None, None).is_none());
    }
}
