//! OpenAI Codex CLI as an LLM Provider — ChatGPT-subscription path.
//!
//! Mirrors the [`super::claude_cli`] pattern: shell out to a subscription-
//! billed CLI, **strip API-billing env vars** before exec'ing the child
//! so a leaked key can't silently bill the wrong account.
//!
//! - Spawns `codex exec --json --model X --sandbox SAFE --ask-for-approval
//!   never --skip-git-repo-check -` and feeds the prompt on stdin.
//! - **Strips** `OPENAI_API_KEY` / `CODEX_API_KEY` /
//!   `CODEX_AGENT_IDENTITY` (case-insensitive) so codex falls through to
//!   `~/.codex/auth.json` (ChatGPT OAuth) instead of the API path.
//! - Streams stdout line-by-line as JSONL [`ThreadEvent`]s, translating
//!   each to one or more [`ChatEvent`]s on the fly. The translation is
//!   deliberately conservative — it surfaces only `agent_message`
//!   (final answer) and `reasoning` (thinking) by default; tool-use,
//!   file-changes, command executions, etc. are dropped because the
//!   v1 consumer (TARS [`WorkerAgent`]) only cares about the model's
//!   text answer. Surfacing them as folded text is a v2 knob.
//! - **Cancellation** via Drop on the returned event stream — kills the
//!   child process through tokio's `kill_on_drop(true)`.
//!
//! Testability: the actual `Command::spawn()` lives behind a
//! [`SubprocessLineRunner`] trait so unit tests substitute a fake that
//! emits canned JSONL lines without needing the real `codex` binary.
//!
//! ## What this provider intentionally does NOT do
//!
//! - **No `--image` / `--profile` / `--output-last-message` / `-c`
//!   flags.** Each is one or two lines but pulls in user-facing config
//!   choices we don't want to commit to in v1.
//! - **No long-lived process pool** (Doc 01 §6.2.1). Same posture as
//!   `claude_cli` — spawn-per-call now, process pool when B-1 ships.
//! - **No `codex login` automation.** The user runs `codex login` once
//!   themselves; we read the result via codex's normal auth flow.
//! - **No tool / file-change folding into the response text.** v1
//!   drops these events; the LLM's final answer text is the only
//!   thing the consumer sees.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ContentBlock, Message, Modality, PromptCacheKind,
    ProviderError, ProviderId, RequestContext, StopReason, StructuredOutputMode, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// Env vars that must NEVER leak into the child `codex` process —
/// case-insensitive match. If any survives, codex's auth manager
/// (`login/src/auth/manager.rs`) picks it up and routes the request
/// through API billing instead of the user's ChatGPT subscription.
const STRIPPED_ENV_KEYS_UPPER: &[&str] = &[
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_AGENT_IDENTITY",
];

/// Sandbox modes accepted by `codex exec --sandbox`. Default is
/// `ReadOnly` for the principle-of-least-surprise: a TARS user
/// shouldn't get unexpected file mutations from spawning a Worker.
/// Override with [`CodexCliProviderBuilder::sandbox`].
#[derive(Clone, Copy, Debug, Default)]
pub enum SandboxMode {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    fn as_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

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
            // codex exec runs a full agent loop — 10 min default is
            // generous but not crazy. Tune per deployment.
            timeout: Duration::from_secs(600),
            sandbox: SandboxMode::ReadOnly,
            capabilities: None,
            // Default true: TARS workers may operate outside a git
            // repo (tempdir tests, scratch files), and codex's
            // git-repo gate would refuse with confusing error text.
            skip_git_repo_check: true,
        }
    }

    pub fn executable(mut self, e: impl Into<String>) -> Self {
        self.executable = e.into();
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn sandbox(mut self, s: SandboxMode) -> Self {
        self.sandbox = s;
        self
    }

    pub fn skip_git_repo_check(mut self, yes: bool) -> Self {
        self.skip_git_repo_check = yes;
        self
    }

    pub fn capabilities(mut self, c: Capabilities) -> Self {
        self.capabilities = Some(c);
        self
    }

    pub fn build(self) -> Arc<CodexCliProvider> {
        self.build_with_runner(Arc::new(RealSubprocessLineRunner))
    }

    pub fn build_with_runner(
        self,
        runner: Arc<dyn SubprocessLineRunner>,
    ) -> Arc<CodexCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        Arc::new(CodexCliProvider {
            id: self.id,
            executable: self.executable,
            timeout: self.timeout,
            sandbox: self.sandbox,
            skip_git_repo_check: self.skip_git_repo_check,
            capabilities: caps,
            runner,
        })
    }
}

fn default_capabilities() -> Capabilities {
    let mut text = HashSet::new();
    text.insert(Modality::Text);
    Capabilities {
        // ChatGPT 5 / 5-codex is roughly 200K context.
        max_context_tokens: 200_000,
        // CLI doesn't expose --max-output-tokens; we post-truncate.
        max_output_tokens: 64_000,
        // The CLI surfaces tool calls as ThreadItem events (codex's
        // INTERNAL tools — sandbox-shell, apply-patch, MCP). They're
        // NOT exposed back to TARS as our `ToolCall` shape — codex
        // dispatches them itself. So from TARS's perspective this
        // provider does NOT support tool use (the LLM-emit-then-
        // dispatch dance happens entirely inside codex).
        supports_tool_use: false,
        supports_parallel_tool_calls: false,
        // Same reasoning as supports_tool_use: codex doesn't honor
        // OpenAI strict-mode response_format here.
        supports_structured_output: StructuredOutputMode::None,
        supports_vision: false,
        supports_thinking: true,
        // Spawn-per-call; mid-call cancel is via Drop only (the child
        // gets killed when the event stream is dropped).
        supports_cancel: true,
        prompt_cache: PromptCacheKind::Delegated,
        streaming: true,
        modalities_in: text.clone(),
        modalities_out: text,
        // Subscription-billed; per-token pricing N/A here.
        pricing: tars_types::Pricing::default(),
    }
}

pub struct CodexCliProvider {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    sandbox: SandboxMode,
    skip_git_repo_check: bool,
    capabilities: Capabilities,
    runner: Arc<dyn SubprocessLineRunner>,
}

#[async_trait]
impl LlmProvider for CodexCliProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let model = req
            .model
            .explicit()
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "model must be explicit before reaching CLI provider".into(),
                )
            })?
            .to_string();

        let prompt = serialize_messages_for_cli(&req);

        let invocation = CodexInvocation {
            executable: self.executable.clone(),
            model: model.clone(),
            prompt,
            sandbox: self.sandbox,
            skip_git_repo_check: self.skip_git_repo_check,
            timeout: self.timeout,
            stripped_env: STRIPPED_ENV_KEYS_UPPER.iter().map(|s| s.to_string()).collect(),
        };

        let line_stream = self.runner.run(invocation).await?;
        Ok(translate_codex_to_chat(line_stream, model))
    }
}

/// Single CLI invocation — what [`SubprocessLineRunner`] needs to know.
#[derive(Clone, Debug)]
pub struct CodexInvocation {
    pub executable: String,
    pub model: String,
    pub prompt: String,
    pub sandbox: SandboxMode,
    pub skip_git_repo_check: bool,
    pub timeout: Duration,
    /// Env vars to strip from the child (UPPER-CASE for case-insensitive match).
    pub stripped_env: HashSet<String>,
}

/// JSONL line stream from the subprocess. One stdout line per item;
/// errors abort the stream.
pub type CodexLineStream = BoxStream<'static, Result<String, ProviderError>>;

/// Abstraction for "spawn `codex exec --json` and stream stdout lines".
/// Production impl spawns a real subprocess; tests substitute a fake.
#[async_trait]
pub trait SubprocessLineRunner: Send + Sync {
    async fn run(&self, invocation: CodexInvocation) -> Result<CodexLineStream, ProviderError>;
}

pub struct RealSubprocessLineRunner;

#[async_trait]
impl SubprocessLineRunner for RealSubprocessLineRunner {
    async fn run(&self, inv: CodexInvocation) -> Result<CodexLineStream, ProviderError> {
        let mut cmd = Command::new(&inv.executable);
        cmd.arg("exec")
            .arg("--json")
            .arg("--model")
            .arg(&inv.model)
            .arg("--sandbox")
            .arg(inv.sandbox.as_arg())
            .arg("--ask-for-approval")
            .arg("never");
        if inv.skip_git_repo_check {
            cmd.arg("--skip-git-repo-check");
        }
        // `-` tells codex to read the prompt from stdin.
        cmd.arg("-");

        // Strip the dangerous env vars CASE-INSENSITIVELY. Pass through everything else.
        cmd.env_clear();
        for (k, v) in std::env::vars() {
            if !inv.stripped_env.contains(&k.to_uppercase()) {
                cmd.env(k, v);
            }
        }

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("`{}` not found in PATH", inv.executable),
            },
            std::io::ErrorKind::PermissionDenied => ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("`{}` not executable: {e}", inv.executable),
            },
            _ => ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("spawn failed: {e}"),
            },
        })?;

        // Write the prompt on stdin and close it so codex sees EOF.
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(inv.prompt.as_bytes())
                .await
                .map_err(|e| ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("stdin write failed: {e}"),
                })?;
            drop(stdin);
        }

        let stdout = child.stdout.take().ok_or_else(|| ProviderError::Internal(
            "codex child has no stdout pipe (Stdio::piped above)".into(),
        ))?;
        let stderr = child.stderr.take();
        let timeout = inv.timeout;

        let line_stream = stream! {
            // The child is moved into the stream so kill_on_drop fires
            // when the consumer drops the stream mid-read.
            let mut child = child;
            let stderr = stderr;
            let mut lines = BufReader::new(stdout).lines();
            let deadline = tokio::time::Instant::now() + timeout;

            loop {
                let next = tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => {
                        // Drop the child to kill it via kill_on_drop;
                        // surface a clean error on the stream.
                        drop(child);
                        yield Err(ProviderError::CliSubprocessDied {
                            exit_code: None,
                            stderr: format!("timed out after {}s", timeout.as_secs()),
                        });
                        return;
                    }
                    line = lines.next_line() => line,
                };
                match next {
                    Ok(Some(l)) => yield Ok(l),
                    Ok(None) => break,
                    Err(e) => {
                        yield Err(ProviderError::CliSubprocessDied {
                            exit_code: None,
                            stderr: format!("stdout read failed: {e}"),
                        });
                        return;
                    }
                }
            }

            // Stdout closed — child should be exiting. wait() with a
            // small grace period so we can attribute non-zero exits.
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) if !status.success() => {
                    let stderr_text = if let Some(mut se) = stderr {
                        let mut buf = String::new();
                        let _ = tokio::io::AsyncReadExt::read_to_string(&mut se, &mut buf).await;
                        buf
                    } else {
                        String::new()
                    };
                    let truncated = truncate(&stderr_text, 500);
                    yield Err(ProviderError::CliSubprocessDied {
                        exit_code: status.code(),
                        stderr: format!("codex exited non-zero: {truncated}"),
                    });
                }
                Ok(Ok(_)) => { /* clean exit, nothing more to do */ }
                Ok(Err(e)) => {
                    yield Err(ProviderError::CliSubprocessDied {
                        exit_code: None,
                        stderr: format!("wait failed: {e}"),
                    });
                }
                Err(_) => {
                    // Child hadn't exited yet — kill_on_drop will
                    // reap it when the future drops.
                }
            }
        };
        Ok(Box::pin(line_stream))
    }
}

// ── ThreadEvent → ChatEvent translation ───────────────────────────────

/// Mirror of codex's [`exec_events::ThreadEvent`] surface — only the
/// fields v1 actually consumes. Unknown extra fields are ignored
/// (`#[serde(deny_unknown_fields)]` deliberately NOT set so codex can
/// add new event types or fields without breaking us).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ThreadEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted {},
    #[serde(rename = "turn.started")]
    TurnStarted {},
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: CodexUsage },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: ThreadError },
    // We drop these in v1 (see map_thread_event); the field exists
    // only so serde matches the wire shape `{"type":"item.*","item":{...}}`.
    // IgnoredAny avoids the parse cost on payloads we throw away.
    #[serde(rename = "item.started")]
    #[allow(dead_code)]
    ItemStarted { item: serde::de::IgnoredAny },
    #[serde(rename = "item.updated")]
    #[allow(dead_code)]
    ItemUpdated { item: serde::de::IgnoredAny },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: ThreadItem },
    #[serde(rename = "error")]
    Error(ThreadError),
}

#[derive(Debug, Deserialize)]
struct ThreadError {
    message: String,
}

#[derive(Debug, Default, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    reasoning_output_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct ThreadItem {
    #[serde(flatten)]
    details: ThreadItemDetails,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ThreadItemDetails {
    AgentMessage { text: String },
    Reasoning { text: String },
    // All other variants captured as Other so we don't fail on unknown
    // shapes; v1 drops them. When we want to surface tool / command /
    // file_change events we'll add discriminated arms here.
    #[serde(other)]
    Other,
}

/// Translate one codex JSONL event into 0..N TARS ChatEvents. Pure
/// function — exposed for unit testing.
fn map_thread_event(event: ThreadEvent) -> Vec<Result<ChatEvent, ProviderError>> {
    match event {
        // Lifecycle events that don't carry user-facing payload — drop.
        ThreadEvent::ThreadStarted {} => vec![],
        ThreadEvent::TurnStarted {} => vec![],
        // Per codex source (event_processor_with_jsonl_output.rs:335),
        // agent_message + reasoning items only emit `item.completed`,
        // never `item.started` or `item.updated`. Other items DO emit
        // started/updated, but v1 drops everything except completed.
        ThreadEvent::ItemStarted { item: _ } => vec![],
        ThreadEvent::ItemUpdated { item: _ } => vec![],
        ThreadEvent::ItemCompleted { item } => match item.details {
            ThreadItemDetails::AgentMessage { text } if !text.is_empty() => {
                vec![Ok(ChatEvent::Delta { text })]
            }
            ThreadItemDetails::Reasoning { text } if !text.is_empty() => {
                vec![Ok(ChatEvent::ThinkingDelta { text })]
            }
            // Empty text or unknown item kinds — drop.
            _ => vec![],
        },
        ThreadEvent::TurnCompleted { usage } => {
            vec![Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: convert_usage(&usage),
            })]
        }
        ThreadEvent::TurnFailed { error } => {
            vec![Err(ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("codex turn failed: {}", error.message),
            })]
        }
        ThreadEvent::Error(error) => {
            vec![Err(ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("codex stream error: {}", error.message),
            })]
        }
    }
}

fn convert_usage(u: &CodexUsage) -> Usage {
    // codex's tokens are i64 (its TS-friendly representation); ours
    // are u64. Negatives shouldn't occur — clamp defensively.
    let to_u64 = |v: i64| if v < 0 { 0 } else { v as u64 };
    Usage {
        input_tokens: to_u64(u.input_tokens),
        output_tokens: to_u64(u.output_tokens),
        cached_input_tokens: to_u64(u.cached_input_tokens),
        cache_creation_tokens: 0, // codex doesn't model this
        thinking_tokens: to_u64(u.reasoning_output_tokens),
    }
}

/// Wrap a JSONL line stream into a ChatEvent stream. Emits one
/// up-front `Started` event with the model label, then drains the
/// JSONL stream, mapping each line through [`map_thread_event`].
fn translate_codex_to_chat(
    mut line_stream: CodexLineStream,
    model_label: String,
) -> LlmEventStream {
    let s = stream! {
        yield Ok(ChatEvent::started(model_label));
        while let Some(line_result) = line_stream.next().await {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let event: ThreadEvent = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    // Be lenient — codex may add new event variants
                    // we don't recognise; logging + skipping beats
                    // failing the whole stream.
                    tracing::debug!(
                        line = %truncate(&line, 200),
                        error = %e,
                        "codex_cli: skipping unparseable line",
                    );
                    continue;
                }
            };
            for ev in map_thread_event(event) {
                let was_error = ev.is_err();
                yield ev;
                if was_error {
                    return;
                }
            }
        }
    };
    Box::pin(s)
}

/// Flatten our message history into the single text blob `codex exec`
/// reads from stdin. Same `[role]\n content` shape as
/// [`super::claude_cli`] — codex doesn't have a multi-turn API on the
/// CLI surface, so we feed the whole conversation as one prompt.
fn serialize_messages_for_cli(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len());
    if let Some(sys) = &req.system {
        parts.push(format!("[system]\n{sys}"));
    }
    for m in &req.messages {
        let (role, content) = match m {
            Message::User { content } => ("user", content),
            Message::Assistant { content, .. } => ("assistant", content),
            Message::Tool { content, .. } => ("tool", content),
            Message::System { content } => ("system", content),
        };
        let flat = flatten_blocks(content);
        parts.push(format!("[{role}]\n{flat}"));
    }
    parts.join("\n\n")
}

fn flatten_blocks(blocks: &[ContentBlock]) -> String {
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => out.push(text.clone()),
            ContentBlock::Image { mime, .. } => {
                out.push(format!("[image:{mime}]"));
            }
        }
    }
    out.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed = crate::http_base::truncate_utf8(s, max);
    if trimmed.len() == s.len() {
        s.to_string()
    } else {
        format!("{trimmed}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use serde_json::json;
    use std::sync::Mutex;
    use tars_types::ModelHint;

    // ── ThreadEvent → ChatEvent mapping ─────────────────────────────

    fn parse(json_line: serde_json::Value) -> ThreadEvent {
        serde_json::from_value(json_line).expect("test JSON should be a valid ThreadEvent")
    }

    #[test]
    fn agent_message_completed_yields_one_delta_with_full_text() {
        let ev = parse(json!({
            "type": "item.completed",
            "item": {"id": "i1", "type": "agent_message", "text": "Hello, world."},
        }));
        let out = map_thread_event(ev);
        assert_eq!(out.len(), 1);
        match out.into_iter().next().unwrap().unwrap() {
            ChatEvent::Delta { text } => assert_eq!(text, "Hello, world."),
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_completed_yields_thinking_delta() {
        let ev = parse(json!({
            "type": "item.completed",
            "item": {"id": "i2", "type": "reasoning", "text": "Let me think..."},
        }));
        match map_thread_event(ev).into_iter().next().unwrap().unwrap() {
            ChatEvent::ThinkingDelta { text } => assert_eq!(text, "Let me think..."),
            other => panic!("expected ThinkingDelta, got {other:?}"),
        }
    }

    #[test]
    fn empty_agent_message_text_drops_silently() {
        let ev = parse(json!({
            "type": "item.completed",
            "item": {"id": "i3", "type": "agent_message", "text": ""},
        }));
        assert!(map_thread_event(ev).is_empty());
    }

    #[test]
    fn turn_completed_yields_finished_with_converted_usage() {
        let ev = parse(json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 30,
                "output_tokens": 50,
                "reasoning_output_tokens": 20,
            },
        }));
        let out = map_thread_event(ev);
        match out.into_iter().next().unwrap().unwrap() {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.cached_input_tokens, 30);
                assert_eq!(usage.output_tokens, 50);
                assert_eq!(usage.thinking_tokens, 20);
                assert_eq!(usage.cache_creation_tokens, 0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn turn_failed_yields_provider_error_with_message() {
        let ev = parse(json!({
            "type": "turn.failed",
            "error": {"message": "rate limited"},
        }));
        let result = map_thread_event(ev).into_iter().next().unwrap();
        match result.unwrap_err() {
            ProviderError::CliSubprocessDied { stderr, .. } => {
                assert!(stderr.contains("rate limited"));
                assert!(stderr.contains("turn failed"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }

    #[test]
    fn top_level_error_yields_provider_error() {
        let ev = parse(json!({
            "type": "error",
            "message": "stream broke",
        }));
        let result = map_thread_event(ev).into_iter().next().unwrap();
        match result.unwrap_err() {
            ProviderError::CliSubprocessDied { stderr, .. } => {
                assert!(stderr.contains("stream broke"));
                assert!(stderr.contains("stream error"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }

    #[test]
    fn lifecycle_events_drop_silently() {
        for line in [
            json!({"type": "thread.started", "thread_id": "t"}),
            json!({"type": "turn.started"}),
            json!({"type": "item.started", "item": {"id": "x", "type": "command_execution", "command":"ls", "aggregated_output":"", "exit_code": null, "status":"in_progress"}}),
            json!({"type": "item.updated", "item": {"id": "x", "type": "todo_list", "items": []}}),
        ] {
            assert!(
                map_thread_event(parse(line.clone())).is_empty(),
                "lifecycle event {line:?} should drop",
            );
        }
    }

    #[test]
    fn unknown_item_kinds_drop_silently_via_serde_other() {
        // command_execution / file_change / mcp_tool_call all hit the
        // `Other` arm in v1.
        for kind in ["command_execution", "file_change", "mcp_tool_call", "web_search"] {
            let ev = parse(json!({
                "type": "item.completed",
                "item": {"id": "x", "type": kind, "command": "ls", "aggregated_output": "", "status": "completed"},
            }));
            assert!(
                map_thread_event(ev).is_empty(),
                "item.completed of kind `{kind}` should drop in v1",
            );
        }
    }

    #[test]
    fn negative_usage_tokens_are_clamped_to_zero() {
        let u = CodexUsage {
            input_tokens: -5,
            cached_input_tokens: -1,
            output_tokens: 10,
            reasoning_output_tokens: -2,
        };
        let converted = convert_usage(&u);
        assert_eq!(converted.input_tokens, 0);
        assert_eq!(converted.cached_input_tokens, 0);
        assert_eq!(converted.output_tokens, 10);
        assert_eq!(converted.thinking_tokens, 0);
    }

    // ── End-to-end stream translation ───────────────────────────────

    /// A FakeRunner that emits a canned line sequence and records the
    /// invocation it received.
    struct FakeRunner {
        lines: Vec<String>,
        recorded: Mutex<Option<CodexInvocation>>,
    }

    impl FakeRunner {
        fn new(lines: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                lines: lines.into_iter().map(String::from).collect(),
                recorded: Mutex::new(None),
            })
        }
        fn recorded(&self) -> Option<CodexInvocation> {
            self.recorded.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SubprocessLineRunner for FakeRunner {
        async fn run(
            &self,
            invocation: CodexInvocation,
        ) -> Result<CodexLineStream, ProviderError> {
            *self.recorded.lock().unwrap() = Some(invocation);
            let lines = self.lines.clone();
            let s = stream::iter(lines.into_iter().map(Ok));
            Ok(Box::pin(s))
        }
    }

    fn make_provider(lines: Vec<&str>) -> (Arc<CodexCliProvider>, Arc<FakeRunner>) {
        let runner = FakeRunner::new(lines);
        let provider = CodexCliProviderBuilder::new("codex_cli_test")
            .build_with_runner(runner.clone());
        (provider, runner)
    }

    #[tokio::test]
    async fn end_to_end_stream_yields_started_delta_finished() {
        let (provider, _) = make_provider(vec![
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
        // text comes from agent_message only; thinking is on its own
        // accumulator (response.thinking) by the ChatResponseBuilder
        // contract.
        assert_eq!(response.text, "answer");
        assert_eq!(response.usage.thinking_tokens, 5);
    }

    #[tokio::test]
    async fn end_to_end_drops_internal_tool_events_in_v1() {
        let (provider, _) = make_provider(vec![
            r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"ls","aggregated_output":"foo bar","exit_code":0,"status":"completed"}}"#,
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
        // `ls` / `foo bar` MUST NOT leak into the user-visible text.
        assert_eq!(response.text, "done");
    }

    #[tokio::test]
    async fn end_to_end_blank_lines_and_garbage_are_skipped() {
        let (provider, _) = make_provider(vec![
            "",
            "   ",
            "this is not json",
            r#"{"type":"item.completed","item":{"id":"a1","type":"agent_message","text":"ok"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}"#,
        ]);
        let response = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gpt-5".into()), "x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(response.text, "ok");
    }

    #[tokio::test]
    async fn end_to_end_turn_failed_surfaces_provider_error() {
        let (provider, _) = make_provider(vec![
            r#"{"type":"turn.failed","error":{"message":"context too long"}}"#,
        ]);
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

    #[tokio::test]
    async fn invocation_carries_stripped_env_keys() {
        let (provider, runner) = make_provider(vec![
            r#"{"type":"item.completed","item":{"id":"a1","type":"agent_message","text":"x"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}"#,
        ]);
        let _ = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gpt-5".into()), "x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded().expect("runner should have recorded the invocation");
        for k in ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AGENT_IDENTITY"] {
            assert!(
                inv.stripped_env.contains(k),
                "stripped_env must contain {k}",
            );
        }
    }

    #[tokio::test]
    async fn invocation_carries_explicit_model() {
        let (provider, runner) = make_provider(vec![
            r#"{"type":"item.completed","item":{"id":"a1","type":"agent_message","text":"x"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}"#,
        ]);
        let _ = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gpt-5-codex".into()), "x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded().unwrap();
        assert_eq!(inv.model, "gpt-5-codex");
    }

    #[test]
    fn message_serializer_includes_system_then_each_role_block() {
        let req = ChatRequest {
            model: ModelHint::Explicit("x".into()),
            system: Some("be brief".into()),
            messages: vec![
                Message::user_text("first user"),
                Message::assistant_text("first assistant"),
                Message::user_text("second user"),
            ],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: Default::default(),
        };
        let serialized = serialize_messages_for_cli(&req);
        assert!(serialized.starts_with("[system]\nbe brief\n\n[user]\nfirst user\n\n"));
        assert!(serialized.contains("[assistant]\nfirst assistant"));
        assert!(serialized.ends_with("[user]\nsecond user"));
    }

    #[test]
    fn sandbox_mode_arg_string_pins_to_codex_values() {
        // These are the literal strings codex's --sandbox flag accepts;
        // a typo here would silently break the invocation.
        assert_eq!(SandboxMode::ReadOnly.as_arg(), "read-only");
        assert_eq!(SandboxMode::WorkspaceWrite.as_arg(), "workspace-write");
        assert_eq!(SandboxMode::DangerFullAccess.as_arg(), "danger-full-access");
    }
}
