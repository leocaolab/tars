//! Pure helpers shared by [`super::adapter`]: stop-reason mapping,
//! usage parsing, body truncation, URL encoding. Stateless, no I/O —
//! the kind of "mechanical conversion" that the L5 Tribunal split out
//! of the original god-module so the adapter can be read without
//! scrolling through token-level minutiae.

use serde_json::Value;

use tars_types::{StopReason, Usage};

/// Map Gemini's `finishReason` wire string to the canonical
/// [`StopReason`]. The cross-provider conformance suite relies on
/// these mappings — keep in sync with `map_stop_reasons` in
/// [`super::adapter`]'s tests.
pub(super) fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" => StopReason::ContentFilter,
        "FINISH_REASON_UNSPECIFIED" | "OTHER" => StopReason::Other,
        _ => StopReason::Other,
    }
}

/// Parse Gemini's `usageMetadata` object into the canonical [`Usage`]
/// shape. `cache_creation_tokens` stays 0 — Gemini bills cache
/// creation via the separate `cachedContents` API, not inline.
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

/// UTF-8-safe truncation; appends an ellipsis if anything was dropped.
/// Re-exported from the shared HTTP base so both backends share one copy.
pub(super) use crate::http_base::truncate;

/// Minimal URL-encode. We control the input (resolved API key), so a
/// correct-by-construction subset suffices.
pub(super) fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
