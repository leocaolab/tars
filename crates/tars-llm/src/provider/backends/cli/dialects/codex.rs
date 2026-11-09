//! Talks to `codex exec --json`, the prompt on stdin, and reads its JSON-lines events. codex
//! keeps its own `--sandbox`, inside the `tars-sandbox` jail every CLI runs in.
//!
//! A run whose output has no `turn.completed` yields no `Finished` event.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::ProviderError;
use crate::provider::error::ParseProblem;
use tars_types::{
    Caller, ChatEvent, ChatRequest, ContentBlock, JsonKind, Message, StopReason, Usage,
};

use super::super::cli_subprocess_died;
use super::super::dialect::{CliDialect, OutputFraming, OutputMode, PromptChannel};
use super::super::invocation::SubprocessInvocation;
use super::super::subprocess::truncate;

/// Names the variables never passed to `codex`, upper-cased and matched whatever their case: with
/// any of them codex bills an API account instead of the user's ChatGPT subscription.
pub(crate) const STRIPPED_ENV_KEYS_UPPER: &[&str] =
    &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AGENT_IDENTITY"];

/// The value of `codex exec --sandbox`. The default is read-only, so codex edits no file unless
/// asked to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodexSandbox {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandbox {
    fn as_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

/// The codex settings every run uses.
#[derive(Clone, Debug)]
pub struct CodexCliDialect {
    executable: String,
    timeout: Duration,
    sandbox: CodexSandbox,
    skip_git_repo_check: bool,
}

impl CodexCliDialect {
    pub fn new(
        executable: String,
        timeout: Duration,
        sandbox: CodexSandbox,
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

/// Returns `codex`'s argv, without the executable, ending in `-` to read the prompt from stdin.
pub(crate) fn build_codex_argv(
    model: &str,
    sandbox: CodexSandbox,
    skip_git_repo_check: bool,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "exec".into(),
        "--json".into(),
        "--model".into(),
        model.to_string(),
        "--sandbox".into(),
        sandbox.as_arg().into(),
        // An action that needs approval fails back to the model instead of prompting; codex
        // 0.128 has no `--ask-for-approval` flag.
        "-c".into(),
        "approval_policy=\"never\"".into(),
    ];
    if skip_git_repo_check {
        argv.push("--skip-git-repo-check".into());
    }
    argv.push("-".into());
    argv
}

impl CliDialect for CodexCliDialect {
    fn argv(&self, inv: &SubprocessInvocation) -> Vec<String> {
        build_codex_argv(&inv.model, self.sandbox, self.skip_git_repo_check)
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &Caller,
    ) -> Result<SubprocessInvocation, ProviderError> {
        let model = model.to_string();

        let prompt = serialize_messages_for_cli(req);

        Ok(SubprocessInvocation::neutral(
            self.executable.clone(),
            model,
            prompt,
            ctx.call_budget(self.timeout),
            STRIPPED_ENV_KEYS_UPPER
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ctx.cwd.clone(),
            ctx.sandbox.clone(),
        ))
    }

    fn prompt_channel(&self) -> PromptChannel {
        PromptChannel::Stdin
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::JsonEvents
    }

    fn output_framing(&self) -> OutputFraming {
        OutputFraming::JsonLinesArray
    }

    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        // `$CODEX_HOME`, else `~/.codex`, holds codex's config, sessions and logs; its socket is
        // under `$TMPDIR`, which every jail can write.
        let home = std::env::var_os("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| tars_types::env::home_dir().map(|h| h.join(".codex")));
        home.into_iter().collect()
    }

    fn parse_line(&self, raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        // Blank and unknown lines are skipped; a malformed critical event or an error event
        // fails the call.
        let lines = raw.as_array().ok_or_else(|| ProviderError::Parse {
            at: "codex runner payload",
            line: None,
            problem: Box::new(ParseProblem::WrongType {
                expected: JsonKind::Array,
                got: JsonKind::of(raw),
            }),
            raw: Some(truncate(&raw.to_string(), 200)),
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
                    // Unknown types are skipped, but a malformed event that carries usage or a
                    // failure cause is not.
                    let kind = serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string));
                    match kind.as_deref() {
                        Some("turn.completed") | Some("turn.failed") | Some("error") => {
                            return Err(cli_subprocess_died(
                                None,
                                format!(
                                    "codex: failed to parse critical `{}` event: {e}",
                                    kind.as_deref().unwrap_or("?"),
                                ),
                            ));
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
                out.push(ev?);
            }
        }
        Ok(out)
    }
}

/// The part of codex's `ThreadEvent` read here; other fields are ignored, so codex can add some.
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
    AgentMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// Returns the events of one codex event: none, a text or thinking delta, `Finished`, or an error.
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
            vec![Err(cli_subprocess_died(
                None,
                format!("codex turn failed: {}", error.message),
            ))]
        }
        ThreadEvent::Error(error) => {
            vec![Err(cli_subprocess_died(
                None,
                format!("codex stream error: {}", error.message),
            ))]
        }
    }
}

fn convert_usage(u: &CodexUsage) -> Usage {
    // codex counts in i64; a negative count becomes 0.
    let to_u64 = |v: i64| if v < 0 { 0 } else { v as u64 };
    Usage {
        input_tokens: to_u64(u.input_tokens),
        output_tokens: to_u64(u.output_tokens),
        cached_input_tokens: to_u64(u.cached_input_tokens),
        cache_creation_tokens: 0,
        thinking_tokens: to_u64(u.reasoning_output_tokens),
    }
}

/// Returns the request as one prompt under `[role]` lines, the system prompt first as a
/// `[system]` block.
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
            CodexSandbox::ReadOnly,
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
            .invocation(&ChatRequest::user("hi"), "gpt-5", &Caller::test_default())
            .unwrap();
        let argv = d.argv(&inv);
        assert_eq!(argv[0], "exec");
        assert_eq!(argv[1], "--json");
        assert_eq!(argv[2], "--model");
        assert_eq!(argv[3], "gpt-5");
        let s = argv
            .iter()
            .position(|a| a == "--sandbox")
            .expect("--sandbox present");
        assert_eq!(argv[s + 1], "read-only");
        assert!(argv.iter().any(|a| a == "--skip-git-repo-check"));
        assert_eq!(argv.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn argv_sandbox_mode_variants_pin_to_codex_values() {
        assert_eq!(CodexSandbox::ReadOnly.as_arg(), "read-only");
        assert_eq!(CodexSandbox::WorkspaceWrite.as_arg(), "workspace-write");
        assert_eq!(
            CodexSandbox::DangerFullAccess.as_arg(),
            "danger-full-access"
        );
    }

    #[test]
    fn argv_omits_skip_git_when_disabled() {
        let d = CodexCliDialect::new(
            "codex".into(),
            Duration::from_secs(1),
            CodexSandbox::ReadOnly,
            false,
        );
        let argv = build_codex_argv("gpt-5", d.sandbox, d.skip_git_repo_check);
        assert!(!argv.iter().any(|a| a == "--skip-git-repo-check"));
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
                "gpt-5-codex",
                &Caller::test_default(),
            )
            .unwrap();
        assert!(
            inv.prompt
                .starts_with("[system]\nbe brief\n\n[user]\nfirst user\n\n")
        );
        assert!(inv.prompt.contains("[assistant]\nfirst assistant"));
        assert!(inv.prompt.ends_with("[user]\nsecond user"));
    }

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
        for kind in [
            "command_execution",
            "file_change",
            "mcp_tool_call",
            "web_search",
        ] {
            let out = map_thread_event(parse(json!({
                "type": "item.completed",
                "item": {"id": "x", "type": kind, "command": "ls", "aggregated_output": "", "status": "completed"},
            })));
            assert!(
                out.is_empty(),
                "item.completed of kind `{kind}` should drop in v1"
            );
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
