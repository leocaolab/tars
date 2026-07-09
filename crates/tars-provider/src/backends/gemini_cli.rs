//! Gemini CLI as an LLM Provider — subscription path.
//!
//! Since Doc 32 M1 this module is the **gemini construction surface** on top of
//! the shared CLI-delegate machinery in [`crate::backends::cli`]. It shells out
//! to the user-installed `gemini` binary
//! (`gemini -p "<prompt>" -m <model> -o json`), strips the API-mode-trigger env
//! vars so the subscription path stays active, and maps the buffered JSON
//! payload onto canonical `ChatEvent`s.
//!
//! ## What M1 changed (the security fix)
//!
//! gemini used to spawn through its OWN `RealSubprocessRunner` — a bare
//! `Command::new` with **no sandbox**, i.e. an unconfined black-box agent
//! (tracking doc §2). That private runner is **retired**. The runtime provider
//! is now the shared [`AgentCliBackend`](crate::backends::cli::AgentCliBackend)
//! driven by a [`GeminiCliDialect`](crate::backends::cli::GeminiCliDialect) and
//! the shared [`SharedCliRunner`](crate::backends::cli::SharedCliRunner), which
//! spawns through the shared `tars-sandbox` OS-jail primitive. gemini now gets
//! the same write-jail as claude (Doc 29 / FR-3) — confined by default, no env
//! gate required.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tars_types::{
    Capabilities, Modality, PromptCacheKind, ProviderId, StructuredOutputMode,
};

use crate::backends::cli::{AgentCliBackend, GeminiCliDialect, SharedCliRunner};

// Re-export the shared runner trait/invocation under the historical
// `backends::gemini_cli::…` paths (`lib.rs` re-exports `SubprocessRunner` as
// `GeminiCliSubprocessRunner`; `build_with_runner` takes it).
pub use crate::backends::cli::{SubprocessInvocation, SubprocessRunner};

/// The gemini runtime provider is the shared [`AgentCliBackend`]. The alias
/// preserves the `tars_provider::GeminiCliProvider` re-export.
pub type GeminiCliProvider = AgentCliBackend;

#[derive(Clone, Debug)]
pub struct GeminiCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Option<Capabilities>,
}

impl GeminiCliProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            executable: "gemini".to_string(),
            timeout: Duration::from_secs(300),
            capabilities: None,
        }
    }

    builder_setter!(executable: into String);
    builder_setter!(timeout: Duration);
    builder_setter!(capabilities: opt Capabilities);

    /// Build with the shared buffered runner
    /// ([`SharedCliRunner`](crate::backends::cli::SharedCliRunner)) — spawns
    /// through the OS-jail primitive and frames gemini's single-object JSON.
    pub fn build(self) -> Arc<GeminiCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let dialect = Arc::new(GeminiCliDialect::new(self.executable, self.timeout));
        let runner = Arc::new(SharedCliRunner::new(dialect.clone()));
        Arc::new(AgentCliBackend::new(self.id, caps, dialect, runner))
    }

    /// Build with a substituted runner — for tests (FakeRunner).
    pub fn build_with_runner(self, runner: Arc<dyn SubprocessRunner>) -> Arc<GeminiCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let dialect = Arc::new(GeminiCliDialect::new(self.executable, self.timeout));
        Arc::new(AgentCliBackend::new(self.id, caps, dialect, runner))
    }
}

fn default_capabilities() -> Capabilities {
    let mut text = HashSet::new();
    text.insert(Modality::Text);
    Capabilities {
        max_context_tokens: 1_048_576, // Gemini 2.5+ class
        max_output_tokens: 8_192,
        supports_tool_use: false, // CLI -p mode doesn't expose function calling
        supports_parallel_tool_calls: false,
        supports_structured_output: StructuredOutputMode::None,
        supports_vision: false,
        supports_thinking: false,
        supports_cancel: false, // spawn-per-call mode
        prompt_cache: PromptCacheKind::Delegated,
        streaming: false,
        modalities_in: text.clone(),
        modalities_out: text,
        pricing: tars_types::Pricing::default(),
    }
}

/// Convenience builder.
pub fn gemini_cli(id: impl Into<ProviderId>) -> Arc<GeminiCliProvider> {
    GeminiCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tars_types::{ChatEvent, ChatRequest, ModelHint, ProviderError, RequestContext, StopReason};

    use crate::provider::LlmProvider;

    /// Records the invocation and returns a canned gemini JSON payload — the
    /// FakeRunner pattern, now over the shared `SubprocessRunner`.
    struct FakeRunner {
        payload: Value,
        recorded: std::sync::Mutex<Option<SubprocessInvocation>>,
    }

    #[async_trait]
    impl SubprocessRunner for FakeRunner {
        async fn run(&self, invocation: SubprocessInvocation) -> Result<Value, ProviderError> {
            *self.recorded.lock().unwrap() = Some(invocation);
            Ok(self.payload.clone())
        }
    }

    fn make_provider(payload: Value) -> (Arc<GeminiCliProvider>, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            payload,
            recorded: std::sync::Mutex::new(None),
        });
        let p = GeminiCliProviderBuilder::new("gemini_cli_test").build_with_runner(runner.clone());
        (p, runner)
    }

    /// E2E-1 (FR-5): gemini through `AgentCliBackend` + `GeminiCliDialect`
    /// produces the same Started → Delta → Finished stream + usage as the
    /// pre-migration provider.
    #[tokio::test]
    async fn happy_path_returns_text_and_usage() {
        let payload = json!({
            "session_id": "abc",
            "response": "Hello there!",
            "stats": { "models": { "gemini-2.5-flash": {
                "tokens": { "prompt": 50, "candidates": 4, "cached": 10, "thoughts": 7 }
            } } }
        });
        let (provider, runner) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user("say hi"),
                "gemini-2.5-flash", RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "Hello there!");
        assert_eq!(resp.usage.input_tokens, 50);
        assert_eq!(resp.usage.output_tokens, 4);
        assert_eq!(resp.usage.cached_input_tokens, 10);
        assert_eq!(resp.usage.thinking_tokens, 7);

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "gemini-2.5-flash");
        assert!(inv.stripped_env.contains("GEMINI_API_KEY"));
        assert!(inv.stripped_env.contains("GOOGLE_API_KEY"));
        assert!(inv.stripped_env.contains("GOOGLE_APPLICATION_CREDENTIALS"));
    }

    #[tokio::test]
    async fn missing_response_field_yields_empty_text() {
        let (provider, _) = make_provider(json!({"session_id": "x", "stats": {}}));
        let resp = provider
            .complete(
                ChatRequest::user("hi"),
                "gemini-2.5-flash", RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "");
    }

    #[tokio::test]
    async fn oversized_prompt_rejected_with_invalid_request() {
        let (provider, _) = make_provider(json!({"response": "should never run"}));
        let big = "x".repeat(super::super::cli::dialects::gemini::MAX_PROMPT_BYTES + 1);
        let err = provider
            .complete(
                ChatRequest::user(big),
                "gemini-2.5-flash", RequestContext::test_default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn truncates_when_max_output_tokens_exceeded() {
        let big = "x".repeat(1000);
        let (provider, _) = make_provider(json!({"response": big}));
        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(10); // → 40 chars
        let resp = provider
            .complete(req, "gemini-2.5-flash", RequestContext::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text.len(), 40);
        // Backend clamp flips the stop reason when WE truncated (consistent
        // with claude; the pre-migration provider left it EndTurn).
        assert_eq!(resp.stop_reason, Some(StopReason::MaxTokens));
    }

    #[tokio::test]
    async fn system_message_is_embedded_as_prefix_block() {
        let (provider, runner) = make_provider(json!({"response": "ok"}));
        let _ = provider
            .complete(
                ChatRequest::user("x")
                    .with_system("be precise"),
                "gemini-2.5-flash", RequestContext::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert!(inv.prompt.starts_with("[system]\nbe precise"));
        assert!(inv.prompt.contains("[user]\nx"));
    }

    #[tokio::test]
    async fn emits_started_delta_finished_in_order() {
        let (provider, _) = make_provider(json!({"response": "hi"}));
        use futures::StreamExt;
        let events: Vec<ChatEvent> = Arc::clone(&provider)
            .stream(
                ChatRequest::user("hi"),
                "gemini-2.5-flash", RequestContext::test_default(),
            )
            .await
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
            .await;
        assert!(matches!(&events[0], ChatEvent::Started { actual_model, .. } if actual_model == "gemini-2.5-flash"));
        assert!(matches!(&events[1], ChatEvent::Delta { text } if text == "hi"));
        assert!(matches!(&events[2], ChatEvent::Finished { .. }));
    }
}
