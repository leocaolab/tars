//! OpenAI Codex CLI as an LLM Provider — ChatGPT-subscription path.
//!
//! Since Doc 32 M1 this module is the **codex construction surface** on top of
//! the shared CLI-delegate machinery in [`crate::backends::cli`]. It shells out
//! to `codex exec --json --model X --sandbox <mode> -c approval_policy="never"
//! [--skip-git-repo-check] -`, feeds the prompt on stdin, strips the
//! API-billing env vars so codex falls through to the ChatGPT OAuth path, and
//! maps codex's JSONL `ThreadEvent`s onto canonical `ChatEvent`s.
//!
//! ## What M1 changed
//!
//! codex used to re-invent its **own** spawn/stream loop
//! (`codex_cli.rs:253-342`) — a third private subprocess path with no
//! `tars-sandbox`. That private spawn is **retired**. The runtime provider is
//! now the shared [`AgentCliBackend`](crate::backends::cli::AgentCliBackend)
//! driven by a [`CodexCliDialect`](crate::backends::cli::CodexCliDialect) and the
//! shared [`SharedCliRunner`](crate::backends::cli::SharedCliRunner), which
//! spawns through the shared `tars-sandbox` OS-jail primitive. codex keeps its
//! OWN `--sandbox` flag (its internal jail) and now ALSO runs inside the
//! tars-sandbox process jail — defense-in-depth (Doc 29 / FR-3).
//!
//! The codex JSONL is buffered by the runner and mapped per-line by
//! [`CodexCliDialect::parse_line`], so the `agent_message`/`reasoning`/
//! `turn.completed` → `ChatEvent` translation is byte-for-byte; the eager
//! `AgentCliBackend` emits the events after the turn completes rather than
//! live (fine for the spawn-per-call consumer).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tars_types::{
    Capabilities, Modality, PromptCacheKind, ProviderId, StructuredOutputMode,
};

use crate::backends::cli::{AgentCliBackend, CodexCliDialect, SharedCliRunner};

// Re-export the shared runner trait/invocation + the codex sandbox enum under
// the historical `backends::codex_cli::…` paths that `registry.rs` imports.
pub use crate::backends::cli::{SandboxMode, SubprocessInvocation, SubprocessRunner};

/// The codex runtime provider is the shared [`AgentCliBackend`]. The alias
/// preserves the `tars_provider::backends::codex_cli::CodexCliProvider` path.
pub type CodexCliProvider = AgentCliBackend;

#[derive(Clone, Debug)]
pub struct CodexCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    sandbox: SandboxMode,
    capabilities: Option<Capabilities>,
    skip_git_repo_check: bool,
}

impl CodexCliProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            executable: "codex".to_string(),
            // codex exec runs a full agent loop — 10 min default is generous
            // but not crazy. Tune per deployment.
            timeout: Duration::from_secs(600),
            sandbox: SandboxMode::ReadOnly,
            capabilities: None,
            // Default true: TARS workers may operate outside a git repo
            // (tempdir tests, scratch files), and codex's git-repo gate would
            // refuse with confusing error text.
            skip_git_repo_check: true,
        }
    }

    builder_setter!(executable: into String);
    builder_setter!(timeout: Duration);
    builder_setter!(sandbox: SandboxMode);
    builder_setter!(skip_git_repo_check: bool);
    builder_setter!(capabilities: opt Capabilities);

    /// Build with the shared buffered runner
    /// ([`SharedCliRunner`](crate::backends::cli::SharedCliRunner)) — spawns
    /// through the OS-jail primitive and frames codex's JSONL event stream.
    pub fn build(self) -> Arc<CodexCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let dialect = Arc::new(CodexCliDialect::new(
            self.executable,
            self.timeout,
            self.sandbox,
            self.skip_git_repo_check,
        ));
        let runner = Arc::new(SharedCliRunner::new(dialect.clone()));
        Arc::new(AgentCliBackend::new(self.id, caps, dialect, runner))
    }

    /// Build with a substituted runner — for tests (FakeRunner).
    pub fn build_with_runner(self, runner: Arc<dyn SubprocessRunner>) -> Arc<CodexCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let dialect = Arc::new(CodexCliDialect::new(
            self.executable,
            self.timeout,
            self.sandbox,
            self.skip_git_repo_check,
        ));
        Arc::new(AgentCliBackend::new(self.id, caps, dialect, runner))
    }
}

fn default_capabilities() -> Capabilities {
    let mut text = HashSet::new();
    text.insert(Modality::Text);
    Capabilities {
        // ChatGPT 5 / 5-codex is roughly 200K context.
        max_context_tokens: 200_000,
        // CLI doesn't expose --max-output-tokens; the backend post-truncates.
        max_output_tokens: 64_000,
        // The CLI surfaces tool calls as ThreadItem events (codex's INTERNAL
        // tools). They're NOT exposed back to TARS as our `ToolCall` shape —
        // codex dispatches them itself.
        supports_tool_use: false,
        supports_parallel_tool_calls: false,
        supports_structured_output: StructuredOutputMode::None,
        supports_vision: false,
        supports_thinking: true,
        // Buffered under `AgentCliBackend`: the runner drains codex's JSONL
        // to completion into a `Value::Array`, then the backend emits all
        // ChatEvents after the turn ends — nothing is delivered mid-run, so
        // there is nothing to cancel mid-call (cancel is via Drop only) and
        // no incremental token delivery. Advertise both honestly as false.
        supports_cancel: false,
        prompt_cache: PromptCacheKind::Delegated,
        streaming: false,
        modalities_in: text.clone(),
        modalities_out: text,
        // Subscription-billed; per-token pricing N/A here.
        pricing: tars_types::Pricing::default(),
    }
}

/// Convenience builder.
pub fn codex_cli(id: impl Into<ProviderId>) -> Arc<CodexCliProvider> {
    CodexCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;
    use tars_types::{ChatRequest, ModelHint, ProviderError, RequestContext};

    use crate::provider::LlmProvider;

    /// A FakeRunner that returns the canned JSONL lines as the `Value::Array`
    /// the shared runner produces for `JsonLinesArray` framing, and records the
    /// invocation it received.
    struct FakeRunner {
        lines: Vec<String>,
        recorded: std::sync::Mutex<Option<SubprocessInvocation>>,
    }

    impl FakeRunner {
        fn new(lines: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                lines: lines.into_iter().map(String::from).collect(),
                recorded: std::sync::Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl SubprocessRunner for FakeRunner {
        async fn run(&self, invocation: SubprocessInvocation) -> Result<Value, ProviderError> {
            *self.recorded.lock().unwrap() = Some(invocation);
            let arr = self.lines.iter().map(|l| Value::String(l.clone())).collect();
            Ok(Value::Array(arr))
        }
    }

    fn make_provider(lines: Vec<&str>) -> (Arc<CodexCliProvider>, Arc<FakeRunner>) {
        let runner = FakeRunner::new(lines);
        let provider =
            CodexCliProviderBuilder::new("codex_cli_test").build_with_runner(runner.clone());
        (provider, runner)
    }

    /// E2E-1 (FR-5): codex through `AgentCliBackend` + `CodexCliDialect`
    /// yields Started → Delta → Finished with the mapped usage.
    #[tokio::test]
    async fn end_to_end_stream_yields_started_delta_finished() {
        let (provider, runner) = make_provider(vec![
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"hi"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":0}}"#,
        ]);
        let response = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gpt-5".into()), "say hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(response.text, "hi");
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 2);
        assert!(response.is_finished());

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "gpt-5");
        for k in ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AGENT_IDENTITY"] {
            assert!(inv.stripped_env.contains(k), "stripped_env must contain {k}");
        }
    }

    #[tokio::test]
    async fn end_to_end_separates_thinking_from_text() {
        let (provider, _) = make_provider(vec![
            r#"{"type":"item.completed","item":{"id":"i1","type":"reasoning","text":"thinking…"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"answer"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":5}}"#,
        ]);
        let response = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gpt-5".into()), "x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(response.text, "answer");
        assert_eq!(response.usage.thinking_tokens, 5);
    }

    #[tokio::test]
    async fn end_to_end_drops_internal_tool_events_and_garbage() {
        let (provider, _) = make_provider(vec![
            "",
            "this is not json",
            r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"ls","aggregated_output":"foo","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"a1","type":"agent_message","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}"#,
        ]);
        let response = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gpt-5".into()), "x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(response.text, "done");
    }

    #[tokio::test]
    async fn end_to_end_turn_failed_surfaces_provider_error() {
        let (provider, _) =
            make_provider(vec![r#"{"type":"turn.failed","error":{"message":"context too long"}}"#]);
        let result = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gpt-5".into()), "x"),
                RequestContext::test_default(),
            )
            .await;
        match result {
            Err(ProviderError::CliSubprocessDied { stderr, .. }) => {
                assert!(stderr.contains("context too long"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }
}
