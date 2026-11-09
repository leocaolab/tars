//! Talks to DeepSeek, which differs from OpenAI twice. Its thinking switch is a top-level
//! `thinking: {"type": "enabled" | "disabled"}`, not the `chat_template_kwargs.enable_thinking` of
//! vLLM and Qwen; only this dialect sends it, since OpenAI rejects the field. And it sometimes
//! answers a tool call with its own DSML markup in the text, which this dialect turns into tool
//! calls once the text is whole.

use serde_json::{Value, json};

use crate::ProviderError;
use tars_types::{ChatRequest, ThinkingMode};

use super::super::adapter::OpenAiAdapter;
use super::OpenAiDialect;

/// DeepSeek, at `api.deepseek.com` or behind a gateway; its stream events and usage are
/// OpenAI's.
pub struct DeepSeekDialect;

impl OpenAiDialect for DeepSeekDialect {
    // DSML markup spans chunks.
    fn text_is_only_whole(&self) -> bool {
        true
    }

    /// Moves DSML tool calls out of the text; see [`lift_dsml`].
    fn finalize(&self, text: &mut String, tool_calls: &mut Vec<tars_types::ToolCall>) {
        if text.contains(DSML) {
            let (lifted, calls) = lift_dsml(text);
            *text = lifted;
            tool_calls.extend(calls);
        }
    }

    fn build_request(
        &self,
        adapter: &OpenAiAdapter,
        req: &ChatRequest,
        model: &str,
    ) -> Result<Value, ProviderError> {
        let mut body = adapter.build_request_default(req, model)?;

        if let Some(mode) = req.thinking {
            let switch = match mode {
                ThinkingMode::Off => "disabled",
                ThinkingMode::Auto | ThinkingMode::Budget(_) => "enabled",
            };
            body["thinking"] = json!({ "type": switch });
        }

        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::openai::provider::DEFAULT_BASE_URL;
    use crate::provider::http_base::HttpOverrides;
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
            thinking: Some(t),
            enable_chat_template_thinking: None,
        }
    }

    #[test]
    fn a_request_that_says_nothing_about_thinking_sends_no_thinking_field() {
        let adapter = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpOverrides::default(),
            crate::provider::backends::openai::adapter::profile_with(Some(
                StructuredOutputMode::JsonObjectMode,
            )),
        );
        let body = DeepSeekDialect
            .build_request(&adapter, &ChatRequest::user("hi"), "deepseek-v4-flash")
            .unwrap();
        assert!(body.get("thinking").is_none(), "{body}");
    }

    #[test]
    fn deepseek_dialect_emits_top_level_thinking_field() {
        // OpenAI's own URL: the field comes from the dialect, not the URL.
        let adapter = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpOverrides::default(),
            crate::provider::backends::openai::adapter::profile_with(Some(
                StructuredOutputMode::JsonObjectMode,
            )),
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

        assert_eq!(enabled["model"], "deepseek-v4-flash");
        assert_eq!(enabled["stream"], true);
    }
}

/// Opens each tag of DeepSeek's tool-call markup; the bars are two U+FF5C FULLWIDTH VERTICAL
/// LINE, not ASCII `|`.
const DSML: &str = "<｜｜DSML｜｜";
const DSML_CLOSE: &str = "</｜｜DSML｜｜";
const INVOKE_OPEN: &str = "<｜｜DSML｜｜invoke name=\"";
const INVOKE_CLOSE: &str = "</｜｜DSML｜｜invoke>";
const PARAMETER_OPEN: &str = "<｜｜DSML｜｜parameter name=\"";
const PARAMETER_CLOSE: &str = "</｜｜DSML｜｜parameter>";

/// Returns the parameters of one invoke's body, each value as a string; a parameter whose
/// header has no `>` or no closing quote before its closing tag is skipped.
fn dsml_parameters(body: &str) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    let mut p = body;
    while let Some(j) = p.find(PARAMETER_OPEN) {
        let a = &p[j + PARAMETER_OPEN.len()..];
        let Some(pe) = a.find(PARAMETER_CLOSE) else {
            break;
        };
        let element = &a[..pe];
        p = &a[pe + PARAMETER_CLOSE.len()..];
        let parsed = element
            .split_once('>')
            .and_then(|(header, value)| Some((header.split_once('"')?.0, value)));
        if let Some((key, value)) = parsed {
            args.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
    args
}

/// Returns `text` without its DSML markup, and the tool calls the markup held. DeepSeek writes
/// this instead of a JSON tool call; it is DeepSeek's wire format, which a caller reading the
/// text has no reason to know. Every parameter becomes a string, whatever its `string`
/// attribute says; a call cut off before its closing tag makes no call.
///
/// ```text
/// <｜｜DSML｜｜tool_calls>
/// <｜｜DSML｜｜invoke name="grep">
/// <｜｜DSML｜｜parameter name="pattern" string="true">struct Reason</｜｜DSML｜｜parameter>
/// </｜｜DSML｜｜invoke>
/// </｜｜DSML｜｜tool_calls>
/// ```
///
pub(crate) fn lift_dsml(text: &str) -> (String, Vec<tars_types::ToolCall>) {
    if !text.contains(DSML) {
        return (text.to_string(), Vec::new());
    }
    let mut calls = Vec::new();
    let mut rest = text;
    let mut kept = String::new();

    while let Some(i) = rest.find(INVOKE_OPEN) {
        kept.push_str(&rest[..i]);
        let after = &rest[i + INVOKE_OPEN.len()..];
        let Some(close) = after.find(INVOKE_CLOSE) else {
            // Cut off before its closing tag: no call, and the markup is dropped below.
            kept.push_str(&rest[i..]);
            rest = "";
            break;
        };
        let element = &after[..close];
        rest = &after[close + INVOKE_CLOSE.len()..];
        // The header ends at the first `>` inside the element; a model that leaves it out wrote
        // no call, and the markup is dropped below.
        let Some((name, body)) = element
            .split_once('>')
            .and_then(|(header, body)| Some((header.split_once('"')?.0, body)))
        else {
            kept.push_str(INVOKE_OPEN);
            kept.push_str(element);
            continue;
        };
        calls.push(tars_types::ToolCall::new(
            format!("dsml_{}", calls.len()),
            name,
            dsml_parameters(body),
        ));
    }
    kept.push_str(rest);

    // Whatever markup is left, the `tool_calls` wrapper among it, is not words: kept, the caller
    // would see an answer that ends mid-tag. It is dropped line by line.
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

    #[test]
    fn an_ordinary_answer_is_untouched() {
        let plain = "Let me read the file.\n\n{\"action\":\"fs_read\",\"path\":\"a.rs\"}";
        let (text, calls) = lift_dsml(plain);
        assert!(calls.is_empty());
        assert_eq!(text, plain);
    }

    #[test]
    fn a_truncated_call_does_not_panic() {
        let cut = "thinking\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"grep\">\n\
<｜｜DSML｜｜parameter name=\"pattern\" string=\"true\">abc";
        let (_, calls) = lift_dsml(cut);
        assert!(calls.is_empty(), "{calls:?}");
    }
    #[test]
    fn an_invoke_whose_header_never_closes_makes_no_call() {
        let text = "before\n<｜｜DSML｜｜invoke name=\"x\"</｜｜DSML｜｜invoke>\nafter";
        let (text, calls) = lift_dsml(text);
        assert!(calls.is_empty(), "{calls:?}");
        assert_eq!(text, "before\nafter");
    }

    #[test]
    fn a_parameter_whose_header_never_closes_is_skipped() {
        let text = "<｜｜DSML｜｜invoke name=\"x\">\
                    <｜｜DSML｜｜parameter name=\"k\"</｜｜DSML｜｜parameter>\
                    <｜｜DSML｜｜parameter name=\"ok\">v</｜｜DSML｜｜parameter>\
                    </｜｜DSML｜｜invoke>";
        let (_, calls) = lift_dsml(text);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].name, "x");
        assert_eq!(calls[0].arguments.len(), 1, "{:?}", calls[0].arguments);
        assert_eq!(calls[0].arguments["ok"], "v");
    }

    #[test]
    fn a_truncated_call_keeps_the_words_before_it_once() {
        let cut = "thinking\n<｜｜DSML｜｜invoke name=\"grep\">\nabc";
        let (text, _) = lift_dsml(cut);
        assert_eq!(text, "thinking\nabc");
    }

    #[test]
    fn finalize_lifts_markup_out_of_an_assembled_response() {
        let (mut text, mut tool_calls) = (ANSWER.to_string(), Vec::new());
        DeepSeekDialect.finalize(&mut text, &mut tool_calls);

        assert_eq!(tool_calls.len(), 2, "{:?}", tool_calls);
        assert_eq!(tool_calls[0].name, "grep");
        assert_eq!(tool_calls[0].arguments["pattern"], "struct JournaledReason");
        assert_eq!(
            text, "I need the failure shape first.",
            "the words, not the markup"
        );
    }

    #[test]
    fn finalize_leaves_an_ordinary_response_alone() {
        let plain = "Let me read the file.\n\n{\"action\":\"fs_read\",\"path\":\"a.rs\"}";
        let (mut text, mut tool_calls) = (plain.to_string(), Vec::new());
        DeepSeekDialect.finalize(&mut text, &mut tool_calls);
        assert!(tool_calls.is_empty());
        assert_eq!(text, plain);
    }

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

    #[test]
    fn a_dsml_reply_with_reasoning_reads_the_same_whole_and_streamed() {
        use serde_json::json;
        use std::sync::Arc;

        use super::super::both_paths;

        let body = json!({
            "model": "deepseek-v4-flash",
            "choices": [{
                "message": { "reasoning_content": "Find the type first.", "content": ANSWER },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 20 }
        });
        let (whole, streamed) = both_paths::read(Arc::new(DeepSeekDialect), &body);

        assert_eq!(both_paths::content(&whole), both_paths::content(&streamed));
        assert_eq!(whole.tool_calls.len(), 2, "{:?}", whole.tool_calls);
        assert_eq!(whole.thinking, "Find the type first.");
    }
}
