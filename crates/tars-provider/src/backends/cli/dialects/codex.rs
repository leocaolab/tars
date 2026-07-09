//! [`CodexCliDialect`] — the codex-cli behavior expressed as a
//! [`CliDialect`] (Doc 32 §5 C3, M1). Reproduces the pre-migration
//! `codex_cli` behavior byte-for-byte: the `codex exec --json --model X
//! --sandbox <mode> -c approval_policy="never" [--skip-git-repo-check] -`
//! argv (prompt on stdin), codex's JSONL [`ThreadEvent`] stream, and the
//! `agent_message`/`reasoning`/`turn.completed` → `ChatEvent` mapping.
//!
//! ## M1 changes
//! - codex's **private spawn/stream loop** (`codex_cli.rs:253-342`) is retired;
//!   codex now spawns through the shared
//!   [`SharedCliRunner`](super::super::subprocess::SharedCliRunner) + OS-jail
//!   primitive, so it gets the `tars-sandbox` write-jail **on top of** its own
//!   `--sandbox` flag (defense-in-depth — the `--sandbox` token stays in the argv).
//! - The shared runner buffers codex's JSONL (declared
//!   [`OutputFraming::JsonLinesArray`]) into a `Value::Array` of raw lines that
//!   [`CodexCliDialect::parse_line`] maps through [`map_thread_event`] — the
//!   same per-line translation as before.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use tars_types::{
    ChatEvent, ChatRequest, ContentBlock, Message, ProviderError, RequestContext, StopReason, Usage,
};

use super::super::argv::SubprocessInvocation;
use super::super::dialect::{CliDialect, CliInvocation, OutputFraming, OutputMode, PromptChannel};
use super::super::subprocess::truncate;

/// Env vars that must NEVER leak into the child `codex` process —
/// case-insensitive match. If any survives, codex's auth manager picks it up
/// and routes the request through API billing instead of the user's ChatGPT
/// subscription.
pub(crate) const STRIPPED_ENV_KEYS_UPPER: &[&str] =
    &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AGENT_IDENTITY"];

/// Sandbox modes accepted by `codex exec --sandbox`. Default is `ReadOnly` for
/// the principle-of-least-surprise: a TARS user shouldn't get unexpected file
/// mutations from spawning a Worker. Override with
/// [`CodexCliProviderBuilder::sandbox`](crate::backends::codex_cli::CodexCliProviderBuilder::sandbox).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

/// Codex CLI dialect. Holds the per-invocation codex configuration
/// (executable, timeout, `--sandbox` mode, git-repo gate) that `codex_cli`'s
/// builder used to keep on the provider.
#[derive(Clone, Debug)]
pub struct CodexCliDialect {
    executable: String,
    timeout: Duration,
    sandbox: SandboxMode,
    skip_git_repo_check: bool,
}

impl CodexCliDialect {
    pub fn new(
        executable: String,
        timeout: Duration,
        sandbox: SandboxMode,
        skip_git_repo_check: bool,
    ) -> Self {
        Self {
            executable,
            timeout,
            sandbox,
            skip_git_repo_check,
        }
    }
}

/// Construct the full `codex` argv (without the executable), used by
/// [`CodexCliDialect::argv`] (which the shared runner calls) so the flag shape
/// lives in exactly one place. **Keeps codex's own `--sandbox` flag** — that is
/// codex's INTERNAL sandbox; the tars-sandbox OS jail wraps the process on top.
pub(crate) fn build_codex_argv(
    model: &str,
    sandbox: SandboxMode,
    skip_git_repo_check: bool,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "exec".into(),
        "--json".into(),
        "--model".into(),
        model.to_string(),
        "--sandbox".into(),
        sandbox.as_arg().into(),
        // Force non-interactive mode: any approval-required action becomes an
        // immediate failure returned to the model rather than a prompt. codex
        // 0.128 has no `--ask-for-approval` flag — `-c approval_policy="never"`
        // is the supported path.
        "-c".into(),
        "approval_policy=\"never\"".into(),
    ];
    if skip_git_repo_check {
        argv.push("--skip-git-repo-check".into());
    }
    // `-` tells codex to read the prompt from stdin.
    argv.push("-".into());
    argv
}

impl CliDialect for CodexCliDialect {
    fn argv(&self, inv: &CliInvocation) -> Vec<String> {
        build_codex_argv(&inv.model, self.sandbox, self.skip_git_repo_check)
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &RequestContext,
    ) -> Result<CliInvocation, ProviderError> {
        let model = model.to_string();

        let prompt = serialize_messages_for_cli(req);

        Ok(SubprocessInvocation::neutral(
            self.executable.clone(),
            model,
            prompt,
            self.timeout,
            STRIPPED_ENV_KEYS_UPPER.iter().map(|s| s.to_string()).collect(),
            ctx.cwd.clone(),
            ctx.sandbox.clone(),
        ))
    }

    fn prompt_channel(&self) -> PromptChannel {
        // `codex exec … -` — the prompt is written to stdin.
        PromptChannel::Stdin
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::JsonEvents
    }

    fn output_framing(&self) -> OutputFraming {
        // codex emits a JSONL event stream; the shared runner buffers it into a
        // `Value::Array` of raw lines that `parse_line` maps per event.
        OutputFraming::JsonLinesArray
    }

    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        // codex's own home (`$CODEX_HOME`, default `~/.codex`) holds its config,
        // sessions and logs. Its app-server socket lives under `$TMPDIR`, which
        // the delegate spawn grants centrally — that socket was the live
        // `Operation not permitted (os error 1)` under the old TMPDIR-in-worktree
        // jail. Grant `~/.codex` too (skipped if absent).
        let home = std::env::var_os("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| tars_types::env::home_dir().map(|h| h.join(".codex")));
        home.into_iter().collect()
    }

    fn parse_line(&self, raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        // The runner reconstructs codex's JSONL stream into a `Value::Array` of
        // raw lines (each a JSON string). Map each line through the same
        // translation the pre-migration streaming path used: skip blank /
        // unknown lines, surface a malformed CRITICAL event, and stop at the
        // first error. The backend prepends `Started`, so we own only the
        // content + terminal `Finished`.
        let lines = raw.as_array().ok_or_else(|| {
            ProviderError::Parse(format!(
                "codex runner payload must be a JSONL array, got: {}",
                truncate(&raw.to_string(), 200)
            ))
        })?;

        let mut out: Vec<ChatEvent> = Vec::new();
        for line_val in lines {
            let line = line_val.as_str().unwrap_or_default();
            if line.trim().is_empty() {
                continue;
            }
            let event: ThreadEvent = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(e) => {
                    // Leniency is for *unknown* event types (codex may add
                    // variants we don't model) — those we skip. But a malformed
                    // *critical* event (turn.completed carries usage;
                    // turn.failed/error carry the failure cause) must not be
                    // silently dropped. Peek the `type` tag to tell them apart.
                    let kind = serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string));
                    match kind.as_deref() {
                        Some("turn.completed") | Some("turn.failed") | Some("error") => {
                            return Err(ProviderError::CliSubprocessDied {
                                exit_code: None,
                                stderr: format!(
                                    "codex: failed to parse critical `{}` event: {e}",
                                    kind.as_deref().unwrap_or("?"),
                                ),
                            });
                        }
                        _ => {
                            tracing::debug!(
                                line = %truncate(line, 200),
                                error = %e,
                                "codex_cli: skipping unparseable line",
                            );
                            continue;
                        }
                    }
                }
            };
            for ev in map_thread_event(event) {
                // On the first error, propagate it (matching the pre-migration
                // stream, which yielded the error and returned).
                out.push(ev?);
            }
        }
        Ok(out)
    }
}

// ── ThreadEvent → ChatEvent translation ───────────────────────────────

/// Mirror of codex's `ThreadEvent` surface — only the fields v1 consumes.
/// Unknown extra fields are ignored (`deny_unknown_fields` deliberately NOT
/// set so codex can add new event types or fields without breaking us).
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
    #[serde(other)]
    Other,
}

/// Translate one codex JSONL event into 0..N TARS ChatEvents. Pure function —
/// exposed for unit testing.
fn map_thread_event(event: ThreadEvent) -> Vec<Result<ChatEvent, ProviderError>> {
    match event {
        ThreadEvent::ThreadStarted {} => vec![],
        ThreadEvent::TurnStarted {} => vec![],
        ThreadEvent::ItemStarted { item: _ } => vec![],
        ThreadEvent::ItemUpdated { item: _ } => vec![],
        ThreadEvent::ItemCompleted { item } => match item.details {
            ThreadItemDetails::AgentMessage { text } if !text.is_empty() => {
                vec![Ok(ChatEvent::Delta { text })]
            }
            ThreadItemDetails::Reasoning { text } if !text.is_empty() => {
                vec![Ok(ChatEvent::ThinkingDelta { text })]
            }
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
    // codex's tokens are i64 (its TS-friendly representation); ours are u64.
    // Negatives shouldn't occur — clamp defensively.
    let to_u64 = |v: i64| if v < 0 { 0 } else { v as u64 };
    Usage {
        input_tokens: to_u64(u.input_tokens),
        output_tokens: to_u64(u.output_tokens),
        cached_input_tokens: to_u64(u.cached_input_tokens),
        cache_creation_tokens: 0, // codex doesn't model this
        thinking_tokens: to_u64(u.reasoning_output_tokens),
    }
}

/// Flatten our message history into the single text blob `codex exec` reads
/// from stdin. Same `[role]\n content` shape as the other CLI delegates.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    

    fn dialect() -> CodexCliDialect {
        CodexCliDialect::new(
            "codex".into(),
            Duration::from_secs(600),
            SandboxMode::ReadOnly,
            true,
        )
    }

    fn parse(json_line: serde_json::Value) -> ThreadEvent {
        serde_json::from_value(json_line).expect("test JSON should be a valid ThreadEvent")
    }

    #[test]
    fn argv_keeps_codex_sandbox_flag_and_shape() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("hi"),
                "gpt-5", &RequestContext::test_default(),
            )
            .unwrap();
        let argv = d.argv(&inv);
        // `codex exec --json --model gpt-5 --sandbox read-only -c
        // approval_policy="never" --skip-git-repo-check -`
        assert_eq!(argv[0], "exec");
        assert_eq!(argv[1], "--json");
        assert_eq!(argv[2], "--model");
        assert_eq!(argv[3], "gpt-5");
        // codex's OWN sandbox flag MUST stay (tars-sandbox wraps on top).
        let s = argv.iter().position(|a| a == "--sandbox").expect("--sandbox present");
        assert_eq!(argv[s + 1], "read-only");
        assert!(argv.iter().any(|a| a == "--skip-git-repo-check"));
        assert_eq!(argv.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn argv_sandbox_mode_variants_pin_to_codex_values() {
        assert_eq!(SandboxMode::ReadOnly.as_arg(), "read-only");
        assert_eq!(SandboxMode::WorkspaceWrite.as_arg(), "workspace-write");
        assert_eq!(SandboxMode::DangerFullAccess.as_arg(), "danger-full-access");
    }

    #[test]
    fn argv_omits_skip_git_when_disabled() {
        let d = CodexCliDialect::new("codex".into(), Duration::from_secs(1), SandboxMode::ReadOnly, false);
        let argv = build_codex_argv("gpt-5", d.sandbox, d.skip_git_repo_check);
        assert!(!argv.iter().any(|a| a == "--skip-git-repo-check"));
    }

    #[test]
    fn channel_is_stdin_and_mode_is_json_events() {
        let d = dialect();
        assert_eq!(d.prompt_channel(), PromptChannel::Stdin);
        assert_eq!(d.output_mode(), OutputMode::JsonEvents);
    }

    #[test]
    fn invocation_strips_env() {
        // (The old `invocation_requires_explicit_model` assertion is gone:
        // the model is now a concrete `&str` arg, so there is no
        // non-explicit-model rejection path. The env-strip contract below
        // is the still-relevant part of the test.)
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("x"),
                "gpt-5-codex", &RequestContext::test_default(),
            )
            .unwrap();
        assert_eq!(inv.model, "gpt-5-codex");
        for k in ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AGENT_IDENTITY"] {
            assert!(inv.stripped_env.contains(k), "stripped_env must contain {k}");
        }
    }

    #[test]
    fn serializer_includes_system_then_each_role_block() {
        let inv = dialect()
            .invocation(
                &ChatRequest {
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
                    enable_chat_template_thinking: None,
                },
                "gpt-5-codex", &RequestContext::test_default(),
            )
            .unwrap();
        assert!(inv.prompt.starts_with("[system]\nbe brief\n\n[user]\nfirst user\n\n"));
        assert!(inv.prompt.contains("[assistant]\nfirst assistant"));
        assert!(inv.prompt.ends_with("[user]\nsecond user"));
    }

    // ── parse_line over a JSONL array (the runner's reconstructed shape) ──

    fn parse_line(lines: &[&str]) -> Result<Vec<ChatEvent>, ProviderError> {
        let arr = Value::Array(lines.iter().map(|l| Value::String(l.to_string())).collect());
        dialect().parse_line(&arr)
    }

    #[test]
    fn parse_line_yields_delta_then_finished() {
        let events = parse_line(&[
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"hi"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":0}}"#,
        ])
        .unwrap();
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "hi"));
        match &events[1] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 2);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_separates_thinking_from_text() {
        let events = parse_line(&[
            r#"{"type":"item.completed","item":{"id":"i1","type":"reasoning","text":"thinking…"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"answer"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":5}}"#,
        ])
        .unwrap();
        assert!(matches!(&events[0], ChatEvent::ThinkingDelta { text } if text == "thinking…"));
        assert!(matches!(&events[1], ChatEvent::Delta { text } if text == "answer"));
        match &events[2] {
            ChatEvent::Finished { usage, .. } => assert_eq!(usage.thinking_tokens, 5),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_drops_internal_tool_events_and_blank_garbage() {
        let events = parse_line(&[
            "",
            "   ",
            "this is not json",
            r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"ls","aggregated_output":"foo","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"a1","type":"agent_message","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}"#,
        ])
        .unwrap();
        // Only the agent_message Delta + Finished survive.
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "done"));
    }

    #[test]
    fn parse_line_turn_failed_surfaces_error() {
        let err = parse_line(&[r#"{"type":"turn.failed","error":{"message":"context too long"}}"#])
            .unwrap_err();
        match err {
            ProviderError::CliSubprocessDied { stderr, .. } => {
                assert!(stderr.contains("context too long"));
                assert!(stderr.contains("turn failed"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_top_level_error_surfaces_error() {
        let err = parse_line(&[r#"{"type":"error","message":"stream broke"}"#]).unwrap_err();
        match err {
            ProviderError::CliSubprocessDied { stderr, .. } => {
                assert!(stderr.contains("stream broke"));
                assert!(stderr.contains("stream error"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }

    // ── map_thread_event unit coverage (pure fn, unchanged from codex_cli) ──

    #[test]
    fn agent_message_completed_yields_one_delta() {
        let out = map_thread_event(parse(json!({
            "type": "item.completed",
            "item": {"id": "i1", "type": "agent_message", "text": "Hello, world."},
        })));
        assert_eq!(out.len(), 1);
        match out.into_iter().next().unwrap().unwrap() {
            ChatEvent::Delta { text } => assert_eq!(text, "Hello, world."),
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn empty_agent_message_drops_silently() {
        let out = map_thread_event(parse(json!({
            "type": "item.completed",
            "item": {"id": "i3", "type": "agent_message", "text": ""},
        })));
        assert!(out.is_empty());
    }

    #[test]
    fn turn_completed_yields_finished_with_converted_usage() {
        let out = map_thread_event(parse(json!({
            "type": "turn.completed",
            "usage": {"input_tokens": 100, "cached_input_tokens": 30, "output_tokens": 50, "reasoning_output_tokens": 20},
        })));
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
    fn unknown_item_kinds_drop_via_serde_other() {
        for kind in ["command_execution", "file_change", "mcp_tool_call", "web_search"] {
            let out = map_thread_event(parse(json!({
                "type": "item.completed",
                "item": {"id": "x", "type": kind, "command": "ls", "aggregated_output": "", "status": "completed"},
            })));
            assert!(out.is_empty(), "item.completed of kind `{kind}` should drop in v1");
        }
    }

    #[test]
    fn negative_usage_tokens_are_clamped_to_zero() {
        let converted = convert_usage(&CodexUsage {
            input_tokens: -5,
            cached_input_tokens: -1,
            output_tokens: 10,
            reasoning_output_tokens: -2,
        });
        assert_eq!(converted.input_tokens, 0);
        assert_eq!(converted.cached_input_tokens, 0);
        assert_eq!(converted.output_tokens, 10);
        assert_eq!(converted.thinking_tokens, 0);
    }
}
