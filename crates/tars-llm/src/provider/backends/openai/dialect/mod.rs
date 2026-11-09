//! Holds what one OpenAI-compatible server (DeepSeek, Groq, LM Studio, vLLM, …) does differently
//! from OpenAI, so the shared adapter has no per-server branches. Design:
//! `docs/architecture/30-openai-dialect.md`.

pub mod deepseek;
pub use deepseek::DeepSeekDialect;

use serde_json::Value;

use crate::ProviderError;
use tars_types::{ChatEvent, ChatRequest, ChatResponse, Usage};

use crate::provider::http_base::SseEvent;
use crate::provider::tool_buffer::StreamDecodeState;

use super::adapter::OpenAiAdapter;
use super::mapping::{openai_chat_completion_to_chat_response, parse_openai_usage};

/// One server's departures from OpenAI; every default is OpenAI's behavior.
pub trait OpenAiDialect: Send + Sync {
    /// Returns whether the text can be read only once it is whole; by default it is not, and
    /// each delta goes out as it arrives.
    /// When true, the provider holds the streamed text and hands it to [`Self::finalize`] at the
    /// end, so a consumer gets tool calls without knowing the dialect. DeepSeek's tool-call markup
    /// arrives split across chunks: forwarded piece by piece, it reaches the consumer as text and
    /// no call runs.
    fn text_is_only_whole(&self) -> bool {
        false
    }

    fn build_request(
        &self,
        adapter: &OpenAiAdapter,
        req: &ChatRequest,
        model: &str,
    ) -> Result<Value, ProviderError> {
        adapter.build_request_default(req, model)
    }

    fn parse_event(
        &self,
        adapter: &OpenAiAdapter,
        raw: &SseEvent,
        buf: &mut StreamDecodeState,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        adapter.parse_event_default(raw, buf)
    }

    /// Returns the usage of a streamed response; by default `parse_openai_usage`.
    fn parse_usage(&self, usage: &serde_json::Map<String, Value>) -> Usage {
        parse_openai_usage(usage)
    }

    /// Returns a whole (non-streamed) completion as a response, read as a stream of it would be:
    /// the same message reading, this dialect's usage, then [`Self::finalize`].
    fn parse_response(&self, raw: &Value) -> Result<ChatResponse, ProviderError> {
        let mut r = openai_chat_completion_to_chat_response(raw, |u| self.parse_usage(u))?;
        self.finalize(&mut r);
        Ok(r)
    }

    /// Fixes a whole response, for what no single event shows; by default it changes nothing.
    /// It runs on every path: streamed (when [`Self::text_is_only_whole`]), `complete` and batch.
    fn finalize(&self, _r: &mut ChatResponse) {}
}

/// OpenAI itself, and every server that follows it.
pub struct StandardDialect;

impl OpenAiDialect for StandardDialect {}

/// Reads one completion both ways a reply arrives, for the tests that hold them to one content.
#[cfg(test)]
pub(super) mod both_paths {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tars_types::{ChatResponse, ChatResponseBuilder, StructuredOutputMode};

    use super::OpenAiDialect;
    use crate::provider::backends::openai::adapter::{OpenAiAdapter, profile_with};
    use crate::provider::http_base::{HttpOverrides, SseEvent};
    use crate::provider::tool_buffer::StreamDecodeState;

    /// Returns `body` as the stream a server sends of it: reasoning, the text in two pieces,
    /// each tool call's id and name with half its arguments then the other half, the finish
    /// reason, the usage, `[DONE]`.
    fn stream_of(body: &Value) -> Vec<String> {
        let model = &body["model"];
        let choice = &body["choices"][0];
        let message = &choice["message"];
        let chunk =
            |delta: Value| json!({"model": model, "choices": [{"index": 0, "delta": delta}]});
        let halves = |s: &str| {
            let mid = s
                .char_indices()
                .nth(s.chars().count() / 2)
                .map_or(0, |(i, _)| i);
            (s[..mid].to_string(), s[mid..].to_string())
        };

        let mut frames = Vec::new();
        if let Some(r) = message["reasoning_content"].as_str() {
            frames.push(chunk(json!({"role": "assistant", "reasoning_content": r})));
        }
        if let Some(text) = message["content"].as_str() {
            let (a, b) = halves(text);
            frames.push(chunk(json!({"content": a})));
            frames.push(chunk(json!({"content": b})));
        }
        for (i, call) in message["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let (a, b) = halves(call["function"]["arguments"].as_str().unwrap_or(""));
            frames.push(chunk(json!({"tool_calls": [{
                "index": i, "id": call["id"], "type": "function",
                "function": {"name": call["function"]["name"], "arguments": a},
            }]})));
            frames.push(chunk(
                json!({"tool_calls": [{"index": i, "function": {"arguments": b}}]}),
            ));
        }
        frames.push(json!({
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": choice["finish_reason"]}],
        }));
        frames.push(json!({"model": model, "choices": [], "usage": body["usage"]}));
        let mut data: Vec<String> = frames.iter().map(Value::to_string).collect();
        data.push("[DONE]".to_string());
        data
    }

    /// Returns `body` read whole by `dialect`, and read as its stream then assembled and given to
    /// `finalize`, as `complete` does.
    pub(crate) fn read(
        dialect: Arc<dyn OpenAiDialect>,
        body: &Value,
    ) -> (ChatResponse, ChatResponse) {
        let whole = dialect
            .parse_response(body)
            .expect("the whole completion parses");

        let adapter = OpenAiAdapter::new(
            "http://localhost/v1".into(),
            HttpOverrides::default(),
            profile_with(Some(StructuredOutputMode::StrictSchema)),
        )
        .with_dialect(dialect.clone());
        let mut buf = StreamDecodeState::new();
        let mut acc = ChatResponseBuilder::new();
        for data in stream_of(body) {
            let event = SseEvent {
                event: "message".into(),
                data,
            };
            for ev in dialect
                .parse_event(&adapter, &event, &mut buf)
                .expect("each chunk parses")
            {
                acc.apply(ev);
            }
        }
        let mut streamed = acc.finish();
        dialect.finalize(&mut streamed);
        (whole, streamed)
    }

    /// Returns what a reader gets from a response: its model, text, thinking, tool calls, stop
    /// reason and usage, not the clock it was assembled at.
    pub(crate) fn content(r: &ChatResponse) -> Value {
        json!({
            "model": r.actual_model,
            "text": r.text,
            "thinking": r.thinking,
            "tool_calls": r.tool_calls,
            "stop_reason": r.stop_reason,
            "usage": r.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn standard_dialect_parse_response_matches_direct_path() {
        let body = json!({
            "model": "gpt-4o",
            "choices": [{
                "message": { "role": "assistant", "content": "hello world" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "completion_tokens_details": { "reasoning_tokens": 3 },
                "prompt_tokens_details": { "cached_tokens": 4 }
            }
        });

        let via_dialect = StandardDialect.parse_response(&body).unwrap();
        let direct = openai_chat_completion_to_chat_response(&body, parse_openai_usage).unwrap();

        // `created` is the clock at parse time, so it differs.
        assert_eq!(via_dialect.text, direct.text);
        assert_eq!(via_dialect.text, "hello world");
        assert_eq!(via_dialect.stop_reason, direct.stop_reason);
        assert_eq!(via_dialect.usage.input_tokens, direct.usage.input_tokens);
        assert_eq!(via_dialect.usage.output_tokens, direct.usage.output_tokens);
        assert_eq!(
            via_dialect.usage.thinking_tokens,
            direct.usage.thinking_tokens
        );
        assert_eq!(
            via_dialect.usage.cached_input_tokens,
            direct.usage.cached_input_tokens
        );
        assert_eq!(via_dialect.usage.input_tokens, 12);
        assert_eq!(via_dialect.usage.output_tokens, 8);
        assert_eq!(via_dialect.usage.thinking_tokens, 3);
        assert_eq!(via_dialect.usage.cached_input_tokens, 4);
    }

    #[test]
    fn standard_dialect_parse_usage_matches_direct_path() {
        let usage = json!({
            "prompt_tokens": 22,
            "completion_tokens": 224,
            "completion_tokens_details": { "reasoning_tokens": 130 }
        });
        let map = usage.as_object().unwrap();
        let via_dialect = StandardDialect.parse_usage(map);
        let direct = parse_openai_usage(map);
        assert_eq!(via_dialect.input_tokens, direct.input_tokens);
        assert_eq!(via_dialect.output_tokens, direct.output_tokens);
        assert_eq!(via_dialect.thinking_tokens, direct.thinking_tokens);
        assert_eq!(via_dialect.thinking_tokens, 130);
    }

    #[test]
    fn a_reply_with_reasoning_and_tool_calls_reads_the_same_whole_and_streamed() {
        let body = json!({
            "model": "qwen3-32b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "reasoning_content": "The user wants both files; read them in parallel.",
                    "content": "Reading both files.",
                    "tool_calls": [
                        {"id": "call_a", "type": "function",
                         "function": {"name": "fs-read", "arguments": "{\"path\":\"a.rs\"}"}},
                        {"id": "call_b", "type": "function",
                         "function": {"name": "fs-read", "arguments": "{\"path\":\"b.rs\"}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 40,
                "completion_tokens": 30,
                "completion_tokens_details": { "reasoning_tokens": 12 },
                "prompt_tokens_details": { "cached_tokens": 8 }
            }
        });
        let (whole, streamed) = both_paths::read(std::sync::Arc::new(StandardDialect), &body);

        assert_eq!(both_paths::content(&whole), both_paths::content(&streamed));
        assert_eq!(
            whole.thinking,
            "The user wants both files; read them in parallel."
        );
        assert_eq!(whole.text, "Reading both files.");
        assert_eq!(whole.tool_calls.len(), 2, "{:?}", whole.tool_calls);
        assert_eq!(whole.tool_calls[1].id, "call_b");
        assert_eq!(whole.tool_calls[1].arguments["path"], "b.rs");
        assert_eq!(whole.usage.thinking_tokens, 12);
    }

    #[test]
    fn a_whole_message_that_names_a_tool_call_without_an_id_is_refused_carrying_the_body() {
        let body = json!({
            "model": "m",
            "choices": [{
                "message": {"tool_calls": [{"function": {"name": "f", "arguments": "{}"}}]},
                "finish_reason": "tool_calls"
            }]
        });
        let err = StandardDialect.parse_response(&body).unwrap_err();
        assert!(
            matches!(&err, ProviderError::Parse { raw: Some(raw), .. } if raw.contains("\"f\"")),
            "{err:?}"
        );
    }

    #[test]
    fn whole_tool_call_arguments_that_are_not_an_object_are_the_models_malformed_call() {
        let body = json!({
            "model": "m",
            "choices": [{
                "message": {"tool_calls": [{"id": "c", "function": {"name": "f", "arguments": "[1]"}}]},
                "finish_reason": "tool_calls"
            }]
        });
        let err = StandardDialect.parse_response(&body).unwrap_err();
        assert!(
            matches!(err, ProviderError::MalformedToolCall { .. }),
            "{err:?}"
        );
    }
}
