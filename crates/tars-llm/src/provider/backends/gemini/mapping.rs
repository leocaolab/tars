//! Reads Gemini's stop reasons and usage.

use serde_json::Value;

use tars_types::{StopReason, Usage};

/// Returns the stop reason of a Gemini `finishReason`; an unknown one is `Other`.
pub(super) fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" => StopReason::ContentFilter,
        "FINISH_REASON_UNSPECIFIED" | "OTHER" => StopReason::Other,
        _ => StopReason::Other,
    }
}

/// Returns the usage of a `usageMetadata` object. Cache creation is always 0: Gemini bills it
/// through the separate `cachedContents` API.
pub(super) fn parse_usage(u: &serde_json::Map<String, Value>) -> Usage {
    let prompt = u
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let candidates = u
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = u
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let thoughts = u
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Usage {
        input_tokens: prompt,
        output_tokens: candidates,
        cached_input_tokens: cached,
        cache_creation_tokens: 0,
        thinking_tokens: thoughts,
    }
}

pub(super) use crate::provider::http_base::truncate;
