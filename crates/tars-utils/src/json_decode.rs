//! Result-side, provider-mode-aware JSON decode — the full generic seam
//! for turning an LLM completion into a typed value at the transport
//! boundary, so no consumer re-implements "scrape JSON out of a chat
//! response".
//!
//! Three layers, each generic (no domain tag values, no domain types):
//!
//! 1. [`decode_json`] — bare decode of a response's text into
//!    `T: DeserializeOwned`, strategy keyed off [`StructuredOutputMode`].
//! 2. [`JsonAgentResponse`] + [`decode`] — the same, but the response may
//!    be wrapped in a consumer-declared envelope tag
//!    (`<tag>…</tag>`) and the caller may opt into a lossy
//!    integer-clamp recovery via [`DecodeOpts`].
//! 3. [`JsonValueType`] — a Python-style JSON type tag, handy for a
//!    consumer's own "expected X, got Y" error messages.
//!
//! Mode dispatch:
//! - [`StrictSchema`](StructuredOutputMode::StrictSchema) /
//!   [`JsonObjectMode`](StructuredOutputMode::JsonObjectMode): the
//!   provider guarantees a **clean JSON document** → parse directly.
//! - [`None`](StructuredOutputMode::None) /
//!   [`ToolUseEmulation`](StructuredOutputMode::ToolUseEmulation): the
//!   text may be **chatty prose** with JSON embedded (optionally inside a
//!   ```` ```json ```` fence) → strip fences, scan for the first balanced
//!   JSON value, parse that.
//!
//! Deliberately generic: the envelope-tag STRINGS and the clamp opt-in
//! are the consumer's convention; the extraction/recovery MECHANISM is
//! here. No wrapper-tag values, no domain validation.

use serde::de::DeserializeOwned;
use serde_json::Value;

use tars_types::ChatResponse;
use tars_types::capabilities::StructuredOutputMode;

/// Typed failure of the decode family ([`decode_json`] / [`decode`] /
/// [`ResponseJsonExt::json`]).
#[derive(Debug, thiserror::Error)]
pub enum TarsJsonError {
    /// The response text was empty — the stream produced no assistant
    /// text to decode (e.g. a tool-only turn, a filtered/empty
    /// completion). Distinct from "text present but no JSON in it".
    #[error("response text was empty; nothing to decode")]
    EmptyStream,

    /// The response declared one or more envelope [`wrapper_tags`] but
    /// none of them (`<tag>…</tag>`) was found in the text. `tried` is
    /// the tags that were looked for, in order.
    ///
    /// [`wrapper_tags`]: JsonAgentResponse::wrapper_tags
    #[error("no envelope block found; expected one of {tried:?}")]
    MissingBlock { tried: Vec<String> },

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
    /// the JSON was well-formed but didn't fit the target type (schema
    /// mismatch); `Syntax`/`Eof`/`Io` mean the bytes weren't valid JSON.
    fn from_serde(source: serde_json::Error) -> Self {
        match source.classify() {
            serde_json::error::Category::Data => Self::Schema { source },
            _ => Self::InvalidJson { source },
        }
    }
}

/// A JSON value's type, named the Python way (`NoneType` / `bool` /
/// `int` / `float` / `str` / `list` / `dict`). Generic helper for a
/// consumer that wants a stable "expected an object, got a list"-style
/// message without hand-matching [`serde_json::Value`] everywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonValueType {
    Null,
    Bool,
    Integer,
    Float,
    String,
    Array,
    Object,
}

impl JsonValueType {
    /// Classify a [`serde_json::Value`]. A number is [`Integer`] unless it
    /// was parsed as a float (`is_f64`), in which case it is [`Float`].
    ///
    /// [`Integer`]: JsonValueType::Integer
    /// [`Float`]: JsonValueType::Float
    pub fn of(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Bool,
            Value::Number(n) => {
                if n.is_f64() {
                    Self::Float
                } else {
                    Self::Integer
                }
            }
            Value::String(_) => Self::String,
            Value::Array(_) => Self::Array,
            Value::Object(_) => Self::Object,
        }
    }

    /// The Python type name for this JSON type.
    pub fn py_name(&self) -> &'static str {
        match self {
            Self::Null => "NoneType",
            Self::Bool => "bool",
            Self::Integer => "int",
            Self::Float => "float",
            Self::String => "str",
            Self::Array => "list",
            Self::Object => "dict",
        }
    }
}

impl std::fmt::Display for JsonValueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.py_name())
    }
}

/// Options for [`decode`]. Defaults are the safe, lossless choices;
/// every recovery is opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeOpts {
    /// When `true`, before the final deserialize, walk the parsed JSON and
    /// clamp any integer above `i64::MAX` down to `i64::MAX`. This is a
    /// **lossy** recovery for models that emit out-of-range integers
    /// (e.g. a bogus 20-digit id) into a field that only holds an
    /// `i64`/`i32`. Off by default — a consumer opts in per call.
    pub clamp_ints: bool,
}

impl DecodeOpts {
    /// Opts with integer clamping enabled.
    pub fn clamping() -> Self {
        Self { clamp_ints: true }
    }
}

/// A response type that may arrive wrapped in a declared envelope tag.
///
/// The generic contract behind [`decode`]: a consumer implements this for
/// its own response type and supplies the envelope [`wrapper_tags`] its
/// convention uses (e.g. an agent that wraps its JSON in
/// `<some_report>…</some_report>`). The **tag strings are the consumer's
/// convention**; the extraction mechanism in [`decode`] is generic.
///
/// The default [`wrapper_tags`] is empty → the response is bare JSON.
///
/// [`wrapper_tags`]: JsonAgentResponse::wrapper_tags
pub trait JsonAgentResponse: DeserializeOwned {
    /// Envelope tags this response may be wrapped in, tried in order,
    /// first match wins. Each entry may be written with or without
    /// angle brackets (`"<fix_report>"` and `"fix_report"` are
    /// equivalent). List a newer tag first and legacy aliases after it to
    /// accept both. Empty (the default) means bare JSON — no envelope.
    fn wrapper_tags() -> &'static [&'static str] {
        &[]
    }
}

/// Upper bound on candidate `{`/`[` start positions the chatty-text
/// scraper will try before giving up. Keeps a pathological completion
/// full of stray braces from turning decode into a quadratic scan.
const MAX_SCRAPE_ATTEMPTS: usize = 50;

/// Decode `text` into `T`, choosing the strategy from `mode`. No envelope
/// unwrapping, no clamp — the bare convenience over the full [`decode`].
///
/// - Native-JSON modes ([`StrictSchema`](StructuredOutputMode::StrictSchema)
///   / [`JsonObjectMode`](StructuredOutputMode::JsonObjectMode)): parse
///   `text` directly (whitespace tolerated).
/// - Chatty modes ([`None`](StructuredOutputMode::None) /
///   [`ToolUseEmulation`](StructuredOutputMode::ToolUseEmulation)): strip
///   fences, scan for the first balanced JSON value, parse it.
///
/// Empty (whitespace-only) text is [`TarsJsonError::EmptyStream`].
pub fn decode_json<T: DeserializeOwned>(
    text: &str,
    mode: StructuredOutputMode,
) -> Result<T, TarsJsonError> {
    let value = parse_value(text, mode)?;
    serde_json::from_value::<T>(value).map_err(TarsJsonError::from_serde)
}

/// Decode `text` into a [`JsonAgentResponse`], composing the full seam:
///
/// 1. if `T::wrapper_tags()` is non-empty, extract the substring between
///    the first matching `<tag>…</tag>` from the RAW text (→
///    [`TarsJsonError::MissingBlock`] if none match); otherwise strip an
///    outer code fence and use that,
/// 2. dispatch on `mode` (native → direct parse; chatty → fence-scrape),
/// 3. if `opts.clamp_ints`, clamp out-of-`i64`-range integers before the
///    final deserialize.
///
/// The wrapper-tag block is extracted BEFORE any fence stripping: a reply
/// that narrates a fenced ```diff block and then emits its `<tag>…</tag>`
/// envelope must have the envelope found, not discarded when
/// `strip_code_fences` consumes the leading fence. Stripping first (which a
/// refactor briefly did) drops the envelope for exactly that shape — the
/// fixer's `<fix_report>` / `<agent_reply>` after a diff — and the decode
/// fails with `MissingBlock`.
pub fn decode<T: JsonAgentResponse>(
    text: &str,
    mode: StructuredOutputMode,
    opts: DecodeOpts,
) -> Result<T, TarsJsonError> {
    let tags = T::wrapper_tags();
    let inner = if tags.is_empty() {
        strip_code_fences(text)
    } else {
        extract_tag_block(text, tags).ok_or_else(|| TarsJsonError::MissingBlock {
            tried: tags.iter().map(|t| (*t).to_string()).collect(),
        })?
    };

    let mut value = parse_value(inner, mode)?;
    if opts.clamp_ints {
        clamp_ints_in_place(&mut value);
    }
    serde_json::from_value::<T>(value).map_err(TarsJsonError::from_serde)
}

/// Parse `text` to a [`serde_json::Value`] using the `mode` strategy.
/// Shared front half of both decode entry points; the caller does the
/// final `from_value::<T>` (so a clamp can slot in between).
fn parse_value(text: &str, mode: StructuredOutputMode) -> Result<Value, TarsJsonError> {
    if text.trim().is_empty() {
        return Err(TarsJsonError::EmptyStream);
    }
    match mode {
        // Provider guarantees a clean JSON document in `text`.
        StructuredOutputMode::StrictSchema | StructuredOutputMode::JsonObjectMode => {
            serde_json::from_str::<Value>(text.trim()).map_err(TarsJsonError::from_serde)
        }
        // `text` may be chatty free-form; scrape the JSON out of it.
        StructuredOutputMode::None | StructuredOutputMode::ToolUseEmulation => scrape_value(text),
    }
}

/// Strip an optional leading/trailing Markdown code fence, then scan the
/// text for the first balanced JSON value (`{…}` or `[…]`) that parses.
fn scrape_value(text: &str) -> Result<Value, TarsJsonError> {
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
        if attempts >= MAX_SCRAPE_ATTEMPTS {
            break;
        }
        attempts += 1;
        if let Some(end) = find_balanced(bytes, i, open, close) {
            let candidate = &body[i..=end];
            match serde_json::from_str::<Value>(candidate) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // The balanced region wasn't valid JSON (e.g. a `{` in
                    // prose). Remember why and keep looking — a later
                    // candidate may be the real payload.
                    last_err = Some(e);
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

/// Extract the substring between the first matching `<tag>…</tag>` in
/// `text`. Tags are tried in order; the first whose open **and** close
/// markers both appear wins. Each `tag` may be written with or without
/// angle brackets. Returns `None` if no tag matched.
fn extract_tag_block<'a>(text: &'a str, tags: &[&str]) -> Option<&'a str> {
    for tag in tags {
        let name = tag.trim().trim_start_matches('<').trim_end_matches('>');
        if name.is_empty() {
            continue;
        }
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        if let Some(os) = text.find(&open) {
            let after = os + open.len();
            if let Some(rel) = text[after..].find(&close) {
                return Some(text[after..after + rel].trim());
            }
        }
    }
    None
}

/// Recursively clamp every integer above `i64::MAX` down to `i64::MAX`.
/// Only unsigned integers that overflow `i64` are affected; signed
/// integers, floats, and every non-number value are left untouched.
fn clamp_ints_in_place(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                if u > i64::MAX as u64 {
                    *n = serde_json::Number::from(i64::MAX);
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(clamp_ints_in_place),
        Value::Object(map) => map.values_mut().for_each(clamp_ints_in_place),
        _ => {}
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

/// Extension trait adding a `json` decode method to
/// [`tars_types::ChatResponse`].
///
/// `ChatResponse` is defined in `tars-types`, so the orphan rule forbids
/// an inherent `impl` here in `tars-utils`; the method rides in on this
/// trait instead. Bring it into scope (`use tars_utils::ResponseJsonExt;`)
/// at any `resp.json::<T>(mode)` call site.
pub trait ResponseJsonExt {
    /// Decode this response's assistant `text` into a typed `T`, using
    /// `mode` to pick the decode strategy. Convenience wrapper over
    /// [`decode_json`]; see it for the mode semantics.
    ///
    /// `mode` is the [`StructuredOutputMode`] the request/provider used
    /// (from the provider's [`Capabilities`](tars_types::capabilities::Capabilities)),
    /// so the caller — which knows how the response was produced — tells
    /// the decoder whether `text` is clean JSON or chatty prose.
    fn json<T: DeserializeOwned>(&self, mode: StructuredOutputMode) -> Result<T, TarsJsonError>;
}

impl ResponseJsonExt for ChatResponse {
    fn json<T: DeserializeOwned>(&self, mode: StructuredOutputMode) -> Result<T, TarsJsonError> {
        decode_json(&self.text, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Point {
        x: i32,
        y: i32,
    }

    // ── decode_json: strict / native-JSON modes ─────────────────────

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

    // ── decode_json: chatty (None) fence-scrape fallback ────────────

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

    // ── decode_json: error cases ────────────────────────────────────

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
        let text = "{x ".repeat(200);
        let err = decode_json::<Point>(&text, StructuredOutputMode::None).unwrap_err();
        match err {
            TarsJsonError::NoJsonObject { attempts } => {
                assert!(attempts <= MAX_SCRAPE_ATTEMPTS, "attempts={attempts}");
            }
            other => panic!("expected NoJsonObject, got {other:?}"),
        }
    }

    #[test]
    fn chat_response_json_method_decodes_text() {
        let resp = ChatResponse {
            text: "here: {\"x\":1,\"y\":2}".into(),
            ..Default::default()
        };
        let p: Point = resp.json(StructuredOutputMode::None).unwrap();
        assert_eq!(p, Point { x: 1, y: 2 });
    }

    // ── decode: wrapper-tag envelope extraction ─────────────────────

    #[derive(Debug, Deserialize, PartialEq)]
    struct Wrapped {
        x: i32,
        y: i32,
    }
    impl JsonAgentResponse for Wrapped {
        fn wrapper_tags() -> &'static [&'static str] {
            &["<report>"]
        }
    }

    #[test]
    fn decode_extracts_json_from_envelope_tag() {
        let text = "Preamble.\n<report>{\"x\":1,\"y\":2}</report>\nEpilogue.";
        let w: Wrapped = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(w, Wrapped { x: 1, y: 2 });
    }

    #[test]
    fn decode_extracts_envelope_that_follows_a_code_fence() {
        // Regression: a reply that narrates a fenced ```diff block and THEN
        // emits its `<report>…</report>` envelope must have the envelope found.
        // The wrapper block is extracted from the RAW text before any fence
        // stripping — stripping first consumes the leading fence and discards
        // the envelope that follows it (the fixer's `<fix_report>` after a diff),
        // failing with MissingBlock. This is the shape the agent fixer emits.
        let text = "```diff\n--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n```\n\
                    <report>{\"x\":1,\"y\":2}</report>";
        let w: Wrapped = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(w, Wrapped { x: 1, y: 2 });
    }

    #[test]
    fn decode_missing_envelope_tag_is_missing_block() {
        let text = "no envelope here, just {\"x\":1,\"y\":2}";
        let err =
            decode::<Wrapped>(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap_err();
        match err {
            TarsJsonError::MissingBlock { tried } => {
                assert_eq!(tried, vec!["<report>".to_string()])
            }
            other => panic!("expected MissingBlock, got {other:?}"),
        }
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Aliased {
        v: i32,
    }
    impl JsonAgentResponse for Aliased {
        // New tag first, legacy alias second — accept both.
        fn wrapper_tags() -> &'static [&'static str] {
            &["<result>", "<legacy>"]
        }
    }

    #[test]
    fn decode_first_matching_tag_wins() {
        // Both tags present → the first listed (`<result>`) wins.
        let text = "<legacy>{\"v\":1}</legacy>\n<result>{\"v\":2}</result>";
        let a: Aliased = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(a, Aliased { v: 2 });
    }

    #[test]
    fn decode_falls_back_to_legacy_alias_tag() {
        // Only the legacy alias present → still extracted.
        let text = "chatter <legacy>{\"v\":9}</legacy> chatter";
        let a: Aliased = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(a, Aliased { v: 9 });
    }

    #[test]
    fn decode_tag_written_without_brackets_is_equivalent() {
        // The trait may list "report" instead of "<report>".
        #[derive(Debug, Deserialize, PartialEq)]
        struct Bare {
            x: i32,
            y: i32,
        }
        impl JsonAgentResponse for Bare {
            fn wrapper_tags() -> &'static [&'static str] {
                &["report"]
            }
        }
        let text = "<report>{\"x\":3,\"y\":4}</report>";
        let b: Bare = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(b, Bare { x: 3, y: 4 });
    }

    #[test]
    fn decode_empty_wrapper_tags_means_bare_json() {
        // Default wrapper_tags() = [] → behaves like decode_json.
        #[derive(Debug, Deserialize, PartialEq)]
        struct BareResp {
            x: i32,
            y: i32,
        }
        impl JsonAgentResponse for BareResp {}
        let b: BareResp = decode(
            "prefix {\"x\":5,\"y\":6} suffix",
            StructuredOutputMode::None,
            DecodeOpts::default(),
        )
        .unwrap();
        assert_eq!(b, BareResp { x: 5, y: 6 });
    }

    #[test]
    fn decode_extracts_fenced_json_inside_envelope() {
        // Envelope whose body is itself a ```json fence.
        let text = "<report>```json\n{\"x\":7,\"y\":8}\n```</report>";
        let w: Wrapped = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(w, Wrapped { x: 7, y: 8 });
    }

    // ── decode: opt-in integer clamp ────────────────────────────────

    #[derive(Debug, Deserialize, PartialEq)]
    struct HasId {
        id: i64,
    }
    impl JsonAgentResponse for HasId {}

    #[test]
    fn clamp_off_by_default_overflow_int_is_schema_error() {
        // u64::MAX doesn't fit i64 → Schema error when clamp is off.
        let text = r#"{"id": 18446744073709551615}"#;
        let err =
            decode::<HasId>(text, StructuredOutputMode::StrictSchema, DecodeOpts::default())
                .unwrap_err();
        assert!(matches!(err, TarsJsonError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn clamp_on_recovers_overflow_int_to_i64_max() {
        let text = r#"{"id": 18446744073709551615}"#;
        let h: HasId =
            decode(text, StructuredOutputMode::StrictSchema, DecodeOpts::clamping()).unwrap();
        assert_eq!(h, HasId { id: i64::MAX });
    }

    #[test]
    fn clamp_on_leaves_in_range_ints_and_floats_untouched() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Mix {
            a: i64,
            b: f64,
            c: i64,
        }
        impl JsonAgentResponse for Mix {}
        // a in range, b float, c negative — none should be clamped.
        let text = r#"{"a": 42, "b": 3.5, "c": -100}"#;
        let m: Mix =
            decode(text, StructuredOutputMode::StrictSchema, DecodeOpts::clamping()).unwrap();
        assert_eq!(m, Mix { a: 42, b: 3.5, c: -100 });
    }

    #[test]
    fn clamp_walks_nested_arrays_and_objects() {
        let mut v = json!({
            "outer": [ {"n": 18446744073709551615u64}, {"n": 5} ],
            "flat": 5
        });
        clamp_ints_in_place(&mut v);
        assert_eq!(v["outer"][0]["n"], json!(i64::MAX));
        assert_eq!(v["outer"][1]["n"], json!(5));
        assert_eq!(v["flat"], json!(5));
    }

    // ── JsonValueType ───────────────────────────────────────────────

    #[test]
    fn json_value_type_classifies_and_names() {
        assert_eq!(JsonValueType::of(&json!(null)), JsonValueType::Null);
        assert_eq!(JsonValueType::of(&json!(true)), JsonValueType::Bool);
        assert_eq!(JsonValueType::of(&json!(7)), JsonValueType::Integer);
        assert_eq!(JsonValueType::of(&json!(-7)), JsonValueType::Integer);
        assert_eq!(JsonValueType::of(&json!(1.5)), JsonValueType::Float);
        assert_eq!(JsonValueType::of(&json!("s")), JsonValueType::String);
        assert_eq!(JsonValueType::of(&json!([1, 2])), JsonValueType::Array);
        assert_eq!(JsonValueType::of(&json!({"k": 1})), JsonValueType::Object);

        assert_eq!(JsonValueType::Object.to_string(), "dict");
        assert_eq!(JsonValueType::Array.py_name(), "list");
        assert_eq!(JsonValueType::Null.py_name(), "NoneType");
        assert_eq!(JsonValueType::Integer.to_string(), "int");
    }
}
