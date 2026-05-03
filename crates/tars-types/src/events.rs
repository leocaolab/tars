//! Streaming events. The Provider exposes a `Stream<Item = ChatEvent>`
//! to the layer above — one event per "thing the provider told us".

use serde::{Deserialize, Serialize};

use crate::cache::CacheHitInfo;
use crate::usage::Usage;

pub type ChatChunk = ChatEvent; // legacy alias; some tests prefer this name

/// Streaming event from a Provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// First event — adapter has selected a model and (maybe) hit cache.
    Started {
        actual_model: String,
        cache_hit: CacheHitInfo,
    },

    /// Streaming text increment.
    Delta { text: String },

    /// "Thinking" / reasoning increment (Anthropic thinking, o1 reasoning).
    /// Distinct so callers can choose to display / hide / discard.
    ThinkingDelta { text: String },

    /// A tool call has begun. `index` is the parallel-call slot
    /// (0-based) — multiple calls can interleave.
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },

    /// Streaming arguments fragment. **Do not** attempt to parse mid-stream;
    /// accumulate by `index` and parse on `ToolCallEnd`.
    ToolCallArgsDelta { index: usize, args_delta: String },

    /// Tool call complete; `parsed_args` is guaranteed to be valid JSON.
    ToolCallEnd {
        index: usize,
        id: String,
        parsed_args: serde_json::Value,
    },

    /// Mid-stream usage snapshot (some providers send these periodically).
    UsageProgress { partial: PartialUsage },

    /// Terminal event. Always emitted exactly once for a successful stream.
    Finished {
        stop_reason: StopReason,
        usage: Usage,
    },
}

/// Why the model stopped emitting tokens.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model finished its turn naturally.
    EndTurn,
    /// Hit `max_output_tokens`. Caller may continue with another turn.
    MaxTokens,
    /// Hit a `stop_sequences` entry.
    StopSequence,
    /// Model wants the caller to execute tools, then continue.
    ToolUse,
    /// Provider safety filter triggered.
    ContentFilter,
    /// Caller-initiated cancel propagated through the chain.
    Cancelled,
    /// Anything else; check provider-specific logs.
    Other,
}

/// Mid-stream usage report — fields are best-effort from the provider.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct PartialUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl ChatEvent {
    /// Convenience for the Started event with no cache info.
    pub fn started(actual_model: impl Into<String>) -> Self {
        Self::Started {
            actual_model: actual_model.into(),
            cache_hit: CacheHitInfo::default(),
        }
    }

    /// True iff this is the terminal event.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
