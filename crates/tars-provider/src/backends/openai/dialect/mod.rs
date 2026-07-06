//! `OpenAiDialect` — the behavior seam for OpenAI-protocol variants.
//!
//! Every OpenAI-compatible endpoint (DeepSeek, Groq, LM Studio, vLLM, MLX,
//! llama.cpp, …) speaks the *same* wire protocol with small per-variant
//! quirks. Rather than fragmenting the shared adapter/mapping with
//! `if provider == deepseek` special-cases, each variant's behavior lives in
//! its own `impl OpenAiDialect`. See `docs/architecture/30-openai-dialect.md`.
//!
//! **M0 (this milestone):** the trait + [`StandardDialect`] only. Every
//! method has a **default** whose body *delegates* to the existing
//! adapter/mapping code — no logic is re-implemented, so `StandardDialect`
//! is byte-for-byte identical to today's behavior. The variant impls
//! (`DeepSeekDialect`, `LmStudioDialect`) are later milestones.

pub mod deepseek;
pub use deepseek::DeepSeekDialect;

use serde_json::Value;

use tars_types::{ChatEvent, ChatRequest, ChatResponse, ProviderError, Usage};

use crate::http_base::SseEvent;
use crate::tool_buffer::ToolCallBuffer;

use super::adapter::OpenAiAdapter;
use super::mapping::{openai_chat_completion_to_chat_response, parse_openai_usage};

/// Behavior-driven per-variant seam for the shared `openai` backend.
///
/// The default methods *are* standard OpenAI — each delegates to the
/// current adapter/mapping implementation, so a dialect that overrides
/// nothing behaves exactly like today's code. A variant overrides only the
/// quirk that differs (Open-Closed: the shared core is never reopened).
pub trait OpenAiDialect: Send + Sync {
    /// Canonical [`ChatRequest`] → provider wire JSON.
    ///
    /// Default = the standard OpenAI chat/completions body built by
    /// [`OpenAiAdapter::build_request_default`].
    fn build_request(
        &self,
        adapter: &OpenAiAdapter,
        req: &ChatRequest,
    ) -> Result<Value, ProviderError> {
        adapter.build_request_default(req)
    }

    /// One streaming SSE `data:` line → 0..N canonical [`ChatEvent`]s.
    ///
    /// Default = the standard delta/tool-call/finish parsing in
    /// [`OpenAiAdapter::parse_event_default`].
    fn parse_event(
        &self,
        adapter: &OpenAiAdapter,
        raw: &SseEvent,
        buf: &mut ToolCallBuffer,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        adapter.parse_event_default(raw, buf)
    }

    /// Provider `usage` object → canonical [`Usage`].
    ///
    /// Default = [`parse_openai_usage`] (reads `prompt_tokens`,
    /// `completion_tokens`, nested `cached_tokens`, and
    /// `completion_tokens_details.reasoning_tokens`). Used by the streaming
    /// finish path, so a dialect can reinterpret token accounting without
    /// rewriting the whole SSE loop.
    fn parse_usage(&self, usage: &serde_json::Map<String, Value>) -> Usage {
        parse_openai_usage(usage)
    }

    /// One non-streaming chat-completion body → canonical [`ChatResponse`]
    /// (the batch / one-shot path).
    ///
    /// Default = [`openai_chat_completion_to_chat_response`].
    fn parse_response(&self, raw: &Value) -> Result<ChatResponse, ProviderError> {
        openai_chat_completion_to_chat_response(raw)
    }
}

/// Standard OpenAI (and every openai_compat endpoint without a quirk).
/// All-defaults: it is exactly today's shared behavior.
pub struct StandardDialect;

impl OpenAiDialect for StandardDialect {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// E2E-2 (CUJ-3): a standard OpenAI chat-completion body parsed through
    /// `StandardDialect::parse_response` is byte-for-byte identical to the
    /// direct `openai_chat_completion_to_chat_response` path it delegates to.
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
        let direct = openai_chat_completion_to_chat_response(&body).unwrap();

        // Same canonical response — the seam changes nothing. (`Usage` has no
        // `PartialEq`, so compare its fields; `created` is a wall-clock stamp
        // set per-call, so it is intentionally not compared.)
        assert_eq!(via_dialect.text, direct.text);
        assert_eq!(via_dialect.text, "hello world");
        assert_eq!(via_dialect.stop_reason, direct.stop_reason);
        assert_eq!(via_dialect.usage.input_tokens, direct.usage.input_tokens);
        assert_eq!(via_dialect.usage.output_tokens, direct.usage.output_tokens);
        assert_eq!(via_dialect.usage.thinking_tokens, direct.usage.thinking_tokens);
        assert_eq!(
            via_dialect.usage.cached_input_tokens,
            direct.usage.cached_input_tokens
        );
        assert_eq!(via_dialect.usage.input_tokens, 12);
        assert_eq!(via_dialect.usage.output_tokens, 8);
        assert_eq!(via_dialect.usage.thinking_tokens, 3);
        assert_eq!(via_dialect.usage.cached_input_tokens, 4);
    }

    /// The `parse_usage` seam delegates to `parse_openai_usage` unchanged —
    /// including the generically-read `reasoning_tokens` (must not regress).
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
}
