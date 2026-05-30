//! Claude Code CLI as an LLM Provider — subscription path.
//!
//! Mirrors the Python `ClaudeSubprocessClient` in
//! the equivalent Python subprocess client:
//!
//! - Shells out to `claude -p - --model X --output-format json
//!   --disable-slash-commands` and feeds the prompt on stdin.
//! - **Strips** `ANTHROPIC_API_KEY` (case-insensitive) and 3rd-party
//!   routing env vars before exec'ing the child. If any leak through,
//!   `claude` switches to API-billing mode and silently bills the
//!   wrong account.
//! - Parses the single JSON output from the CLI; surfaces typed errors
//!   on non-zero exit / malformed JSON / `is_error: true` payload.
//! - **Not yet streaming** — Doc 01 §6.2 calls for a long-lived process
//!   pool with `--output-format stream-json`; that's the next iteration
//!   (Doc 01 §6.2.1). This first cut spawns per call.
//!
//! Testability: the actual `Command::output()` call is behind a
//! [`SubprocessRunner`] trait so tests substitute a fake without
//! needing the real `claude` binary installed.
//!
//! ## `arc scan --judge` finding `ARC-L5-COH-19` (env + subprocess)
//!
//! This module owns three `std::env` reads and one `Command::new`
//! site that the scan flagged as scattered cohesion: the
//! `TARS_CLAUDE_CLI_STREAM` feature flag, the `std::env::vars()`
//! sweep used to **strip** untrusted keys (the security boundary
//! commented above), and the `Command::new(&inv.executable)` for the
//! `claude -p` subprocess. They are deliberately co-located with the
//! backend that interprets them:
//!
//! - `TARS_CLAUDE_CLI_STREAM` is a per-backend toggle whose semantics
//!   ("stream-json vs single-blob JSON") only make sense alongside the
//!   `build_argv` shape; moving it to a typed-env config crate would
//!   scatter the var name from the call site that knows the trade-off.
//! - The `env::vars()` sweep is a security boundary, not generic
//!   environment access — the strip table (`STRIPPED_ENV_KEYS_UPPER`)
//!   is specific to this backend's auth-routing concerns and would
//!   not generalize to a shared helper.
//! - `Command::new(&inv.executable)` is the spawn site for *this*
//!   backend's CLI; each provider backend (claude / codex / gemini)
//!   legitimately spawns its own provider-specific executable, which
//!   the scan's `[coh] subprocess` row classifies as **essential**
//!   (the `claude_cli.rs` count is one of those essential sites).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ContentBlock, Message, Modality, PromptCacheKind,
    ProviderError, ProviderId, RequestContext, StopReason, StructuredOutputMode, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// Env vars that must NEVER leak into the child `claude` process.
/// Case-insensitive — Windows preserves env var case, so `Anthropic_Api_Key`
/// would slip past a literal-equality check (the Python comment is exactly
/// about this hazard).
const STRIPPED_ENV_KEYS_UPPER: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// What to pass via `--tools` on the CLI argv.
///
/// `Disabled` is the safe default — without any tools available the
/// CLI cannot start its internal agent loop, so a `--tools ""` call
/// is a pure inference channel and is **auth-neutral**. See
/// [docs/architecture/01-llm-provider.md §17](../../../../docs/architecture/01-llm-provider.md)
/// for the design rationale and the token-inflation data that motivated it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ClaudeCliTools {
    /// `--tools ""` — disable every tool. No agent loop possible.
    #[default]
    Disabled,
    /// Omit `--tools` entirely — inherit the CLI's default (full tool access).
    Default,
    /// `--tools "<csv>"` — allow only the named tools (e.g. `["Read","Bash"]`).
    Allow(Vec<String>),
}

/// What to pass via `--effort` on the CLI argv. `None` means omit the flag
/// and let the CLI use its own default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeCliEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ClaudeCliEffort {
    fn as_arg(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClaudeCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Option<Capabilities>,
    tools: ClaudeCliTools,
    bare: bool,
    effort: Option<ClaudeCliEffort>,
    exclude_dynamic_sections: bool,
    extra_args: Vec<String>,
}

impl ClaudeCliProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            executable: "claude".to_string(),
            timeout: Duration::from_secs(300),
            capabilities: None,
            // Default-safe values — see field doc-comments below.
            tools: ClaudeCliTools::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: Vec::new(),
        }
    }

    /// Override the binary path / name. Defaults to `claude` (PATH lookup).
    pub fn executable(mut self, e: impl Into<String>) -> Self {
        self.executable = e.into();
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn capabilities(mut self, c: Capabilities) -> Self {
        self.capabilities = Some(c);
        self
    }

    /// Configure `--tools`. Default: [`ClaudeCliTools::Disabled`] — kills
    /// the CLI's internal agent loop without affecting auth. Use
    /// [`ClaudeCliTools::Allow`] for a curated tool whitelist or
    /// [`ClaudeCliTools::Default`] to get the CLI's full agent behavior.
    pub fn tools(mut self, t: ClaudeCliTools) -> Self {
        self.tools = t;
        self
    }

    /// Set `--bare`. **Default: `false`.** Setting `true` makes the CLI
    /// skip auto-memory / `CLAUDE.md` auto-discovery / hooks / plugin sync
    /// — but **also disables OAuth + keychain auth**, requiring
    /// `ANTHROPIC_API_KEY` or `apiKeyHelper` to be set. Most `claude_cli`
    /// users authenticate via `claude login` (OAuth + keychain), so the
    /// default is `false` to preserve that path.
    pub fn bare(mut self, b: bool) -> Self {
        self.bare = b;
        self
    }

    /// Set `--effort`. Default: `None` (CLI default, currently `medium`).
    pub fn effort(mut self, e: Option<ClaudeCliEffort>) -> Self {
        self.effort = e;
        self
    }

    /// Set `--exclude-dynamic-system-prompt-sections`. Default: `true`
    /// (improves cross-tenant prompt-cache reuse by stripping per-machine
    /// `cwd` / `env` / `git status` sections out of the system prompt).
    pub fn exclude_dynamic_sections(mut self, b: bool) -> Self {
        self.exclude_dynamic_sections = b;
        self
    }

    /// Escape hatch: append raw argv tokens after every flag the Builder
    /// constructs. Use for flags the Builder doesn't yet model. Don't use
    /// to override flags already set — argv order matters for some flags
    /// and the Builder's value will win on others.
    pub fn extra_args(mut self, a: Vec<String>) -> Self {
        self.extra_args = a;
        self
    }

    /// Build with the default real-process runner.
    pub fn build(self) -> Arc<ClaudeCliProvider> {
        self.build_with_runner(Arc::new(RealSubprocessRunner))
    }

    /// Build with a substituted runner — for tests.
    pub fn build_with_runner(self, runner: Arc<dyn SubprocessRunner>) -> Arc<ClaudeCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        Arc::new(ClaudeCliProvider {
            id: self.id,
            executable: self.executable,
            timeout: self.timeout,
            capabilities: caps,
            tools: self.tools,
            bare: self.bare,
            effort: self.effort,
            exclude_dynamic_sections: self.exclude_dynamic_sections,
            extra_args: self.extra_args,
            runner,
        })
    }
}

fn default_capabilities() -> Capabilities {
    let mut text = HashSet::new();
    text.insert(Modality::Text);
    Capabilities {
        max_context_tokens: 200_000,
        // CLI doesn't expose --max-output-tokens; we post-truncate.
        max_output_tokens: 64_000,
        supports_tool_use: false, // CLI -p mode doesn't expose tool use
        supports_parallel_tool_calls: false,
        supports_structured_output: StructuredOutputMode::None,
        supports_vision: false,
        supports_thinking: false,
        // First iteration: spawn-per-call; cancel works only via Drop
        // before the call begins. Mid-call cancel needs the long-lived
        // mode (Doc 01 §6.2.1).
        supports_cancel: false,
        prompt_cache: PromptCacheKind::Delegated,
        streaming: false,
        modalities_in: text.clone(),
        modalities_out: text,
        // Subscription-billed; per-token pricing N/A here.
        pricing: tars_types::Pricing::default(),
    }
}

pub struct ClaudeCliProvider {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Capabilities,
    tools: ClaudeCliTools,
    bare: bool,
    effort: Option<ClaudeCliEffort>,
    exclude_dynamic_sections: bool,
    extra_args: Vec<String>,
    runner: Arc<dyn SubprocessRunner>,
}

#[async_trait]
impl LlmProvider for ClaudeCliProvider {
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
        let system = req.system.clone();

        let invocation = SubprocessInvocation {
            executable: self.executable.clone(),
            model: model.clone(),
            system,
            prompt,
            timeout: self.timeout,
            stripped_env: STRIPPED_ENV_KEYS_UPPER
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tools: self.tools.clone(),
            bare: self.bare,
            effort: self.effort,
            exclude_dynamic_sections: self.exclude_dynamic_sections,
            extra_args: self.extra_args.clone(),
        };

        let payload = self.runner.run(invocation).await?;
        let response_text = extract_result_text(&payload);
        let usage = extract_usage(&payload);

        let max_chars = req.max_output_tokens.map(|t| (t as usize) * 4);
        let truncated = match max_chars {
            // Truncate on a UTF-8 char boundary — `cap` (max_output_tokens
            // * 4) can land mid-codepoint, and byte-indexing `[..cap]`
            // would panic. `truncate_utf8` rounds down to the previous
            // boundary (no ellipsis, so the byte cap is still honored).
            Some(cap) if response_text.len() > cap => {
                crate::http_base::truncate_utf8(&response_text, cap).to_string()
            }
            _ => response_text,
        };

        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(model)),
            Ok(ChatEvent::Delta { text: truncated }),
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage,
            }),
        ];

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Single CLI invocation — what [`SubprocessRunner`] needs to know.
#[derive(Clone, Debug)]
pub struct SubprocessInvocation {
    pub executable: String,
    pub model: String,
    pub system: Option<String>,
    pub prompt: String,
    pub timeout: Duration,
    /// Env vars to strip from the child (UPPER-CASE for case-insensitive match).
    pub stripped_env: HashSet<String>,
    /// `--tools` knob — see [`ClaudeCliTools`].
    pub tools: ClaudeCliTools,
    /// `--bare` — see [`ClaudeCliProviderBuilder::bare`] for the auth caveat.
    pub bare: bool,
    /// `--effort <level>` — `None` omits the flag.
    pub effort: Option<ClaudeCliEffort>,
    /// `--exclude-dynamic-system-prompt-sections`.
    pub exclude_dynamic_sections: bool,
    /// Raw argv tokens appended at the very end. Escape hatch.
    pub extra_args: Vec<String>,
}

/// Abstraction for "run `claude` and get back its JSON payload".
/// Production impl spawns a real subprocess; tests substitute a fake.
#[async_trait]
pub trait SubprocessRunner: Send + Sync {
    async fn run(&self, invocation: SubprocessInvocation) -> Result<Value, ProviderError>;
}

/// True iff the env var `TARS_CLAUDE_CLI_STREAM` is set to a non-empty,
/// non-zero, non-"false" value. Triggers stream-json mode in [`build_argv`]
/// and live-event mirroring in [`RealSubprocessRunner`].
///
/// Stream mode is opt-in (off by default) so existing callers that depend
/// on the buffered `--output-format json` shape are unaffected.
pub(crate) fn streaming_enabled() -> bool {
    match std::env::var("TARS_CLAUDE_CLI_STREAM") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "" | "0" | "false" | "off" | "no")
        }
        Err(_) => false,
    }
}

/// Construct the full `claude` argv (without the executable itself) for
/// a given [`SubprocessInvocation`]. Shared between [`RealSubprocessRunner`]
/// and the argv-shape tests — that's the whole point of factoring this
/// out: when Anthropic renames a flag, exactly one place changes and
/// every test covering that flag fails immediately.
///
/// Output format is `json` by default. When `TARS_CLAUDE_CLI_STREAM` is
/// set, the CLI is invoked with `stream-json` + `--include-partial-messages`
/// + `--verbose`, which produces a real-time NDJSON event stream
///   ([`RealSubprocessRunner`] tees each event to stderr for observability,
///   reconstructs the final `result` event as the return Value).
// Production now calls `build_argv_with` directly (reading
// `streaming_enabled()` exactly once — see `RealSubprocessRunner::run`).
// This convenience wrapper remains for the argv unit tests.
#[allow(dead_code)]
pub(crate) fn build_argv(inv: &SubprocessInvocation) -> Vec<String> {
    build_argv_with(inv, streaming_enabled())
}

/// Inner constructor used by tests + by [`build_argv`] (which is the
/// production wrapper that reads `streaming_enabled()` from env). Pulled
/// out so tests can exercise both modes without process-global env
/// mutation (workspace forbids `unsafe`; Rust 2024 makes `env::set_var`
/// `unsafe`).
pub(crate) fn build_argv_with(inv: &SubprocessInvocation, streaming: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "-p".into(),
        "-".into(),
        "--model".into(),
        inv.model.clone(),
        "--output-format".into(),
        if streaming {
            "stream-json".into()
        } else {
            "json".into()
        },
        "--disable-slash-commands".into(),
    ];
    if streaming {
        // --verbose is REQUIRED for the CLI to emit the per-event stream
        // alongside stream-json; without it, the result is the same single
        // payload as plain json. --include-partial-messages adds the
        // text_delta / thinking_delta chunks (the "live console" effect).
        argv.push("--include-partial-messages".into());
        argv.push("--verbose".into());
    }

    match &inv.tools {
        ClaudeCliTools::Disabled => {
            argv.push("--tools".into());
            argv.push(String::new());
        }
        ClaudeCliTools::Default => { /* omit --tools entirely */ }
        ClaudeCliTools::Allow(list) => {
            argv.push("--tools".into());
            argv.push(list.join(","));
        }
    }

    if inv.bare {
        argv.push("--bare".into());
    }

    if let Some(e) = inv.effort {
        argv.push("--effort".into());
        argv.push(e.as_arg().into());
    }

    if inv.exclude_dynamic_sections {
        argv.push("--exclude-dynamic-system-prompt-sections".into());
    }

    if let Some(sys) = &inv.system {
        argv.push("--system-prompt".into());
        argv.push(sys.clone());
    }

    argv.extend(inv.extra_args.iter().cloned());

    argv
}

pub struct RealSubprocessRunner;

#[async_trait]
impl SubprocessRunner for RealSubprocessRunner {
    async fn run(&self, inv: SubprocessInvocation) -> Result<Value, ProviderError> {
        // Read the streaming flag ONCE and thread it consistently into
        // both argv construction and the execution-path branch below.
        // Reading `streaming_enabled()` twice is a TOCTOU race: if
        // `TARS_CLAUDE_CLI_STREAM` flips between the two reads the child
        // is spawned with `--output-format json` but parsed as
        // `stream-json` (or vice-versa), corrupting the result.
        let streaming = streaming_enabled();

        let mut cmd = Command::new(&inv.executable);
        for tok in build_argv_with(&inv, streaming) {
            cmd.arg(tok);
        }

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

        // Write the prompt on stdin and close it.
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(inv.prompt.as_bytes()).await.map_err(|e| {
                ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("stdin write failed: {e}"),
                }
            })?;
            // dropping `stdin` here closes the pipe so the child sees EOF
            drop(stdin);
        }

        // Streaming branch — `TARS_CLAUDE_CLI_STREAM=1`: read stdout line
        // by line as NDJSON events, tee a pretty per-event summary to
        // stderr, return the reconstructed `result` event so callers see
        // the same shape as buffered mode.
        if streaming {
            return run_streaming(&mut child, &inv).await;
        }

        // Wait with timeout.
        let output = match tokio::time::timeout(inv.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("wait failed: {e}"),
                });
            }
            Err(_) => {
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!(
                        "timed out after {}s (model={}, prompt_chars={})",
                        inv.timeout.as_secs(),
                        inv.model,
                        inv.prompt.len()
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // UTF-8-safe truncation — byte-indexing `[..500]` panics if
            // byte 500 lands mid-codepoint (stderr can carry arbitrary
            // Unicode: paths, user messages).
            let truncated = truncate(&stderr, 500);
            return Err(ProviderError::CliSubprocessDied {
                exit_code: output.status.code(),
                stderr: truncated,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let payload: Value = serde_json::from_str(&stdout).map_err(|e| {
            ProviderError::Parse(format!(
                "claude CLI non-JSON stdout: {e} (first 300: {})",
                truncate(&stdout, 300)
            ))
        })?;

        if !payload.is_object() {
            return Err(ProviderError::Parse(format!(
                "claude CLI returned non-object JSON ({:?})",
                payload
            )));
        }

        if payload
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let detail = payload
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("<no detail>")
                .to_string();
            return Err(ProviderError::CliSubprocessDied {
                exit_code: Some(0), // CLI signaled error in payload, not via exit code
                stderr: format!("claude CLI returned error: {}", truncate(&detail, 300)),
            });
        }

        Ok(payload)
    }
}

/// Flatten our message history into the single text blob the CLI expects.
/// Mirrors the Python `chat_multi` serializer ([role]\n content per turn).
fn serialize_messages_for_cli(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len());
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

/// CLI puts the response in `.result`. Python uses `payload.get("result") or ""`
/// to coerce JSON-null to empty string — same behavior here.
fn extract_result_text(payload: &Value) -> String {
    payload
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn extract_usage(payload: &Value) -> Usage {
    let usage = match payload.get("usage").and_then(|u| u.as_object()) {
        Some(u) => u,
        None => return Usage::default(),
    };
    Usage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        thinking_tokens: 0,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed = crate::http_base::truncate_utf8(s, max);
    if trimmed.len() == s.len() {
        s.to_string()
    } else {
        format!("{trimmed}…")
    }
}

/// Drive `claude -p --output-format stream-json` and stream events to
/// stderr while reconstructing the `result` event for the return value.
///
/// `claude` emits NDJSON: one JSON object per line, one of:
///   - `system/init`, `system/status`               — lifecycle
///   - `rate_limit_event`                            — quota
///   - `stream_event/message_start`                  — API responded
///   - `stream_event/content_block_start|stop`       — thinking/text/tool boundary
///   - `stream_event/content_block_delta`            — partial chunks
///   - `stream_event/message_delta|message_stop`     — usage / done
///   - `assistant`                                   — assembled message
///   - `result`                                      — final aggregate (THE return value)
///
/// On EOF without a `result` event we fail loud — that's broken-invariant
/// territory, never a silent empty Value.
async fn run_streaming(
    child: &mut tokio::process::Child,
    inv: &SubprocessInvocation,
) -> Result<Value, ProviderError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::CliSubprocessDied {
            exit_code: None,
            stderr: "stream-json: stdout pipe missing on spawned child".into(),
        })?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ProviderError::CliSubprocessDied {
            exit_code: None,
            stderr: "stream-json: stderr pipe missing on spawned child".into(),
        })?;

    // Drain stderr in a separate task so the child can't block on a full
    // pipe (claude prints rate limit / debug to stderr).
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        BufReader::new(stderr_pipe).read_to_end(&mut buf).await.ok();
        buf
    });

    // Reader for stdout NDJSON events.
    let mut reader = BufReader::new(stdout).lines();
    let mut final_result: Option<Value> = None;
    let mut session_short: String = "????????".into();

    let read_fut = async {
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    let parsed: Result<Value, _> = serde_json::from_str(&line);
                    match parsed {
                        Ok(ev) => {
                            // Capture short session id from the init event
                            // so subsequent log lines are correlatable.
                            if session_short == "????????" {
                                if let Some(sid) = ev.get("session_id").and_then(|v| v.as_str()) {
                                    session_short = sid.chars().take(8).collect();
                                }
                            }
                            emit_event_summary(&ev, &session_short);
                            if ev.get("type").and_then(|v| v.as_str()) == Some("result") {
                                final_result = Some(ev);
                            }
                        }
                        Err(_) => {
                            // Non-JSON line on stdout — claude shouldn't
                            // emit these in stream-json mode, but if it
                            // does, surface them rather than swallowing.
                            eprintln!(
                                "[claude_cli {session_short}] !! non-json stdout line: {}",
                                truncate(&line, 200)
                            );
                        }
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    return Err(ProviderError::CliSubprocessDied {
                        exit_code: None,
                        stderr: format!("stream read failed: {e}"),
                    });
                }
            }
        }
        Ok::<(), ProviderError>(())
    };

    let wait_fut = tokio::time::timeout(inv.timeout, child.wait());

    let (read_res, wait_res) = tokio::join!(read_fut, wait_fut);
    let stderr_buf = stderr_task.await.unwrap_or_default();

    read_res?;

    let status = match wait_res {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("wait failed: {e}"),
            });
        }
        Err(_) => {
            return Err(ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!(
                    "timed out after {}s (model={}, prompt_chars={})",
                    inv.timeout.as_secs(),
                    inv.model,
                    inv.prompt.len()
                ),
            });
        }
    };

    if !status.success() {
        let stderr_s = String::from_utf8_lossy(&stderr_buf).to_string();
        // UTF-8-safe truncation — see the buffered path; `[..500]` can
        // panic on a multi-byte boundary.
        let truncated = truncate(&stderr_s, 500);
        return Err(ProviderError::CliSubprocessDied {
            exit_code: status.code(),
            stderr: truncated,
        });
    }

    // Strip the `type` wrapper from the result event so callers see the
    // same shape as buffered `--output-format json` mode (which returns
    // the result object directly, not wrapped in {type: "result", ...}).
    let mut result = final_result.ok_or_else(|| {
        ProviderError::Parse(
            "stream-json mode: child exited without emitting a `result` event".into(),
        )
    })?;
    if let Some(obj) = result.as_object_mut() {
        obj.remove("type");
    }

    if !result.is_object() {
        return Err(ProviderError::Parse(format!(
            "stream-json result is not an object: {result:?}"
        )));
    }

    if result
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let detail = result
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("(no detail in result event)");
        return Err(ProviderError::CliSubprocessDied {
            exit_code: status.code(),
            stderr: format!("claude reported is_error: {detail}"),
        });
    }

    Ok(result)
}

/// One-line stderr summary for a stream-json event. Mirrors the
/// information a user would see watching `claude` interactively (init,
/// thinking, text generation, result). Designed to be cheap (no
/// allocation when stdlib formatting suffices) and human-readable.
fn emit_event_summary(ev: &Value, sid: &str) {
    let evtype = ev.get("type").and_then(|v| v.as_str()).unwrap_or("?");

    match evtype {
        "system" => {
            let sub = ev.get("subtype").and_then(|v| v.as_str()).unwrap_or("?");
            let model = ev.get("model").and_then(|v| v.as_str()).unwrap_or("");
            if sub == "init" {
                eprintln!("[claude_cli {sid}] init model={model}");
            } else {
                let status = ev.get("status").and_then(|v| v.as_str()).unwrap_or("");
                eprintln!("[claude_cli {sid}] {sub} {status}");
            }
        }
        "rate_limit_event" => {
            let status = ev
                .get("rate_limit_info")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            eprintln!("[claude_cli {sid}] rate_limit {status}");
        }
        "stream_event" => {
            let inner = ev
                .get("event")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            match inner {
                "message_start" => {
                    let ttft = ev.get("ttft_ms").and_then(|v| v.as_i64()).unwrap_or(-1);
                    eprintln!("[claude_cli {sid}] message_start ttft={ttft}ms");
                }
                "content_block_start" => {
                    let kind = ev
                        .get("event")
                        .and_then(|v| v.get("content_block"))
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    eprintln!("[claude_cli {sid}] block_start kind={kind}");
                }
                "content_block_delta" => {
                    let delta = ev.get("event").and_then(|v| v.get("delta"));
                    let dtype = delta
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    // Pull whichever field carries the payload for this delta variant.
                    let chunk = delta
                        .and_then(|v| {
                            v.get("thinking")
                                .or_else(|| v.get("text"))
                                .or_else(|| v.get("partial_json"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("");
                    if !chunk.is_empty() {
                        eprintln!("[claude_cli {sid}] {dtype}: {}", truncate(chunk, 200));
                    }
                }
                "content_block_stop" => { /* low-signal, skip */ }
                "message_delta" => { /* usage update, low-signal mid-call */ }
                "message_stop" => {
                    eprintln!("[claude_cli {sid}] message_stop");
                }
                other => {
                    eprintln!("[claude_cli {sid}] stream_event/{other}");
                }
            }
        }
        "assistant" | "user" => { /* fully-assembled message — already streamed via deltas */ }
        "result" => {
            let dur = ev.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(-1);
            let usage = ev.get("usage");
            let tin = usage
                .and_then(|v| v.get("input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let tout = usage
                .and_then(|v| v.get("output_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let tcached = usage
                .and_then(|v| v.get("cache_read_input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cost = ev
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let subtype = ev.get("subtype").and_then(|v| v.as_str()).unwrap_or("?");
            eprintln!(
                "[claude_cli {sid}] result {subtype} dur={dur}ms in={tin} out={tout} cached={tcached} cost=${cost:.4}"
            );
        }
        other => {
            eprintln!("[claude_cli {sid}] {other}");
        }
    }
}

/// Convenience builder — the most common path.
pub fn claude_cli(id: impl Into<ProviderId>) -> Arc<ClaudeCliProvider> {
    ClaudeCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_types::ModelHint;

    /// Test runner that returns a canned payload and records the invocation.
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

    fn make_provider(payload: Value) -> (Arc<ClaudeCliProvider>, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            payload,
            recorded: std::sync::Mutex::new(None),
        });
        let p = ClaudeCliProviderBuilder::new("claude_cli_test").build_with_runner(runner.clone());
        (p, runner)
    }

    #[tokio::test]
    async fn happy_path_returns_text_and_usage() {
        let payload = json!({
            "result": "hello from claude",
            "is_error": false,
            "usage": {
                "input_tokens": 12,
                "output_tokens": 5,
                "cache_read_input_tokens": 3
            }
        });
        let (provider, runner) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "hello from claude");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 5);
        assert_eq!(resp.usage.cached_input_tokens, 3);

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "opus");
        // env strip list is correctly populated case-insensitively
        assert!(inv.stripped_env.contains("ANTHROPIC_API_KEY"));
        assert!(inv.stripped_env.contains("CLAUDE_CODE_USE_BEDROCK"));
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
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
        let provider = ClaudeCliProviderBuilder::new("c").build_with_runner(Arc::new(ErrRunner));
        let err = match provider
            .clone()
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "x"),
                RequestContext::test_default(),
            )
            .await
        {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::CliSubprocessDied { .. }));
    }

    #[tokio::test]
    async fn null_result_becomes_empty_text() {
        let payload = json!({"result": null, "is_error": false});
        let (provider, _) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "");
    }

    #[tokio::test]
    async fn truncates_when_max_output_tokens_exceeded() {
        let big = "x".repeat(1000);
        let payload = json!({"result": big, "is_error": false});
        let (provider, _) = make_provider(payload);
        let mut req = ChatRequest::user(ModelHint::Explicit("opus".into()), "hi");
        req.max_output_tokens = Some(10); // ~40 chars at 4 chars/token
        let resp = provider
            .complete(req, RequestContext::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text.len(), 40);
    }

    #[tokio::test]
    async fn system_prompt_propagates_into_invocation() {
        let payload = json!({"result": "ok", "is_error": false});
        let (provider, runner) = make_provider(payload);
        let _ = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "x").with_system("be brief"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.system.as_deref(), Some("be brief"));
    }

    #[test]
    fn message_serializer_preserves_role_tags() {
        let req = ChatRequest {
            model: ModelHint::Explicit("x".into()),
            system: None,
            messages: vec![
                Message::user_text("first user turn"),
                Message::assistant_text("first assistant"),
                Message::user_text("second user turn"),
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
            enable_chat_template_thinking: None,
        };
        let s = serialize_messages_for_cli(&req);
        assert!(s.contains("[user]\nfirst user turn"));
        assert!(s.contains("[assistant]\nfirst assistant"));
        assert!(s.contains("[user]\nsecond user turn"));
    }

    #[test]
    fn env_strip_list_is_uppercase_for_case_insensitive_match() {
        for k in STRIPPED_ENV_KEYS_UPPER {
            assert_eq!(*k, k.to_uppercase());
        }
        // The list MUST include ANTHROPIC_API_KEY — that's the entire
        // point of CLI-mode auth.
        assert!(STRIPPED_ENV_KEYS_UPPER.contains(&"ANTHROPIC_API_KEY"));
    }

    // ─── argv-shape tests ──────────────────────────────────────────────
    //
    // The point of these tests is *not* to assert that the CLI does what
    // we hope. The point is: when Anthropic renames `--tools` to
    // `--enabled-tools` (or whatever), exactly the tests below break,
    // they break loudly, and we know to update one constant in one place.

    fn make_invocation(
        configure: impl FnOnce(ClaudeCliProviderBuilder) -> ClaudeCliProviderBuilder,
    ) -> SubprocessInvocation {
        // Record-only runner — doesn't return a real payload, but
        // captures the SubprocessInvocation that build_argv consumed.
        struct RecRunner(std::sync::Mutex<Option<SubprocessInvocation>>);
        #[async_trait]
        impl SubprocessRunner for RecRunner {
            async fn run(&self, inv: SubprocessInvocation) -> Result<Value, ProviderError> {
                *self.0.lock().unwrap() = Some(inv);
                Ok(json!({"result": "", "is_error": false}))
            }
        }
        let runner = Arc::new(RecRunner(std::sync::Mutex::new(None)));
        let provider =
            configure(ClaudeCliProviderBuilder::new("test")).build_with_runner(runner.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = provider
                .complete(
                    ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                    RequestContext::test_default(),
                )
                .await;
        });
        runner.0.lock().unwrap().clone().unwrap()
    }

    /// Helper: find argv index of a flag token; panic if not present.
    fn idx(argv: &[String], flag: &str) -> usize {
        argv.iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("flag {flag:?} not in argv: {argv:?}"))
    }

    #[test]
    fn argv_default_is_pure_inference_subscription_friendly() {
        let inv = make_invocation(|b| b);
        let argv = build_argv(&inv);

        // Required backbone — unchanged from the pre-Builder-overhaul shape.
        assert_eq!(argv[0], "-p");
        assert_eq!(argv[1], "-");
        let m = idx(&argv, "--model");
        assert_eq!(argv[m + 1], "opus");
        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "json");
        assert!(argv.iter().any(|a| a == "--disable-slash-commands"));

        // Default tools = Disabled → "--tools" followed by literal "".
        let t = idx(&argv, "--tools");
        assert_eq!(
            argv[t + 1],
            "",
            "tools=Disabled must yield literal empty-string value, not omission or sentinel"
        );

        // Default bare = false → no --bare token.
        assert!(
            !argv.iter().any(|a| a == "--bare"),
            "bare default must be false to preserve OAuth/keychain auth"
        );

        // Default effort = None → no --effort token.
        assert!(!argv.iter().any(|a| a == "--effort"));

        // Default exclude_dynamic_sections = true.
        assert!(
            argv.iter()
                .any(|a| a == "--exclude-dynamic-system-prompt-sections")
        );

        // No stray extra_args.
        assert_eq!(
            argv.last().map(|s| s.as_str()),
            Some("--exclude-dynamic-system-prompt-sections")
        );
    }

    #[test]
    fn argv_streaming_true_switches_format_and_adds_partial_msg_verbose() {
        // Pass `streaming = true` directly — no env mutation. (Workspace
        // forbids `unsafe`; Rust 2024 makes env::set_var `unsafe`.)
        let inv = make_invocation(|b| b);
        let argv = build_argv_with(&inv, true);

        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "stream-json");

        // claude requires --verbose for the per-event stream to emit
        // alongside stream-json; --include-partial-messages is what
        // adds the text_delta / thinking_delta chunks.
        assert!(argv.iter().any(|a| a == "--include-partial-messages"));
        assert!(argv.iter().any(|a| a == "--verbose"));
    }

    #[test]
    fn argv_streaming_false_keeps_buffered_json_format() {
        let inv = make_invocation(|b| b);
        let argv = build_argv_with(&inv, false);

        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "json");
        assert!(!argv.iter().any(|a| a == "--include-partial-messages"));
        assert!(!argv.iter().any(|a| a == "--verbose"));
    }

    #[test]
    fn argv_tools_default_omits_flag() {
        let inv = make_invocation(|b| b.tools(ClaudeCliTools::Default));
        let argv = build_argv(&inv);
        assert!(
            !argv.iter().any(|a| a == "--tools"),
            "tools=Default must omit --tools entirely (lets CLI use its own default)"
        );
    }

    #[test]
    fn argv_tools_allow_serializes_as_csv() {
        let inv = make_invocation(|b| {
            b.tools(ClaudeCliTools::Allow(vec![
                "Read".into(),
                "Bash(git *)".into(),
            ]))
        });
        let argv = build_argv(&inv);
        let t = idx(&argv, "--tools");
        assert_eq!(argv[t + 1], "Read,Bash(git *)");
    }

    #[test]
    fn argv_bare_opt_in_adds_flag() {
        let inv = make_invocation(|b| b.bare(true));
        let argv = build_argv(&inv);
        assert!(argv.iter().any(|a| a == "--bare"));
    }

    #[test]
    fn argv_effort_renders_each_variant() {
        for (variant, expected) in [
            (ClaudeCliEffort::Low, "low"),
            (ClaudeCliEffort::Medium, "medium"),
            (ClaudeCliEffort::High, "high"),
            (ClaudeCliEffort::Xhigh, "xhigh"),
            (ClaudeCliEffort::Max, "max"),
        ] {
            let inv = make_invocation(|b| b.effort(Some(variant)));
            let argv = build_argv(&inv);
            let e = idx(&argv, "--effort");
            assert_eq!(argv[e + 1], expected, "wrong arg for {variant:?}");
        }
    }

    #[test]
    fn argv_exclude_dynamic_sections_opt_out_drops_flag() {
        let inv = make_invocation(|b| b.exclude_dynamic_sections(false));
        let argv = build_argv(&inv);
        assert!(
            !argv
                .iter()
                .any(|a| a == "--exclude-dynamic-system-prompt-sections"),
            "exclude_dynamic_sections=false must drop the flag"
        );
    }

    #[test]
    fn argv_extra_args_appended_verbatim_at_end() {
        let inv = make_invocation(|b| {
            b.extra_args(vec![
                "--betas".into(),
                "experimental-x".into(),
                "--debug".into(),
            ])
        });
        let argv = build_argv(&inv);
        let last_three = &argv[argv.len() - 3..];
        assert_eq!(last_three, &["--betas", "experimental-x", "--debug"]);
    }

    #[test]
    fn argv_system_prompt_appears_when_set() {
        // Direct construction — we already proved propagation in
        // `system_prompt_propagates_into_invocation` above.
        let inv = SubprocessInvocation {
            executable: "claude".into(),
            model: "opus".into(),
            system: Some("be brief".into()),
            prompt: String::new(),
            timeout: Duration::from_secs(1),
            stripped_env: HashSet::new(),
            tools: ClaudeCliTools::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: vec![],
        };
        let argv = build_argv(&inv);
        let sp = idx(&argv, "--system-prompt");
        assert_eq!(argv[sp + 1], "be brief");
    }
}
