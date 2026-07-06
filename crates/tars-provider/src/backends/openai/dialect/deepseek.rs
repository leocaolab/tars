//! `DeepSeekDialect` — the one genuine per-provider quirk (Doc 30, M1).
//!
//! DeepSeek's thinking toggle is its OWN openai-compat extension: a top-level
//! `thinking: {"type": "enabled"|"disabled"}` (what the openai client merges
//! from `extra_body`) — NOT the vLLM/Qwen `chat_template_kwargs.enable_thinking`
//! that the shared builder already emits. It maps the generic `req.thinking` so
//! deepseek-v4-flash (thinking-off by default) can be flipped on for a benchmark
//! and -pro turned off.
//!
//! This used to be an `if self.base_url.contains("deepseek")` branch inside the
//! shared `build_request` body. Moving it here keeps a stray `thinking` field
//! from ever reaching OpenAI proper (FR-1: no provider-name / base_url string
//! branch in the shared body builder) — only a provider whose dialect is
//! `DeepSeekDialect` emits it.

use serde_json::{Value, json};

use tars_types::{ChatRequest, ProviderError, ThinkingMode};

use super::OpenAiDialect;
use super::super::adapter::OpenAiAdapter;

/// DeepSeek (`api.deepseek.com` and openai_compat gateways fronting it).
///
/// Overrides only `build_request`: the standard body plus DeepSeek's
/// top-level `thinking: {type}`. Every other method keeps the default
/// (delegates to the shared adapter/mapping) — DeepSeek's SSE deltas and
/// usage are standard OpenAI shapes.
pub struct DeepSeekDialect;

impl OpenAiDialect for DeepSeekDialect {
    fn build_request(
        &self,
        adapter: &OpenAiAdapter,
        req: &ChatRequest,
    ) -> Result<Value, ProviderError> {
        // Standard body first — no logic duplicated from the adapter.
        let mut body = adapter.build_request_default(req)?;

        // Then DeepSeek's own thinking toggle.
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
    use tars_types::{Message, ModelHint, StructuredOutputMode};

    fn req(t: ThinkingMode) -> ChatRequest {
        ChatRequest {
            model: ModelHint::Explicit("deepseek-v4-flash".into()),
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

    /// E2E-1 (CUJ-2): `DeepSeekDialect::build_request` directly emits the
    /// top-level `thinking: {type}` field — Auto/Budget → enabled, Off →
    /// disabled — on top of the standard body, in isolation from any host.
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
            .build_request(&adapter, &req(ThinkingMode::Auto))
            .unwrap();
        assert_eq!(enabled["thinking"]["type"], "enabled");

        let budget = DeepSeekDialect
            .build_request(&adapter, &req(ThinkingMode::Budget(1024)))
            .unwrap();
        assert_eq!(budget["thinking"]["type"], "enabled");

        let disabled = DeepSeekDialect
            .build_request(&adapter, &req(ThinkingMode::Off))
            .unwrap();
        assert_eq!(disabled["thinking"]["type"], "disabled");

        // Standard body is preserved — it is the adapter default plus one field.
        assert_eq!(enabled["model"], "deepseek-v4-flash");
        assert_eq!(enabled["stream"], true);
    }
}
