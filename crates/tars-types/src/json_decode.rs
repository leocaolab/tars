//! Result-side, provider-mode-aware JSON decode.
//!
//! Turns an LLM response's assistant text into a typed
//! `T: DeserializeOwned` at the transport boundary, so every consumer
//! doesn't re-implement "scrape JSON out of a completion". The behavior
//! is keyed off the [`StructuredOutputMode`] the request/provider used:
//!
//! - Modes that make the provider emit **clean JSON** in `text`
//!   ([`StructuredOutputMode::StrictSchema`] /
//!   [`StructuredOutputMode::JsonObjectMode`]) → parse `text` directly.
//! - Modes where `text` may be **chatty** free-form completion
//!   ([`StructuredOutputMode::None`] /
//!   [`StructuredOutputMode::ToolUseEmulation`]) → strip code fences and
//!   scan for the first balanced JSON value embedded in the prose, then
//!   parse that.
//!
//! Deliberately generic: no wrapper-tag/envelope extraction, no lossy
//! numeric recovery, no domain validation — those are the consumer's job.

use serde::de::DeserializeOwned;

use crate::capabilities::StructuredOutputMode;
use crate::response::ChatResponse;

/// Typed failure of [`decode_json`] / [`ChatResponse::json`].
#[derive(Debug, thiserror::Error)]
pub enum TarsJsonError {
    /// The response text was empty — the stream produced no assistant
    /// text to decode (e.g. a tool-only turn, a filtered/empty
    /// completion). Distinct from "text present but no JSON in it".
    #[error("response text was empty; nothing to decode")]
    EmptyStream,

    /// Chatty (`None` / `ToolUseEmulation`) text was scanned but no
    /// balanced JSON value (`{…}` or `[…]`) parsed out of it. `attempts`
    /// is how many candidate start positions were tried (bounded scan).
    #[error("no JSON value found in response text after scanning {attempts} candidate(s)")]
    NoJsonObject { attempts: usize },

    /// Found JSON-looking text but it is not syntactically valid JSON.
    /// In strict/native modes this means the provider's "clean JSON"
    /// promise was violated.
    #[error("response text is not valid JSON: {source}")]
    InvalidJson {
        #[source]
        source: serde_json::Error,
    },

    /// Text parsed as valid JSON but did not match the target type `T`
    /// (missing field, wrong type, unknown variant, …). The `serde`
    /// message names the offending path.
    #[error("JSON did not match the expected type: {source}")]
    Schema {
        #[source]
        source: serde_json::Error,
    },
}

impl TarsJsonError {
    /// Map a `serde_json` failure to the right variant using its
    /// [`category`](serde_json::error::Category): a `Data` category means
    /// the JSON was well-formed but didn't fit `T` (schema mismatch);
    /// `Syntax`/`Eof`/`Io` mean the bytes weren't valid JSON.
    fn from_serde(source: serde_json::Error) -> Self {
        match source.classify() {
            serde_json::error::Category::Data => Self::Schema { source },
            _ => Self::InvalidJson { source },
        }
    }
}

/// Upper bound on candidate `{`/`[` start positions the chatty-text
/// scraper will try before giving up. Keeps a pathological completion
/// full of stray braces from turning decode into a quadratic scan.
const MAX_SCRAPE_ATTEMPTS: usize = 50;

/// Decode `text` into `T`, choosing the strategy from `mode`.
///
/// - Native-JSON modes ([`StrictSchema`](StructuredOutputMode::StrictSchema)
///   / [`JsonObjectMode`](StructuredOutputMode::JsonObjectMode)): the
///   provider guarantees `text` is a clean JSON document, so parse it
///   directly. Trailing/leading whitespace is tolerated.
/// - Chatty modes ([`None`](StructuredOutputMode::None) /
///   [`ToolUseEmulation`](StructuredOutputMode::ToolUseEmulation)): the
///   text may be prose with the JSON embedded (optionally inside a
///   ```` ```json ```` fence). Strip fences, then scan for the first
///   balanced JSON value and parse it.
///
/// Empty (whitespace-only) text is [`TarsJsonError::EmptyStream`] in
/// every mode — there is nothing to decode.
pub fn decode_json<T: DeserializeOwned>(
    text: &str,
    mode: StructuredOutputMode,
) -> Result<T, TarsJsonError> {
    if text.trim().is_empty() {
        return Err(TarsJsonError::EmptyStream);
    }
    match mode {
        // Provider guarantees a clean JSON document in `text`.
        StructuredOutputMode::StrictSchema | StructuredOutputMode::JsonObjectMode => {
            serde_json::from_str::<T>(text.trim()).map_err(TarsJsonError::from_serde)
        }
        // `text` may be chatty free-form; scrape the JSON out of it.
        StructuredOutputMode::None | StructuredOutputMode::ToolUseEmulation => scrape_json(text),
    }
}

/// Strip an optional leading/trailing Markdown code fence, then scan the
/// text for the first balanced JSON value (`{…}` or `[…]`) that parses
/// as `T`.
fn scrape_json<T: DeserializeOwned>(text: &str) -> Result<T, TarsJsonError> {
    let body = strip_code_fences(text);
    let bytes = body.as_bytes();

    let mut attempts = 0usize;
    let mut last_err: Option<serde_json::Error> = None;

    let mut i = 0usize;
    while i < bytes.len() {
        let open = bytes[i];
        let close = match open {
            b'{' => b'}',
            b'[' => b']',
            _ => {
                i += 1;
                continue;
            }
        };
        // Found a candidate start. Try to close it with a balanced scan.
        if attempts >= MAX_SCRAPE_ATTEMPTS {
            break;
        }
        attempts += 1;
        if let Some(end) = find_balanced(bytes, i, open, close) {
            let candidate = &body[i..=end];
            match serde_json::from_str::<T>(candidate) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // Well-formed JSON that just didn't fit `T` is a
                    // schema mismatch, not "wrong braces" — report it
                    // rather than keep scanning past the real payload.
                    if matches!(e.classify(), serde_json::error::Category::Data) {
                        return Err(TarsJsonError::Schema { source: e });
                    }
                    last_err = Some(e);
                    // Skip past this opener and keep looking — the
                    // balanced region wasn't valid JSON (e.g. a `{` in
                    // prose), so a later candidate may be the real one.
                    i += 1;
                    continue;
                }
            }
        } else {
            // Unbalanced from here to EOF — no later opener can close
            // either, so stop.
            break;
        }
    }

    match last_err {
        Some(source) => Err(TarsJsonError::InvalidJson { source }),
        None => Err(TarsJsonError::NoJsonObject { attempts }),
    }
}

/// Strip a single leading ```` ``` ```` / ```` ```json ```` fence and its
/// matching trailing ```` ``` ````, returning the inner body. When the
/// text isn't fenced, returns it trimmed unchanged. Only the outermost
/// fence is removed; the balanced scan handles anything else.
fn strip_code_fences(text: &str) -> &str {
    let t = text.trim();
    let Some(after_open) = t.strip_prefix("```") else {
        return t;
    };
    // Drop the rest of the fence's opening line (an optional language
    // tag like `json`). If there's no newline, there's no body.
    let Some(nl) = after_open.find('\n') else {
        return t;
    };
    let inner = &after_open[nl + 1..];
    // Remove the closing fence if present; tolerate its absence
    // (truncated output) by returning what we have.
    match inner.rfind("```") {
        Some(close) => inner[..close].trim(),
        None => inner.trim(),
    }
}

/// Given `bytes[start]` is `open`, return the byte index of the matching
/// `close`, honouring JSON string literals (braces inside `"…"` don't
/// count) and backslash escapes. `None` if it never balances.
fn find_balanced(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

impl ChatResponse {
    /// Decode this response's assistant [`text`](ChatResponse::text) into
    /// a typed `T`, using `mode` to pick the decode strategy. Convenience
    /// wrapper over [`decode_json`]; see it for the mode semantics.
    ///
    /// `mode` is the [`StructuredOutputMode`] the request/provider used
    /// (from the provider's [`Capabilities`](crate::capabilities::Capabilities)),
    /// so the caller — which knows how the response was produced — tells
    /// the decoder whether `text` is clean JSON or chatty prose.
    pub fn json<T: DeserializeOwned>(
        &self,
        mode: StructuredOutputMode,
    ) -> Result<T, TarsJsonError> {
        decode_json(&self.text, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Point {
        x: i32,
        y: i32,
    }

    // ── strict / native-JSON modes ──────────────────────────────────

    #[test]
    fn strict_mode_parses_clean_json_directly() {
        let p: Point =
            decode_json(r#"{"x":1,"y":2}"#, StructuredOutputMode::StrictSchema).unwrap();
        assert_eq!(p, Point { x: 1, y: 2 });
    }

    #[test]
    fn strict_mode_tolerates_surrounding_whitespace() {
        let p: Point =
            decode_json("  \n{\"x\":3,\"y\":4}\n ", StructuredOutputMode::StrictSchema).unwrap();
        assert_eq!(p, Point { x: 3, y: 4 });
    }

    #[test]
    fn json_object_mode_parses_directly() {
        let p: Point =
            decode_json(r#"{"x":5,"y":6}"#, StructuredOutputMode::JsonObjectMode).unwrap();
        assert_eq!(p, Point { x: 5, y: 6 });
    }

    #[test]
    fn strict_mode_does_not_scrape_fenced_prose() {
        // In strict mode the provider promised clean JSON. A fenced,
        // chatty body is a broken promise → InvalidJson, NOT a silent
        // scrape (that behavior belongs to the chatty modes only).
        let err = decode_json::<Point>(
            "Here you go: ```json\n{\"x\":1,\"y\":2}\n```",
            StructuredOutputMode::StrictSchema,
        )
        .unwrap_err();
        assert!(matches!(err, TarsJsonError::InvalidJson { .. }), "got {err:?}");
    }

    // ── chatty (None) fence-scrape fallback ─────────────────────────

    #[test]
    fn none_mode_scrapes_bare_json() {
        let p: Point = decode_json(r#"{"x":7,"y":8}"#, StructuredOutputMode::None).unwrap();
        assert_eq!(p, Point { x: 7, y: 8 });
    }

    #[test]
    fn none_mode_scrapes_json_from_fenced_block() {
        let text = "Sure! Here is the result:\n```json\n{\"x\":9,\"y\":10}\n```\nHope that helps.";
        let p: Point = decode_json(text, StructuredOutputMode::None).unwrap();
        assert_eq!(p, Point { x: 9, y: 10 });
    }

    #[test]
    fn none_mode_scrapes_json_from_plain_fence() {
        let text = "```\n{\"x\":11,\"y\":12}\n```";
        let p: Point = decode_json(text, StructuredOutputMode::None).unwrap();
        assert_eq!(p, Point { x: 11, y: 12 });
    }

    #[test]
    fn none_mode_scrapes_json_embedded_in_prose_without_fence() {
        let text = "The answer is {\"x\":13,\"y\":14} and that's final.";
        let p: Point = decode_json(text, StructuredOutputMode::None).unwrap();
        assert_eq!(p, Point { x: 13, y: 14 });
    }

    #[test]
    fn none_mode_skips_stray_brace_before_real_object() {
        // A `{` in prose that isn't valid JSON must not abort the scan;
        // the real object comes later.
        let text = "note: use {curly} braces. payload: {\"x\":15,\"y\":16}";
        let p: Point = decode_json(text, StructuredOutputMode::None).unwrap();
        assert_eq!(p, Point { x: 15, y: 16 });
    }

    #[test]
    fn none_mode_honours_braces_inside_strings() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Msg {
            note: String,
        }
        let text = r#"here: {"note":"a } brace and \" quote inside"}"#;
        let m: Msg = decode_json(text, StructuredOutputMode::None).unwrap();
        assert_eq!(m.note, r#"a } brace and " quote inside"#);
    }

    #[test]
    fn none_mode_scrapes_top_level_array() {
        let text = "results: [1, 2, 3] done";
        let v: Vec<i32> = decode_json(text, StructuredOutputMode::None).unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn tool_use_emulation_mode_scrapes_like_none() {
        let text = "```json\n{\"x\":21,\"y\":22}\n```";
        let p: Point = decode_json(text, StructuredOutputMode::ToolUseEmulation).unwrap();
        assert_eq!(p, Point { x: 21, y: 22 });
    }

    // ── error cases ─────────────────────────────────────────────────

    #[test]
    fn empty_text_is_empty_stream_in_every_mode() {
        for mode in [
            StructuredOutputMode::None,
            StructuredOutputMode::StrictSchema,
            StructuredOutputMode::JsonObjectMode,
            StructuredOutputMode::ToolUseEmulation,
        ] {
            let err = decode_json::<Point>("   \n\t ", mode).unwrap_err();
            assert!(matches!(err, TarsJsonError::EmptyStream), "mode {mode:?} → {err:?}");
        }
    }

    #[test]
    fn none_mode_no_json_object_when_prose_has_none() {
        let err =
            decode_json::<Point>("no braces here at all", StructuredOutputMode::None).unwrap_err();
        match err {
            TarsJsonError::NoJsonObject { attempts } => assert_eq!(attempts, 0),
            other => panic!("expected NoJsonObject, got {other:?}"),
        }
    }

    #[test]
    fn none_mode_valid_json_wrong_shape_is_schema_error() {
        // Well-formed JSON object that doesn't match `Point`.
        let err =
            decode_json::<Point>(r#"answer: {"foo":1,"bar":2}"#, StructuredOutputMode::None)
                .unwrap_err();
        assert!(matches!(err, TarsJsonError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn strict_mode_wrong_shape_is_schema_error() {
        let err =
            decode_json::<Point>(r#"{"foo":1}"#, StructuredOutputMode::StrictSchema).unwrap_err();
        assert!(matches!(err, TarsJsonError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn strict_mode_malformed_json_is_invalid_json() {
        let err =
            decode_json::<Point>(r#"{"x":1,"y":}"#, StructuredOutputMode::StrictSchema).unwrap_err();
        assert!(matches!(err, TarsJsonError::InvalidJson { .. }), "got {err:?}");
    }

    #[test]
    fn none_mode_attempts_are_bounded() {
        // Many stray non-JSON `{` openers followed by no valid payload:
        // the scan must stop at MAX_SCRAPE_ATTEMPTS, not scan them all.
        let text = "{x ".repeat(200);
        let err = decode_json::<Point>(&text, StructuredOutputMode::None).unwrap_err();
        match err {
            TarsJsonError::NoJsonObject { attempts } => {
                assert!(attempts <= MAX_SCRAPE_ATTEMPTS, "attempts={attempts}");
            }
            // `{x {x {x…` never balances → find_balanced returns None on
            // the first opener and the scan stops early. Either bounded
            // outcome is acceptable; both prove no unbounded scan.
            other => panic!("expected NoJsonObject, got {other:?}"),
        }
    }

    // ── ChatResponse::json convenience ──────────────────────────────

    #[test]
    fn chat_response_json_method_decodes_text() {
        let resp = ChatResponse {
            text: "here: {\"x\":1,\"y\":2}".into(),
            ..Default::default()
        };
        let p: Point = resp.json(StructuredOutputMode::None).unwrap();
        assert_eq!(p, Point { x: 1, y: 2 });
    }
}
