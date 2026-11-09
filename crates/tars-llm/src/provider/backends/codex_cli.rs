//! Builds the OpenAI Codex CLI provider, which calls `codex exec` on the user's ChatGPT
//! subscription: the API-key variables are stripped from the child so codex uses its own login.

use std::sync::Arc;
use std::time::Duration;

use tars_types::{ProviderId, ProviderProfile};

use crate::provider::backends::cli::{AgentCliBackend, CodexCliDialect, SharedCliRunner};
use crate::provider::wire::WireRecorder;

pub use crate::provider::backends::cli::{CodexSandbox, SubprocessInvocation, SubprocessRunner};

#[derive(Clone, Debug)]
pub struct CodexCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    sandbox: CodexSandbox,
    capabilities: Option<ProviderProfile>,
    skip_git_repo_check: bool,
    wire: Option<WireRecorder>,
}

impl CodexCliProviderBuilder {
    pub fn new(id: ProviderId) -> Self {
        Self {
            id,
            executable: tars_config::providers::CODEX_CLI_EXECUTABLE.to_string(),
            timeout: Duration::from_secs(tars_config::providers::CODEX_CLI_TIMEOUT_SECS),
            sandbox: CodexSandbox::ReadOnly,
            capabilities: None,
            skip_git_repo_check: tars_config::providers::CODEX_CLI_SKIP_GIT_REPO_CHECK,
            wire: None,
        }
    }

    builder_setter!(executable: into String);
    builder_setter!(timeout: Duration);
    builder_setter!(sandbox: CodexSandbox);
    builder_setter!(skip_git_repo_check: bool);
    builder_setter!(capabilities: opt ProviderProfile);
    builder_setter! {
        /// Sets where each run's exchanged bytes are recorded; `None` records none.
        wire: Option<WireRecorder>
    }

    /// Returns the provider, spawning the real `codex` through [`SharedCliRunner`].
    pub fn build(self) -> Arc<AgentCliBackend> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let dialect = Arc::new(CodexCliDialect::new(
            self.executable,
            self.timeout,
            self.sandbox,
            self.skip_git_repo_check,
        ));
        let runner = Arc::new(SharedCliRunner::new(dialect.clone()));
        Arc::new(AgentCliBackend::new(self.id, caps, dialect, runner).wire(self.wire))
    }

    /// Returns the provider with `runner` in place of the real one.
    pub fn build_with_runner(self, runner: Arc<dyn SubprocessRunner>) -> Arc<AgentCliBackend> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let dialect = Arc::new(CodexCliDialect::new(
            self.executable,
            self.timeout,
            self.sandbox,
            self.skip_git_repo_check,
        ));
        Arc::new(AgentCliBackend::new(self.id, caps, dialect, runner).wire(self.wire))
    }
}

/// Returns the capabilities of the `codex_cli` block of `data/provider.toml`, which says no
/// streaming and no cancel, as `AgentCliBackend` buffers every run.
fn default_capabilities() -> ProviderProfile {
    tars_config::capabilities_for("codex_cli", "")
}

pub fn codex_cli(id: ProviderId) -> Arc<AgentCliBackend> {
    CodexCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;
    use async_trait::async_trait;
    use serde_json::Value;
    use tars_types::{Caller, ChatRequest};

    use crate::provider::LlmProvider;
    use crate::provider::backends::cli::{RunFailure, RunOutput};

    /// A runner that answers with the lines framed as the shared runner frames codex's output,
    /// and records the invocation.
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
        async fn run(&self, invocation: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
            *self.recorded.lock().unwrap() = Some(invocation);
            let arr = self
                .lines
                .iter()
                .map(|l| Value::String(l.clone()))
                .collect();
            Ok(RunOutput {
                payload: Value::Array(arr),
                exchanged: Default::default(),
            })
        }
    }

    fn make_provider(lines: Vec<&str>) -> (Arc<AgentCliBackend>, Arc<FakeRunner>) {
        let runner = FakeRunner::new(lines);
        let provider = CodexCliProviderBuilder::new(ProviderId::new("codex_cli_test"))
            .build_with_runner(runner.clone());
        (provider, runner)
    }

    #[tokio::test]
    async fn end_to_end_stream_yields_started_delta_finished() {
        let (provider, runner) = make_provider(vec![
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"hi"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":0}}"#,
        ]);
        let response = provider
            .complete(ChatRequest::user("say hi"), "gpt-5", Caller::test_default())
            .await
            .unwrap();
        assert_eq!(response.text, "hi");
        assert_eq!(response.usage.unwrap().input_tokens, 10);
        assert_eq!(response.usage.unwrap().output_tokens, 2);

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "gpt-5");
        for k in ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AGENT_IDENTITY"] {
            assert!(
                inv.stripped_env.contains(k),
                "stripped_env must contain {k}"
            );
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
            .complete(ChatRequest::user("x"), "gpt-5", Caller::test_default())
            .await
            .unwrap();
        assert_eq!(response.text, "answer");
        assert_eq!(response.usage.unwrap().thinking_tokens, 5);
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
            .complete(ChatRequest::user("x"), "gpt-5", Caller::test_default())
            .await
            .unwrap();
        assert_eq!(response.text, "done");
    }

    #[tokio::test]
    async fn end_to_end_turn_failed_surfaces_provider_error() {
        let (provider, _) = make_provider(vec![
            r#"{"type":"turn.failed","error":{"message":"context too long"}}"#,
        ]);
        let result = provider
            .complete(ChatRequest::user("x"), "gpt-5", Caller::test_default())
            .await;
        match result {
            Err(ProviderError::CliSubprocessDied { stderr, .. }) => {
                assert!(stderr.contains("context too long"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }
}
