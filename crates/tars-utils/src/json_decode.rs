//! Turns an LLM completion's text into a typed value, so no consumer scrapes JSON out of a
//! chat response on its own.
//!
//! How much the text is trusted follows the [`StructuredOutputMode`] the response was
//! produced under (see [`decode_json`]). The envelope tag strings and the integer clamp are
//! the consumer's choice, through [`JsonAgentResponse`] and [`DecodeOpts`]; this module
//! knows no tag values and no domain types.

use serde::de::DeserializeOwned;
use serde_json::Value;

use tars_types::ChatResponse;
use tars_types::StructuredOutputMode;

/// A failure of [`decode_json`], [`decode`] or [`ResponseJsonExt::json`].
#[derive(Debug, thiserror::Error)]
pub enum DecodeJsonError {
    /// The response text was empty or whitespace, as after a tool-only turn or a
    /// filtered completion.
    #[error("response text was empty; nothing to decode")]
    EmptyStream,

    /// The response type declares [`wrapper_tags`] and no `<tag>…</tag>` of them is in the
    /// text. `tried` lists the tags in the order they were looked for.
    ///
    /// [`wrapper_tags`]: JsonAgentResponse::wrapper_tags
    #[error("no envelope block found; expected one of {tried:?}")]
    MissingBlock { tried: Vec<String> },

    /// Chatty (`None` / `ToolUseEmulation`) text held no balanced `{…}` or `[…]` region and
    /// no truncated JSON value.
    /// `attempts` counts the start positions tried, at most `MAX_SCRAPE_ATTEMPTS`.
    #[error("no JSON value found in response text after scanning {attempts} candidate(s)")]
    NoJsonObject { attempts: usize },

    /// The text, or every balanced region scraped from it, is not valid JSON. In the
    /// native modes this means the provider broke its promise of a clean document; in the
    /// chatty modes a reply cut off inside its JSON lands here too, with
    /// [`serde_json::Error::is_eof`] true.
    #[error("response text is not valid JSON")]
    InvalidJson {
        #[source]
        source: serde_json::Error,
    },

    /// The JSON is valid but does not fit the target type; the `serde` message names the
    /// offending path.
    #[error("JSON did not match the expected type")]
    Schema {
        #[source]
        source: serde_json::Error,
    },
}

impl DecodeJsonError {
    /// Classifies a `serde_json` failure: [`Category::Data`](serde_json::error::Category::Data)
    /// is [`Schema`](Self::Schema), anything else is [`InvalidJson`](Self::InvalidJson).
    fn from_serde(source: serde_json::Error) -> Self {
        match source.classify() {
            serde_json::error::Category::Data => Self::Schema { source },
            _ => Self::InvalidJson { source },
        }
    }
}

/// A JSON value's type, displayed by its Python name (`NoneType`, `bool`, `int`, `float`,
/// `str`, `list`, `dict`) for "expected X, got Y" messages.
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
    /// Returns the type of `v`; a number is [`Float`] when it was parsed as one and
    /// [`Integer`] otherwise.
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

    /// Returns the Python type name.
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

/// Options for [`decode`]. The default is lossless; every recovery is opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeOpts {
    /// Clamps every integer above `i64::MAX` to `i64::MAX` before the final deserialize, a
    /// lossy recovery for a model that emits a bogus 20-digit id into an `i64` field.
    pub clamp_ints: bool,
}

impl DecodeOpts {
    /// Returns options with integer clamping on.
    pub fn clamping() -> Self {
        Self { clamp_ints: true }
    }
}

/// A response type that may arrive wrapped in an envelope tag of its own choosing, such as
/// `<some_report>…</some_report>`.
pub trait JsonAgentResponse: DeserializeOwned {
    /// Returns the envelope tags to try, in order, the first present one winning; an entry
    /// may omit the angle brackets. The default, empty, means bare JSON.
    fn wrapper_tags() -> &'static [&'static str] {
        &[]
    }
}

/// Bounds the `{` / `[` start positions the chatty-text scraper tries, so a completion
/// full of stray braces does not make decoding quadratic.
const MAX_SCRAPE_ATTEMPTS: usize = 50;

/// Decodes `text` into `T` with no envelope and no clamp.
///
/// Under [`StrictSchema`](StructuredOutputMode::StrictSchema) and
/// [`JsonObjectMode`](StructuredOutputMode::JsonObjectMode) the provider promised a clean
/// document, so the trimmed text is parsed as it is. Under
/// [`None`](StructuredOutputMode::None) and
/// [`ToolUseEmulation`](StructuredOutputMode::ToolUseEmulation) the text may be prose: an
/// outer code fence is stripped and the first balanced `{…}` or `[…]` that parses is used. A
/// balanced region that does not parse is passed over whole: a value nested inside a broken
/// answer is not the answer.
///
/// # Errors
///
/// Returns [`DecodeJsonError::EmptyStream`] for whitespace-only text, and the other variants
/// as they describe.
pub fn decode_json<T: DeserializeOwned>(
    text: &str,
    mode: StructuredOutputMode,
) -> Result<T, DecodeJsonError> {
    let value = parse_value(text, mode)?;
    serde_json::from_value::<T>(value).map_err(DecodeJsonError::from_serde)
}

/// Decodes `text` into `T`: takes the body of `T`'s envelope tag when it declares any, and
/// otherwise the text with an outer code fence stripped in every mode; then parses it by
/// `mode` as [`decode_json`] does, clamping integers if `opts` asks.
///
/// The envelope is looked for in the raw text, before any code fence is stripped: a reply
/// that shows a fenced diff and then its `<tag>…</tag>` would otherwise lose the envelope
/// with the leading fence.
///
/// # Errors
///
/// Returns [`DecodeJsonError::MissingBlock`] when `T` declares tags and none is present, and
/// the errors of [`decode_json`] otherwise.
pub fn decode<T: JsonAgentResponse>(
    text: &str,
    mode: StructuredOutputMode,
    opts: DecodeOpts,
) -> Result<T, DecodeJsonError> {
    let tags = T::wrapper_tags();
    let inner = if tags.is_empty() {
        strip_code_fences(text)
    } else {
        extract_tag_block(text, tags).ok_or_else(|| DecodeJsonError::MissingBlock {
            tried: tags.iter().map(|t| (*t).to_string()).collect(),
        })?
    };

    let mut value = parse_value(inner, mode)?;
    if opts.clamp_ints {
        clamp_ints_in_place(&mut value);
    }
    serde_json::from_value::<T>(value).map_err(DecodeJsonError::from_serde)
}

/// Parses `text` to a [`Value`] by `mode`, leaving the typed deserialize to the caller so a
/// clamp can run in between.
fn parse_value(text: &str, mode: StructuredOutputMode) -> Result<Value, DecodeJsonError> {
    if text.trim().is_empty() {
        return Err(DecodeJsonError::EmptyStream);
    }
    match mode {
        StructuredOutputMode::StrictSchema | StructuredOutputMode::JsonObjectMode => {
            serde_json::from_str::<Value>(text.trim()).map_err(DecodeJsonError::from_serde)
        }
        StructuredOutputMode::None | StructuredOutputMode::ToolUseEmulation => scrape_value(text),
    }
}

/// Strips an outer code fence, then returns the first balanced `{…}` or `[…]` that parses.
///
/// An opener that never balances ends the scan with an error when the text after it is a
/// truncated JSON value, and is skipped as prose otherwise.
fn scrape_value(text: &str) -> Result<Value, DecodeJsonError> {
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
        match find_balanced(bytes, i, open, close) {
            Some(end) => match serde_json::from_str::<Value>(&body[i..=end]) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    i = end + 1;
                }
            },
            None => {
                if let Some(source) = truncation(&body[i..]) {
                    return Err(DecodeJsonError::InvalidJson { source });
                }
                i += 1;
            }
        }
    }

    match last_err {
        Some(source) => Err(DecodeJsonError::InvalidJson { source }),
        None => Err(DecodeJsonError::NoJsonObject { attempts }),
    }
}

/// Returns the parse error of `rest` when it is a JSON value cut off before its end, as a
/// reply stopped at `max_tokens` is; `None` when it breaks off as prose does, so the scan
/// moves past a stray `{` instead of decoding a value nested inside a cut-off reply.
fn truncation(rest: &str) -> Option<serde_json::Error> {
    serde_json::from_str::<Value>(rest)
        .err()
        .filter(serde_json::Error::is_eof)
}

/// Returns the trimmed body of the first tag in `tags` whose open and close markers both
/// appear in `text`.
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

/// Clamps every integer above `i64::MAX`, at any depth, to `i64::MAX`.
fn clamp_ints_in_place(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(u) = n.as_u64()
                && u > i64::MAX as u64
            {
                *n = serde_json::Number::from(i64::MAX);
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(clamp_ints_in_place),
        Value::Object(map) => map.values_mut().for_each(clamp_ints_in_place),
        _ => {}
    }
}

/// Returns the trimmed body inside one outer ```` ``` ```` fence, or the trimmed text when
/// it does not start with one. A missing closing fence (truncated output) is tolerated.
fn strip_code_fences(text: &str) -> &str {
    let t = text.trim();
    let Some(after_open) = t.strip_prefix("```") else {
        return t;
    };
    let Some(nl) = after_open.find('\n') else {
        return t;
    };
    let inner = &after_open[nl + 1..];
    match inner.rfind("```") {
        Some(close) => inner[..close].trim(),
        None => inner.trim(),
    }
}

/// Returns the index of the `close` that balances the `open` at `bytes[start]`, skipping
/// string literals and their escapes; `None` when it never balances.
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

/// Adds `resp.json::<T>(mode)` to [`ChatResponse`], which lives in `tars-types`.
pub trait ResponseJsonExt {
    /// Decodes the response text as [`decode_json`] does. `mode` is the one the provider's
    /// [`ProviderProfile`](tars_types::ProviderProfile) says the response was produced under.
    fn json<T: DeserializeOwned>(&self, mode: StructuredOutputMode) -> Result<T, DecodeJsonError>;
}

impl ResponseJsonExt for ChatResponse {
    fn json<T: DeserializeOwned>(&self, mode: StructuredOutputMode) -> Result<T, DecodeJsonError> {
        decode_json(&self.text, mode)
    }
}
/// Returns the complete object and array elements of a possibly truncated JSON array,
/// found as [`find_json_array_start`] finds it.
///
/// Scalar elements are skipped, and so is an element that balances but does not parse;
/// nothing reports either.
pub fn salvage_json_array(raw: &str, array_key: Option<&str>) -> Vec<Value> {
    let Some(arr_start) = find_json_array_start(raw, array_key) else {
        return Vec::new();
    };
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut elem_start: Option<usize> = None;
    let mut i = arr_start;
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
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'[' => {
                if depth == 0 && elem_start.is_none() {
                    elem_start = Some(i);
                }
                depth += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if depth == 0
                    && let Some(s) = elem_start.take()
                {
                    // Braces are ASCII, so `s..=i` is a valid char boundary.
                    if let Ok(v) = serde_json::from_str::<Value>(&raw[s..=i]) {
                        out.push(v);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Returns the index just past the `[` that opens the target array: the first `[` after
/// `"array_key"` when that appears anywhere in `raw`, else the leading `[` of a reply that is
/// a bare array.
pub fn find_json_array_start(raw: &str, array_key: Option<&str>) -> Option<usize> {
    if let Some(key) = array_key {
        let search = format!("\"{}\"", key);
        if let Some(fpos) = raw.find(&search)
            && let Some(rel) = raw[fpos..].find('[')
        {
            return Some(fpos + rel + 1);
        }
    }
    let trimmed = raw.trim_start();
    if trimmed.starts_with('[') {
        return raw.find('[').map(|br| br + 1);
    }
    None
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

    #[test]
    fn an_opener_that_never_closes_does_not_hide_the_object_after_it() {
        let v: Point = decode_json(r#"{ {"x":1,"y":2}"#, StructuredOutputMode::None).unwrap();
        assert_eq!(v, Point { x: 1, y: 2 });
        let v: Vec<i32> = decode_json("see { here: [1, 2]", StructuredOutputMode::None).unwrap();
        assert_eq!(v, vec![1, 2]);
    }

    #[test]
    fn a_truncated_reply_is_invalid_json_not_its_inner_object() {
        let cut =
            r#"{"verdict": "fail", "evidence": {"file": "a.rs", "line": 3}, "fix": "rename the"#;
        let e = decode_json::<Value>(cut, StructuredOutputMode::None).unwrap_err();
        assert!(
            matches!(&e, DecodeJsonError::InvalidJson { source } if source.is_eof()),
            "{e:?}"
        );
        let e = decode_json::<Point>(r#"{"x":1"#, StructuredOutputMode::None).unwrap_err();
        assert!(
            matches!(&e, DecodeJsonError::InvalidJson { source } if source.is_eof()),
            "{e:?}"
        );
    }

    #[test]
    fn text_cut_off_inside_json_is_truncation_and_prose_is_not() {
        assert!(truncation(r#"{"fix": "rename the"#).is_some());
        assert!(truncation(r#"[1, 2"#).is_some());
        assert!(truncation(r#"{ {"x":1}"#).is_none());
        assert!(truncation("{ here: [1, 2]").is_none());
        assert!(truncation(r#"{"x":1}"#).is_none());
    }

    // ── decode_json: strict / native-JSON modes ─────────────────────

    #[test]
    fn strict_mode_parses_clean_json_directly() {
        let p: Point = decode_json(r#"{"x":1,"y":2}"#, StructuredOutputMode::StrictSchema).unwrap();
        assert_eq!(p, Point { x: 1, y: 2 });
    }

    #[test]
    fn strict_mode_tolerates_surrounding_whitespace() {
        let p: Point = decode_json(
            "  \n{\"x\":3,\"y\":4}\n ",
            StructuredOutputMode::StrictSchema,
        )
        .unwrap();
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
        // A fenced, chatty body breaks strict mode's promise of clean JSON.
        let err = decode_json::<Point>(
            "Here you go: ```json\n{\"x\":1,\"y\":2}\n```",
            StructuredOutputMode::StrictSchema,
        )
        .unwrap_err();
        assert!(
            matches!(err, DecodeJsonError::InvalidJson { .. }),
            "got {err:?}"
        );
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
    fn none_mode_broken_outer_object_is_invalid_not_its_nested_object() {
        let text = r#"Here you go: {"verdict": "fail", "items": [1, 2,], "meta": {"n": 1}}"#;
        let err = decode_json::<Value>(text, StructuredOutputMode::None).unwrap_err();
        assert!(
            matches!(err, DecodeJsonError::InvalidJson { .. }),
            "got {err:?}"
        );
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
            assert!(
                matches!(err, DecodeJsonError::EmptyStream),
                "mode {mode:?} → {err:?}"
            );
        }
    }

    #[test]
    fn none_mode_no_json_object_when_prose_has_none() {
        let err =
            decode_json::<Point>("no braces here at all", StructuredOutputMode::None).unwrap_err();
        match err {
            DecodeJsonError::NoJsonObject { attempts } => assert_eq!(attempts, 0),
            other => panic!("expected NoJsonObject, got {other:?}"),
        }
    }

    #[test]
    fn none_mode_valid_json_wrong_shape_is_schema_error() {
        let err = decode_json::<Point>(r#"answer: {"foo":1,"bar":2}"#, StructuredOutputMode::None)
            .unwrap_err();
        assert!(matches!(err, DecodeJsonError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn strict_mode_wrong_shape_is_schema_error() {
        let err =
            decode_json::<Point>(r#"{"foo":1}"#, StructuredOutputMode::StrictSchema).unwrap_err();
        assert!(matches!(err, DecodeJsonError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn strict_mode_malformed_json_is_invalid_json() {
        let err = decode_json::<Point>(r#"{"x":1,"y":}"#, StructuredOutputMode::StrictSchema)
            .unwrap_err();
        assert!(
            matches!(err, DecodeJsonError::InvalidJson { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn none_mode_attempts_are_bounded() {
        let text = "{x ".repeat(200);
        let err = decode_json::<Point>(&text, StructuredOutputMode::None).unwrap_err();
        match err {
            DecodeJsonError::NoJsonObject { attempts } => {
                assert!(attempts <= MAX_SCRAPE_ATTEMPTS, "attempts={attempts}");
            }
            other => panic!("expected NoJsonObject, got {other:?}"),
        }
    }

    #[test]
    fn chat_response_json_method_decodes_text() {
        let resp = ChatResponse {
            actual_model: Some("m".into()),
            text: "here: {\"x\":1,\"y\":2}".into(),
            thinking: String::new(),
            tool_calls: Vec::new(),
            stop_reason: tars_types::StopReason::EndTurn,
            usage: None,
            cache_hit: Default::default(),
            degraded: Vec::new(),
            validation_summary: Default::default(),
            created: 0,
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
            DecodeJsonError::MissingBlock { tried } => {
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
        fn wrapper_tags() -> &'static [&'static str] {
            &["<result>", "<legacy>"]
        }
    }

    #[test]
    fn decode_first_matching_tag_wins() {
        let text = "<legacy>{\"v\":1}</legacy>\n<result>{\"v\":2}</result>";
        let a: Aliased = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(a, Aliased { v: 2 });
    }

    #[test]
    fn decode_falls_back_to_legacy_alias_tag() {
        let text = "chatter <legacy>{\"v\":9}</legacy> chatter";
        let a: Aliased = decode(text, StructuredOutputMode::None, DecodeOpts::default()).unwrap();
        assert_eq!(a, Aliased { v: 9 });
    }

    #[test]
    fn decode_tag_written_without_brackets_is_equivalent() {
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
        let text = r#"{"id": 18446744073709551615}"#;
        let err = decode::<HasId>(
            text,
            StructuredOutputMode::StrictSchema,
            DecodeOpts::default(),
        )
        .unwrap_err();
        assert!(matches!(err, DecodeJsonError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn clamp_on_recovers_overflow_int_to_i64_max() {
        let text = r#"{"id": 18446744073709551615}"#;
        let h: HasId = decode(
            text,
            StructuredOutputMode::StrictSchema,
            DecodeOpts::clamping(),
        )
        .unwrap();
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
        let m: Mix = decode(
            text,
            StructuredOutputMode::StrictSchema,
            DecodeOpts::clamping(),
        )
        .unwrap();
        assert_eq!(
            m,
            Mix {
                a: 42,
                b: 3.5,
                c: -100
            }
        );
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

    #[test]
    fn salvage_array_recovers_complete_elements_and_drops_the_tail() {
        let raw = r#"{"findings":[
            {"id": 1, "msg": "first"},
            {"id": 2, "msg": "second"},
            {"id": 3, "msg": "third"#;
        let items = salvage_json_array(raw, Some("findings"));
        assert_eq!(items.len(), 2, "two complete objects survive");
        assert_eq!(items[0]["msg"], json!("first"));
        assert_eq!(items[1]["msg"], json!("second"));
    }

    #[test]
    fn salvage_array_recovers_clean_prefix_on_unescaped_quote() {
        let raw = r#"{"findings":[
            {"id": 1, "msg": "fully formed"},
            {"id": 2, "msg": "silent "quote" inside"},
            {"id": 3, "msg": "third"}
        ]}"#;
        let items = salvage_json_array(raw, Some("findings"));
        assert!(
            !items.is_empty(),
            "the complete object before the broken syntax survives"
        );
        assert_eq!(items[0]["msg"], json!("fully formed"));
    }

    #[test]
    fn salvage_array_empty_when_truncated_before_first_complete_element() {
        let raw = r#"{"findings":[{"id": 1, "ms"#;
        let items = salvage_json_array(raw, Some("findings"));
        assert!(items.is_empty(), "no complete element found");
    }

    #[test]
    fn salvage_array_does_not_match_non_target_array() {
        let raw = r#"{"metadata": [1, 2, 3], "other": [
            {"id": 1, "msg": "real finding"}
        ]"#;
        let items = salvage_json_array(raw, Some("findings"));
        assert!(items.is_empty(), "should not match the wrong array");
    }

    #[test]
    fn salvage_array_matches_bare_top_level_array() {
        let raw = r#"[
            {"id": 1, "msg": "first"}
        ]"#;
        let items = salvage_json_array(raw, None);
        assert_eq!(items.len(), 1, "bare top-level array matched");
        assert_eq!(items[0]["msg"], json!("first"));
    }

    #[test]
    fn salvage_array_matches_target_key_array() {
        let raw = r#"{"data": [ {"id": 1} ], "logs": [ {"id": 2} ] }"#;
        let items = salvage_json_array(raw, Some("logs"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], json!(2));
    }

    #[test]
    fn salvage_array_bare_elements_are_skipped() {
        let raw = r#"[ "string", 42, {"obj": true}, [1,2,3"#;
        let items = salvage_json_array(raw, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["obj"], json!(true));
    }
}
