//! Runs a coding-agent CLI (claude, codex, opencode, antigravity) as an [`LlmProvider`]: feeds it
//! the prompt inside an OS sandbox and turns its output into [`ChatEvent`]s. Everything that
//! differs between CLIs is behind a [`CliDialect`].
//!
//! A CLI is always sandboxed: an unset or `DangerFullAccess` policy becomes a workspace-write jail
//! on the worktree (else the process cwd); a `ReadOnly` or `WorkspaceWrite` policy is kept. codex,
//! opencode and antigravity run through [`SharedCliRunner`]; claude has its own
//! [`RealSubprocessRunner`] for its streaming output and its process-group teardown.
//!
//! It does not stream: every run is buffered and replayed as events.

pub(crate) mod claude_argv;
pub mod dialect;
pub mod dialects;
pub(crate) mod invocation;
pub(crate) mod prompt;
mod stream_json_log;
mod subprocess;
pub(crate) mod system_prompt_file;

use std::sync::Arc;

use async_trait::async_trait;

use crate::ProviderError;
use tars_types::{Caller, ChatEvent, ChatRequest, ProviderId, ProviderProfile, StopReason};

use crate::provider::wire::{SpawnedProcess, Transport, WireOutcome, WireRecorder, failed};
use crate::provider::{LlmEventStream, LlmProvider};

pub(crate) use claude_argv::is_stripped_env;
pub use claude_argv::{ClaudeCliEffort, ClaudeToolAccess};
pub use dialect::{CliDialect, OutputFraming, OutputMode, PromptChannel};
pub use dialects::antigravity::{AntigravityDialect, AntigravityEffort};
pub use dialects::claude::ClaudeCliDialect;
pub use dialects::codex::{CodexCliDialect, CodexSandbox};
pub use dialects::opencode::OpenCodeDialect;
pub use invocation::{ChildIo, RunFailure, RunOutput, SubprocessInvocation, SubprocessRunner};
pub use subprocess::{RealSubprocessRunner, SharedCliRunner};
pub use system_prompt_file::SystemPromptFile;

/// Returns the error of a CLI that died, with its exit code and stderr.
pub(crate) fn cli_subprocess_died(exit_code: Option<i32>, stderr: String) -> ProviderError {
    ProviderError::CliSubprocessDied { exit_code, stderr }
}

/// A coding-agent CLI as a provider: the dialect says how to talk to it, the runner spawns it.
pub struct AgentCliBackend {
    id: ProviderId,
    capabilities: ProviderProfile,
    dialect: Arc<dyn CliDialect>,
    runner: Arc<dyn SubprocessRunner>,
    wire: Option<WireRecorder>,
}

impl AgentCliBackend {
    pub fn new(
        id: ProviderId,
        capabilities: ProviderProfile,
        dialect: Arc<dyn CliDialect>,
        runner: Arc<dyn SubprocessRunner>,
    ) -> Self {
        Self {
            id,
            capabilities,
            dialect,
            runner,
            wire: None,
        }
    }

    /// Sets where each run's exchanged bytes are recorded; `None` records none.
    pub fn wire(mut self, wire: Option<WireRecorder>) -> Self {
        self.wire = wire;
        self
    }
}

#[async_trait]
impl LlmProvider for AgentCliBackend {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }

    // `err(Display)` logs each failed call once, with the provider id and model.
    #[tracing::instrument(
        name = "agent_cli.stream",
        skip_all,
        fields(provider = %self.id, model = %model),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        let inv = self.dialect.invocation(&req, model, &ctx)?;
        let model = inv.model.clone();

        // Begun before the run, so a run the caller drops is recorded as
        // cancelled; what was sent is known once the runner returns.
        let mut attempt = self.wire.as_ref().and_then(|w| {
            w.begin(
                &ctx,
                &self.id,
                &model,
                Transport::Subprocess { process: None },
                bytes::Bytes::new(),
            )
        });
        let (run, exchanged) = match self.runner.run(inv).await {
            Ok(out) => (Ok(out.payload), out.exchanged),
            Err(failure) => (Err(failure.error), failure.exchanged),
        };
        if let Some(attempt) = attempt.as_mut() {
            // No argv ⇔ never spawned: no process, so no stderr either.
            if let Some(argv) = exchanged.argv {
                attempt.spawned(SpawnedProcess {
                    argv,
                    stderr: exchanged.stderr,
                });
            }
            attempt.sent(exchanged.stdin);
            attempt.received().push(&exchanged.stdout);
        }
        let content = run.and_then(|payload| self.content_of(&payload));
        if let Some(attempt) = attempt {
            attempt.end(match &content {
                Ok(_) => WireOutcome::Completed,
                Err(e) => failed(e),
            });
        }
        let content = content?;

        // These CLIs take no output-token limit, so the reply is cut afterwards.
        let content = clamp_to_output_budget(content, req.max_output_tokens);

        let mut events: Vec<Result<ChatEvent, ProviderError>> =
            Vec::with_capacity(content.len() + 1);
        events.push(Ok(ChatEvent::started(model)));
        events.extend(content.into_iter().map(Ok));

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

impl AgentCliBackend {
    /// Returns the run's payload as the dialect reads it.
    fn content_of(&self, payload: &serde_json::Value) -> Result<Vec<ChatEvent>, ProviderError> {
        Ok(match self.dialect.output_mode() {
            OutputMode::JsonEvents => self.dialect.parse_line(payload)?,
            OutputMode::Text => {
                // A `Text` dialect's runner returns stdout as a JSON string.
                let stdout = payload.as_str().ok_or_else(|| {
                    ProviderError::Internal(format!(
                        "AgentCliBackend: OutputMode::Text runner must return a JSON string of \
                         raw stdout, got: {}",
                        crate::provider::http_base::truncate_utf8(&payload.to_string(), 200)
                    ))
                })?;
                self.dialect.parse_text(stdout)?
            }
        })
    }
}

/// Cuts the reply's text to [`tars_types::BYTES_PER_TOKEN`] bytes per token of
/// `max_output_tokens`, at a char boundary, dropping the deltas past the budget; when it cut
/// anything, `Finished` says `MaxTokens`, so a cut reply does not look like a natural end.
fn clamp_to_output_budget(
    content: Vec<ChatEvent>,
    max_output_tokens: Option<u32>,
) -> Vec<ChatEvent> {
    let Some(cap) =
        max_output_tokens.map(|t| (t as usize).saturating_mul(tars_types::BYTES_PER_TOKEN))
    else {
        return content;
    };

    let mut left = cap;
    let mut truncated = false;
    let mut kept: Vec<ChatEvent> = content
        .into_iter()
        .filter_map(|ev| match ev {
            ChatEvent::Delta { text } => {
                let cut = crate::provider::http_base::truncate_utf8(&text, left);
                left -= cut.len();
                if cut.len() == text.len() {
                    return Some(ChatEvent::Delta { text });
                }
                truncated = true;
                (!cut.is_empty()).then(|| ChatEvent::Delta {
                    text: cut.to_string(),
                })
            }
            other => Some(other),
        })
        .collect();

    if truncated {
        for ev in &mut kept {
            if let ChatEvent::Finished { stop_reason, .. } = ev {
                *stop_reason = StopReason::MaxTokens;
            }
        }
    }

    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tars_types::Usage;

    /// Records the invocation and returns a canned payload.
    struct FakeRunner {
        payload: Value,
        recorded: std::sync::Mutex<Option<SubprocessInvocation>>,
    }

    #[async_trait]
    impl SubprocessRunner for FakeRunner {
        async fn run(&self, inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
            *self.recorded.lock().unwrap() = Some(inv);
            Ok(RunOutput {
                payload: self.payload.clone(),
                exchanged: Default::default(),
            })
        }
    }

    fn claude_backend(payload: Value) -> (Arc<AgentCliBackend>, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            payload,
            recorded: std::sync::Mutex::new(None),
        });
        let dialect = Arc::new(ClaudeCliDialect::new(
            "claude".into(),
            std::time::Duration::from_secs(300),
            ClaudeToolAccess::Disabled,
            false,
            None,
            true,
            Vec::new(),
        ));
        let caps = ProviderProfile::text_only_baseline(tars_types::Pricing::default());
        let backend = Arc::new(AgentCliBackend::new(
            ProviderId::new("agent_cli_test"),
            caps,
            dialect,
            runner.clone(),
        ));
        (backend, runner)
    }

    #[tokio::test]
    async fn claude_dialect_through_backend_emits_started_delta_finished() {
        let payload = json!({
            "result": "hello from claude",
            "is_error": false,
            "usage": { "input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 3 }
        });
        let (backend, runner) = claude_backend(payload);

        use futures::StreamExt;
        let events: Vec<ChatEvent> = Arc::clone(&backend)
            .stream(ChatRequest::user("hi"), "opus", Caller::test_default())
            .await
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
            .await;

        assert!(
            matches!(&events[0], ChatEvent::Started { actual_model, .. } if actual_model.as_deref() == Some("opus"))
        );
        assert!(matches!(&events[1], ChatEvent::Delta { text } if text == "hello from claude"));
        match &events[2] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                // Anthropic's 12 excludes the 3 cache reads; tars counts 15.
                assert_eq!(usage.unwrap().input_tokens, 15);
                assert_eq!(usage.unwrap().output_tokens, 5);
                assert_eq!(usage.unwrap().cached_input_tokens, 3);
            }
            other => panic!("expected Finished, got {other:?}"),
        }

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "opus");
        let argv = backend.dialect.argv(&inv);
        assert_eq!(argv[0], "-p");
        assert!(argv.iter().any(|a| a == "--disable-slash-commands"));
    }

    #[tokio::test]
    async fn budget_clamp_truncates_delta_and_flips_stop_reason() {
        let big = "x".repeat(1000);
        let (backend, _) = claude_backend(json!({"result": big, "is_error": false}));

        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(10); // 40 bytes

        let resp = backend
            .complete(req, "test-model", Caller::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text.len(), 40);
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn the_budget_bounds_the_whole_reply_not_each_delta() {
        let delta = |t: &str| ChatEvent::Delta { text: t.into() };
        let content = vec![
            delta("abc"),
            delta("def"),
            delta("ghi"),
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage::default()),
            },
        ];
        let out = clamp_to_output_budget(content, Some(1));
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "abcd");
        assert!(
            matches!(out.last(), Some(ChatEvent::Finished { stop_reason, .. }) if *stop_reason == StopReason::MaxTokens)
        );
    }

    #[test]
    fn clamp_is_noop_without_budget() {
        let content = vec![
            ChatEvent::Delta {
                text: "x".repeat(100),
            },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage::default()),
            },
        ];
        let out = clamp_to_output_budget(content, None);
        assert!(matches!(&out[0], ChatEvent::Delta { text } if text.len() == 100));
        assert!(
            matches!(&out[1], ChatEvent::Finished { stop_reason, .. } if *stop_reason == StopReason::EndTurn)
        );
    }
}
