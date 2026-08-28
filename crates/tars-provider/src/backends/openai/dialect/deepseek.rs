//! `DeepSeekDialect` — the one genuine per-provider quirk.
//!
//! DeepSeek's thinking toggle is its OWN openai-compat extension: a top-level
//! `thinking: {"type": "enabled"|"disabled"}` (what the openai client merges
//! from `extra_body`) — NOT the vLLM/Qwen `chat_template_kwargs.enable_thinking`
//! that the shared builder already emits. It maps the generic `req.thinking` so
//! deepseek-v4-flash (thinking-off by default) can be flipped on for a benchmark
//! and -pro turned off.
//!
//! This lives in the dialect, not the shared builder: only a provider whose
//! dialect is `DeepSeekDialect` emits `thinking`, so a stray field never
//! reaches OpenAI proper and the shared builder branches on no provider name
//! or base_url string.

use serde_json::{Value, json};

use tars_types::{ChatRequest, ProviderError, ThinkingMode};

use super::super::adapter::OpenAiAdapter;
use super::OpenAiDialect;

/// DeepSeek (`api.deepseek.com` and openai_compat gateways fronting it).
///
/// Overrides only `build_request`: the standard body plus DeepSeek's
/// top-level `thinking: {type}`. Every other method keeps the default
/// (delegates to the shared adapter/mapping) — DeepSeek's SSE deltas and
/// usage are standard OpenAI shapes.
pub struct DeepSeekDialect;

impl OpenAiDialect for DeepSeekDialect {
    /// The standard parse, then DeepSeek's native tool-call markup lifted out of the
    /// text into [`ToolCall`]s. See [`lift_dsml`] — a caller reading `text` has no
    /// reason to know what `<｜｜DSML｜｜invoke …>` is, and measured on one run it did
    /// not: seven well-formed calls were scored as the model saying nothing.
    /// See [`OpenAiDialect::text_is_only_whole`]. `<｜｜DSML｜｜invoke …>` spans chunks.
    fn text_is_only_whole(&self) -> bool {
        true
    }

    fn finalize(&self, r: &mut tars_types::ChatResponse) {
        if r.text.contains(DSML) {
            let (text, calls) = lift_dsml(&r.text);
            r.text = text;
            r.tool_calls.extend(calls);
        }
    }

    fn build_request(
        &self,
        adapter: &OpenAiAdapter,
        req: &ChatRequest,
        model: &str,
    ) -> Result<Value, ProviderError> {
        let mut body = adapter.build_request_default(req, model)?;

        // DeepSeek's own thinking toggle, on top of the standard body.
        let mode = match req.thinking {
            ThinkingMode::Off => "disabled",
            ThinkingMode::Auto | ThinkingMode::Budget(_) => "enabled",
        };
        body["thinking"] = json!({ "type": mode });

        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::openai::provider::DEFAULT_BASE_URL;
    use crate::http_base::HttpProviderExtras;
    use tars_types::{Message, StructuredOutputMode};

    fn req(t: ThinkingMode) -> ChatRequest {
        ChatRequest {
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: t,
            enable_chat_template_thinking: None,
        }
    }

    /// `DeepSeekDialect::build_request` directly emits the top-level
    /// `thinking: {type}` field — Auto/Budget → enabled, Off → disabled —
    /// on top of the standard body, in isolation from any host.
    #[test]
    fn deepseek_dialect_emits_top_level_thinking_field() {
        // The base_url here is the plain OpenAI default: proving the field
        // comes from the DIALECT, not from any base_url string match.
        let adapter = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpProviderExtras::default(),
            StructuredOutputMode::JsonObjectMode,
        );

        let enabled = DeepSeekDialect
            .build_request(&adapter, &req(ThinkingMode::Auto), "deepseek-v4-flash")
            .unwrap();
        assert_eq!(enabled["thinking"]["type"], "enabled");

        let budget = DeepSeekDialect
            .build_request(
                &adapter,
                &req(ThinkingMode::Budget(1024)),
                "deepseek-v4-flash",
            )
            .unwrap();
        assert_eq!(budget["thinking"]["type"], "enabled");

        let disabled = DeepSeekDialect
            .build_request(&adapter, &req(ThinkingMode::Off), "deepseek-v4-flash")
            .unwrap();
        assert_eq!(disabled["thinking"]["type"], "disabled");

        // Standard body is preserved — it is the adapter default plus one field.
        assert_eq!(enabled["model"], "deepseek-v4-flash");
        assert_eq!(enabled["stream"], true);
    }
}

// ────────────────────────── DSML tool calls ──────────────────────────

/// The sentinel DeepSeek wraps its native tool-call markup in.
///
/// The bars are U+FF5C FULLWIDTH VERTICAL LINE, doubled — not ASCII `|`.
const DSML: &str = "<｜｜DSML｜｜";
const DSML_CLOSE: &str = "</｜｜DSML｜｜";

/// Lift DeepSeek's native tool-call markup out of assistant text into
/// [`ToolCall`]s, and return the text with the markup removed.
///
/// # Why this is here and not in the caller
///
/// The model emits this instead of a JSON body when it decides to call a tool
/// its own way, and it is a wire format — DeepSeek's, not ours. A caller that
/// reads `ChatResponse.text` sees a blob of `<｜｜DSML｜｜invoke …>` and has no
/// reason to know what it is; measured on one 36-turn run, seven of its answers
/// were exactly this and every one of them was scored as "the model talked and
/// the world did not move". The calls were well formed and well aimed —
/// `grep "struct JournaledReason" in crates/tars-types/src` — and were thrown
/// away, after which the model fell back to a broader search that returned
/// 196 KB.
///
/// Every other provider's native shape is normalized in its own dialect. This
/// one was not, only because nobody had seen it yet.
///
/// # Shape
///
/// ```text
/// <｜｜DSML｜｜tool_calls>
/// <｜｜DSML｜｜invoke name="grep">
/// <｜｜DSML｜｜parameter name="pattern" string="true">struct Reason</｜｜DSML｜｜parameter>
/// </｜｜DSML｜｜invoke>
/// </｜｜DSML｜｜tool_calls>
/// ```
///
/// Parsed by scanning, not by regex: the markup is fixed and this runs on every
/// DeepSeek response.
pub(crate) fn lift_dsml(text: &str) -> (String, Vec<tars_types::ToolCall>) {
    if !text.contains(DSML) {
        return (text.to_string(), Vec::new());
    }
    let mut calls = Vec::new();
    let mut rest = text;
    let mut kept = String::new();

    while let Some(i) = rest.find(concat!("<｜｜DSML｜｜", "invoke name=\"")) {
        kept.push_str(&rest[..i]);
        let after = &rest[i + "<｜｜DSML｜｜invoke name=\"".len()..];
        let Some(q) = after.find('"') else { break };
        let name = &after[..q];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let Some(close) = after.find(concat!("</｜｜DSML｜｜", "invoke>")) else {
            break;
        };
        let body = &after[open_end + 1..close];

        let mut args = serde_json::Map::new();
        let mut p = body;
        while let Some(j) = p.find(concat!("<｜｜DSML｜｜", "parameter name=\"")) {
            let a = &p[j + "<｜｜DSML｜｜parameter name=\"".len()..];
            let Some(q2) = a.find('"') else { break };
            let key = a[..q2].to_string();
            let Some(ge) = a.find('>') else { break };
            let Some(pe) = a.find(concat!("</｜｜DSML｜｜", "parameter>")) else {
                break;
            };
            args.insert(key, Value::String(a[ge + 1..pe].to_string()));
            p = &a[pe..];
        }

        calls.push(tars_types::ToolCall::new(
            format!("dsml_{}", calls.len()),
            name,
            Value::Object(args),
        ));
        rest = &after[close + "</｜｜DSML｜｜invoke>".len()..];
    }
    kept.push_str(rest);

    // Whatever wrapper survived (`<…tool_calls>` and its close) is markup, not
    // words. Left in, the caller sees an answer that ends mid-tag.
    let cleaned: String = kept
        .lines()
        .filter(|l| !l.trim_start().starts_with(DSML) && !l.trim_start().starts_with(DSML_CLOSE))
        .collect::<Vec<_>>()
        .join("\n");
    (cleaned.trim().to_string(), calls)
}

#[cfg(test)]
mod dsml_tests {
    use super::*;

    const ANSWER: &str = "I need the failure shape first.\n\n\
<｜｜DSML｜｜tool_calls>\n\
<｜｜DSML｜｜invoke name=\"grep\">\n\
<｜｜DSML｜｜parameter name=\"pattern\" string=\"true\">struct JournaledReason</｜｜DSML｜｜parameter>\n\
<｜｜DSML｜｜parameter name=\"path\" string=\"true\">crates/tars-types/src</｜｜DSML｜｜parameter>\n\
</｜｜DSML｜｜invoke>\n\
<｜｜DSML｜｜invoke name=\"read\">\n\
<｜｜DSML｜｜parameter name=\"path\" string=\"true\">migrations/0001.sql</｜｜DSML｜｜parameter>\n\
</｜｜DSML｜｜invoke>\n\
</｜｜DSML｜｜tool_calls>";

    /// The name and every parameter, as a call. Measured: seven answers in one
    /// run were this, and all seven were scored as the model saying nothing.
    #[test]
    fn a_dsml_answer_becomes_tool_calls() {
        let (text, calls) = lift_dsml(ANSWER);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].name, "grep");
        assert_eq!(calls[0].arguments["pattern"], "struct JournaledReason");
        assert_eq!(calls[0].arguments["path"], "crates/tars-types/src");
        assert_eq!(calls[1].name, "read");
        assert_eq!(calls[1].arguments["path"], "migrations/0001.sql");
        assert_eq!(
            text, "I need the failure shape first.",
            "the words, without the markup: {text:?}"
        );
    }

    /// An answer with no markup is returned untouched — this runs on every
    /// DeepSeek response and must not disturb the ordinary ones.
    #[test]
    fn an_ordinary_answer_is_untouched() {
        let plain = "Let me read the file.\n\n{\"action\":\"fs_read\",\"path\":\"a.rs\"}";
        let (text, calls) = lift_dsml(plain);
        assert!(calls.is_empty());
        assert_eq!(text, plain);
    }

    /// Truncated markup yields whatever calls completed and never panics: a
    /// response cut off mid-tag is a thing that happens, and losing the whole
    /// turn to it would be the defect this function exists to fix.
    #[test]
    fn a_truncated_call_does_not_panic() {
        let cut = "thinking\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"grep\">\n\
<｜｜DSML｜｜parameter name=\"pattern\" string=\"true\">abc";
        let (_, calls) = lift_dsml(cut);
        assert!(calls.is_empty(), "{calls:?}");
    }
    /// The lift runs on a FINISHED response, whichever path assembled it.
    ///
    /// This is the test that was missing. `parse_response` was overridden and covered
    /// the batch path; real runs stream, so the override never ran, and one 59-turn
    /// run spent eighteen consecutive turns emitting markup that reached the agent
    /// verbatim and produced nothing. The hook is `finalize`, and a dialect test that
    /// only exercised `lift_dsml` directly could not have caught it.
    #[test]
    fn finalize_lifts_markup_out_of_an_assembled_response() {
        use tars_types::ChatResponse;
        let mut r = ChatResponse {
            text: ANSWER.to_string(),
            ..Default::default()
        };
        DeepSeekDialect.finalize(&mut r);

        assert_eq!(r.tool_calls.len(), 2, "{:?}", r.tool_calls);
        assert_eq!(r.tool_calls[0].name, "grep");
        assert_eq!(
            r.tool_calls[0].arguments["pattern"],
            "struct JournaledReason"
        );
        assert_eq!(
            r.text, "I need the failure shape first.",
            "the words, not the markup"
        );
    }

    /// An ordinary streamed answer is untouched — `finalize` runs on every DeepSeek
    /// response, so it must cost nothing and change nothing when there is no markup.
    #[test]
    fn finalize_leaves_an_ordinary_response_alone() {
        use tars_types::ChatResponse;
        let plain = "Let me read the file.\n\n{\"action\":\"fs_read\",\"path\":\"a.rs\"}";
        let mut r = ChatResponse {
            text: plain.to_string(),
            ..Default::default()
        };
        DeepSeekDialect.finalize(&mut r);
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.text, plain);
    }

    /// The batch path goes through the same hook, so the two cannot drift.
    #[test]
    fn the_batch_path_lifts_it_too() {
        use serde_json::json;
        let body = json!({
            "choices": [{ "message": { "content": ANSWER }, "finish_reason": "stop" }],
            "model": "deepseek-v4-flash"
        });
        let r = DeepSeekDialect.parse_response(&body).expect("parses");
        assert_eq!(r.tool_calls.len(), 2, "{:?}", r.tool_calls);
    }
}
