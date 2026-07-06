//! Shared CLI-delegate backend (Doc 32 §5 C2).
//!
//! [`AgentCliBackend`] is the ONE `LlmProvider` that shells out to a
//! black-box coding-agent CLI, feeds it a prompt, OS-sandboxes the spawn
//! (`tars-sandbox`, Doc 29), and maps the CLI's output onto canonical
//! [`ChatEvent`]s. Everything that varies per CLI lives behind a
//! [`CliDialect`]; the backend itself contains **no per-CLI branching**
//! (FR-1). Adding a CLI = one `CliDialect` impl.
//!
//! ## What is shared here (lifted from `claude_cli`, Doc 32 §7)
//! - [`argv`] — [`SubprocessInvocation`] + [`SubprocessRunner`] + the argv
//!   constructors + the env-strip table.
//! - [`subprocess`] — [`RealSubprocessRunner`]: the real spawn, the
//!   `tars-sandbox` write-jail (gated on `TARS_CLAUDE_SANDBOX`), the
//!   TMPDIR-in-worktree redirect, the stdin prompt, and the buffered JSON
//!   parse.
//! - [`streaming`] — the stream-json NDJSON line-drain.
//!
//! ## Seam boundaries (as-built)
//! The OS-jail wrap is a **shared primitive** (`build_sandboxed_command` in
//! [`subprocess`]). A delegate is confined **by default** (Doc 32 FR-3): an
//! unset/`DangerFullAccess` policy is downgraded to a workspace-write jail on
//! the worktree cwd (else the process cwd), and an explicit `[sandbox]`/
//! `--sandbox` `ReadOnly`/`WorkspaceWrite` policy is honored. The legacy
//! `TARS_CLAUDE_SANDBOX` env gate is no longer needed (nor read).
//!
//! The 5 near-duplicate per-CLI runners the earlier as-built gap booked (Doc 32
//! §9) are **consolidated**: gemini / codex / opencode / antigravity now share
//! ONE [`SharedCliRunner`](subprocess::SharedCliRunner) — a single
//! spawn/prompt-channel/drain skeleton parameterized by the dialect's declared
//! [`OutputFraming`] (single-object / prefix-stripped / JSONL→array / raw-text).
//! A new buffered CLI = a `CliDialect` (argv + parse + declared framing), **no
//! bespoke runner** (FR-6). claude keeps its own
//! [`RealSubprocessRunner`](subprocess::RealSubprocessRunner) because its
//! `stream-json` NDJSON path ([`streaming`]) + child-reaper / process-group
//! teardown are genuinely different; the `security_delegate_cli` test drives it.

pub(crate) mod argv;
pub mod dialect;
pub mod dialects;
mod streaming;
mod subprocess;

use std::sync::Arc;

use async_trait::async_trait;

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ProviderError, ProviderId, RequestContext, StopReason,
};

use crate::provider::{LlmEventStream, LlmProvider};

pub use argv::{ClaudeCliEffort, ClaudeCliTools, SubprocessInvocation, SubprocessRunner};
pub use dialect::{CliDialect, CliInvocation, OutputFraming, OutputMode, PromptChannel};
pub use dialects::antigravity::AntigravityDialect;
pub use dialects::claude::ClaudeCliDialect;
pub use dialects::codex::{CodexCliDialect, SandboxMode};
pub use dialects::gemini::GeminiCliDialect;
pub use dialects::opencode::OpenCodeDialect;
pub use subprocess::{RealSubprocessRunner, SharedCliRunner};

/// The shared CLI-delegate provider. Holds the per-CLI behavior
/// ([`CliDialect`]) and the spawn machinery ([`SubprocessRunner`]);
/// everything CLI-specific is behind the dialect.
pub struct AgentCliBackend {
    id: ProviderId,
    capabilities: Capabilities,
    dialect: Arc<dyn CliDialect>,
    runner: Arc<dyn SubprocessRunner>,
}

impl AgentCliBackend {
    pub fn new(
        id: ProviderId,
        capabilities: Capabilities,
        dialect: Arc<dyn CliDialect>,
        runner: Arc<dyn SubprocessRunner>,
    ) -> Self {
        Self {
            id,
            capabilities,
            dialect,
            runner,
        }
    }
}

#[async_trait]
impl LlmProvider for AgentCliBackend {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    // Boundary log — Err exits auto-emit with provider/model context
    // (mirrors the claude_cli.stream span the pre-refactor provider used).
    #[tracing::instrument(
        name = "agent_cli.stream",
        skip_all,
        fields(provider = %self.id, model = %req.model.label()),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // 1. Per-CLI invocation (argv flags, serialized prompt, env-strip,
        //    cwd). The dialect owns everything CLI-specific.
        let inv = self.dialect.invocation(&req, &ctx)?;
        let model = inv.model.clone();

        // 2. Spawn + OS-sandbox + drain — the shared machinery. The runner
        //    reconstructs the CLI's answer into a single JSON value
        //    (buffered blob or the stream-json `result` event).
        let payload = self.runner.run(inv).await?;

        // 3. Map the CLI answer → content events per the dialect's output
        //    mode. `Started` is prepended by the backend (it knows the
        //    requested model); the dialect owns only the content parse.
        let content = match self.dialect.output_mode() {
            OutputMode::JsonEvents => self.dialect.parse_line(&payload)?,
            OutputMode::Text => {
                // A `Text` dialect's runner returns the raw stdout as a JSON
                // string (agy prints a plain answer, no JSON). Hand it to
                // `parse_text` (default: one Delta + a natural Finished).
                let stdout = payload.as_str().ok_or_else(|| {
                    ProviderError::Internal(format!(
                        "AgentCliBackend: OutputMode::Text runner must return a JSON string of \
                         raw stdout, got: {}",
                        crate::http_base::truncate_utf8(&payload.to_string(), 200)
                    ))
                })?;
                self.dialect.parse_text(stdout)?
            }
        };

        // 4. Honor the caller's output budget. These delegate CLIs take no
        //    `--max-output-tokens` flag, so we post-clamp (chars ≈ tokens*4,
        //    UTF-8-safe) and flip the terminal stop reason to MaxTokens when
        //    WE clipped — otherwise a cut reply looks like a natural end.
        let content = clamp_to_output_budget(content, req.max_output_tokens);

        let mut events: Vec<Result<ChatEvent, ProviderError>> = Vec::with_capacity(content.len() + 1);
        events.push(Ok(ChatEvent::started(model)));
        events.extend(content.into_iter().map(Ok));

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Clamp the assistant text carried by `content` to `max_output_tokens`
/// (interpreted as `*4` chars, matching the pre-refactor claude path). When
/// any truncation happens, the terminal `Finished` is re-stamped
/// `MaxTokens`. A no-op when no budget is set or nothing exceeds it.
fn clamp_to_output_budget(
    mut content: Vec<ChatEvent>,
    max_output_tokens: Option<u32>,
) -> Vec<ChatEvent> {
    let Some(cap) = max_output_tokens.map(|t| (t as usize) * 4) else {
        return content;
    };

    let mut truncated = false;
    for ev in &mut content {
        if let ChatEvent::Delta { text } = ev {
            if text.len() > cap {
                // `truncate_utf8` rounds down to the previous char boundary
                // (no ellipsis, so the byte cap is still honored) — `[..cap]`
                // would panic mid-codepoint.
                *text = crate::http_base::truncate_utf8(text, cap).to_string();
                truncated = true;
            }
        }
    }

    if truncated {
        for ev in &mut content {
            if let ChatEvent::Finished { stop_reason, .. } = ev {
                *stop_reason = StopReason::MaxTokens;
            }
        }
    }

    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tars_types::{ModelHint, Usage};

    /// Records the invocation and returns a canned payload — the FakeRunner
    /// pattern from the pre-refactor claude_cli suite.
    struct FakeRunner {
        payload: Value,
        recorded: std::sync::Mutex<Option<SubprocessInvocation>>,
    }

    #[async_trait]
    impl SubprocessRunner for FakeRunner {
        async fn run(&self, inv: SubprocessInvocation) -> Result<Value, ProviderError> {
            *self.recorded.lock().unwrap() = Some(inv);
            Ok(self.payload.clone())
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
            ClaudeCliTools::Disabled,
            false,
            None,
            true,
            Vec::new(),
        ));
        let caps = Capabilities::text_only_baseline(tars_types::Pricing::default());
        let backend = Arc::new(AgentCliBackend::new(
            "agent_cli_test".into(),
            caps,
            dialect,
            runner.clone(),
        ));
        (backend, runner)
    }

    /// E2E-1: claude through `AgentCliBackend` + `ClaudeCliDialect` produces
    /// the same event stream (Started → Delta → Finished) as the pre-refactor
    /// provider, with the argv the dialect emits driving the invocation.
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
            .stream(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
            .await;

        assert!(matches!(&events[0], ChatEvent::Started { actual_model, .. } if actual_model == "opus"));
        assert!(matches!(&events[1], ChatEvent::Delta { text } if text == "hello from claude"));
        match &events[2] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                // Anthropic `input_tokens` (12) is fresh-only; tars folds in
                // the disjoint cache read (3) for a TOTAL of 15.
                assert_eq!(usage.input_tokens, 15);
                assert_eq!(usage.output_tokens, 5);
                assert_eq!(usage.cached_input_tokens, 3);
            }
            other => panic!("expected Finished, got {other:?}"),
        }

        // The invocation the runner received carries the claude argv the
        // dialect builds — the same tokens the old runner spawned.
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

        let mut req = ChatRequest::user(ModelHint::Explicit("opus".into()), "hi");
        req.max_output_tokens = Some(10); // → 40 chars

        let resp = backend
            .complete(req, RequestContext::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text.len(), 40);
        assert_eq!(resp.stop_reason, Some(StopReason::MaxTokens));
    }

    #[tokio::test]
    async fn runner_error_propagates_through_backend() {
        struct ErrRunner;
        #[async_trait]
        impl SubprocessRunner for ErrRunner {
            async fn run(&self, _: SubprocessInvocation) -> Result<Value, ProviderError> {
                Err(ProviderError::CliSubprocessDied {
                    exit_code: Some(0),
                    stderr: "claude CLI returned error: rate limited".into(),
                })
            }
        }
        let dialect = Arc::new(ClaudeCliDialect::new(
            "claude".into(),
            std::time::Duration::from_secs(1),
            ClaudeCliTools::Disabled,
            false,
            None,
            true,
            Vec::new(),
        ));
        let backend = Arc::new(AgentCliBackend::new(
            "c".into(),
            Capabilities::text_only_baseline(tars_types::Pricing::default()),
            dialect,
            Arc::new(ErrRunner),
        ));
        let err = backend
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::CliSubprocessDied { .. }));
    }

    /// Text-mode wiring (M3): a `Text` dialect (antigravity) whose runner
    /// returns raw stdout as a `Value::String` flows through the backend's
    /// `OutputMode::Text` branch → `parse_text` → Started + Delta + Finished.
    #[tokio::test]
    async fn text_mode_dialect_through_backend_emits_started_delta_finished() {
        use dialects::antigravity::AntigravityDialect;

        let runner = Arc::new(FakeRunner {
            payload: Value::String("plain text answer\n".into()),
            recorded: std::sync::Mutex::new(None),
        });
        let dialect = Arc::new(AntigravityDialect::new(
            "agy".into(),
            std::time::Duration::from_secs(300),
        ));
        let caps = Capabilities::text_only_baseline(tars_types::Pricing::default());
        let backend = Arc::new(AgentCliBackend::new(
            "agy_test".into(),
            caps,
            dialect,
            runner,
        ));

        use futures::StreamExt;
        let events: Vec<ChatEvent> = Arc::clone(&backend)
            .stream(
                ChatRequest::user(ModelHint::Explicit("gemini-2.5-pro".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
            .await;

        assert!(matches!(&events[0], ChatEvent::Started { actual_model, .. } if actual_model == "gemini-2.5-pro"));
        assert!(matches!(&events[1], ChatEvent::Delta { text } if text == "plain text answer"));
        assert!(matches!(&events[2], ChatEvent::Finished { stop_reason, .. } if *stop_reason == StopReason::EndTurn));
    }

    #[test]
    fn clamp_is_noop_without_budget() {
        let content = vec![
            ChatEvent::Delta {
                text: "x".repeat(100),
            },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ];
        let out = clamp_to_output_budget(content, None);
        assert!(matches!(&out[0], ChatEvent::Delta { text } if text.len() == 100));
        assert!(matches!(&out[1], ChatEvent::Finished { stop_reason, .. } if *stop_reason == StopReason::EndTurn));
    }
}
