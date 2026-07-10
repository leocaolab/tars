//! [`AntigravityDialect`] — the `agy` (Antigravity) CLI behavior expressed as
//! a [`CliDialect`] (Doc 32 §5 C3, M3). `agy` is the first **`OutputMode::Text`**
//! delegate: v1.0.16 has NO `--output-format json`, it just prints the plain
//! answer to stdout. The dialect therefore builds the `agy -p "<prompt>"
//! --model <model> --dangerously-skip-permissions --add-dir <cwd>` argv (prompt
//! as the `-p` **arg** value) and maps the drained stdout through
//! [`CliDialect::parse_text`].
//!
//! Like gemini/codex, `agy` spawns through the shared
//! [`SharedCliRunner`](super::super::subprocess::SharedCliRunner) + OS-jail
//! primitive so — a black-box coding agent — it runs inside the same
//! `tars-sandbox` write-jail as the other delegates. This dialect declares only
//! argv + [`OutputFraming::RawText`] + `parse_text`; it has no bespoke runner.
//!
//! ## Auth env passthrough (NOT stripped)
//! `agy` authenticates via `GEMINI_API_KEY` / `ANTIGRAVITY_API_KEY` (or its own
//! OAuth session). Unlike claude/gemini/codex — which STRIP auth env to force a
//! subscription path — antigravity's env-strip table is empty, so those keys
//! pass through untouched (the default `build_sandboxed_command` passes through
//! everything not in `stripped_env`). [`Self::env`] names them for the record.

use std::time::Duration;

use tars_types::{
    ChatEvent, ChatRequest, ContentBlock, Message, ProviderError, RequestContext, StopReason, Usage,
};

use super::super::argv::SubprocessInvocation;
use super::super::dialect::{CliDialect, CliInvocation, OutputFraming, OutputMode, PromptChannel};

/// Auth env `agy` reads. Passed THROUGH (not stripped) — see the module doc.
pub(crate) const PASSTHROUGH_ENV_KEYS: &[&str] = &["GEMINI_API_KEY", "ANTIGRAVITY_API_KEY"];

/// Limit on the rendered prompt length passed via `-p`. Same reasoning as the
/// gemini dialect: the prompt rides in the argv, so cap it well below ARG_MAX
/// and surface a clean `InvalidRequest` rather than let `execve` fail E2BIG.
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// Antigravity (`agy`) CLI dialect. Holds the per-invocation config
/// (executable, timeout); the shared `AgentCliBackend` stays free of any
/// antigravity specifics.
#[derive(Clone, Debug)]
pub struct AntigravityDialect {
    executable: String,
    timeout: Duration,
}

impl AntigravityDialect {
    pub fn new(executable: String, timeout: Duration) -> Self {
        Self {
            executable,
            timeout,
        }
    }
}

/// Construct the full `agy` argv (without the executable), used by
/// [`AntigravityDialect::argv`] (which the shared runner calls). `--add-dir
/// <cwd>` is only emitted when a worktree cwd is present (it needs a real path).
pub(crate) fn build_agy_argv(inv: &SubprocessInvocation) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "-p".into(),
        inv.prompt.clone(),
        "--model".into(),
        inv.model.clone(),
        "--dangerously-skip-permissions".into(),
    ];
    if let Some(cwd) = &inv.cwd {
        argv.push("--add-dir".into());
        argv.push(cwd.to_string_lossy().into_owned());
    }
    argv
}

impl CliDialect for AntigravityDialect {
    fn argv(&self, inv: &CliInvocation) -> Vec<String> {
        build_agy_argv(inv)
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &RequestContext,
    ) -> Result<CliInvocation, ProviderError> {
        let model = model.to_string();

        let prompt = render_prompt_for_cli(req);
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "prompt size {} exceeds agy CLI argv cap {} bytes",
                prompt.len(),
                MAX_PROMPT_BYTES
            )));
        }

        Ok(SubprocessInvocation::neutral(
            self.executable.clone(),
            model,
            prompt,
            ctx.call_budget(self.timeout),
            // Empty strip table: agy's auth env (GEMINI_API_KEY /
            // ANTIGRAVITY_API_KEY) must pass through.
            std::collections::HashSet::new(),
            ctx.cwd.clone(),
            ctx.sandbox.clone(),
        ))
    }

    fn prompt_channel(&self) -> PromptChannel {
        // `agy -p "<prompt>"` — the prompt rides in the argv, not stdin.
        PromptChannel::Arg
    }

    fn output_mode(&self) -> OutputMode {
        // v1.0.16 has no `--output-format json`; it prints the plain answer.
        OutputMode::Text
    }

    fn output_framing(&self) -> OutputFraming {
        // Plain printed answer, no JSON — the shared runner hands the raw stdout
        // to `parse_text` as a `Value::String`.
        OutputFraming::RawText
    }

    fn parse_text(&self, stdout: &str) -> Result<Vec<ChatEvent>, ProviderError> {
        // agy prints the answer followed by a trailing newline; trim it so the
        // Delta carries the clean answer. `trim_end` only — leading whitespace
        // could be meaningful (e.g. a fenced code block).
        let text = stdout.trim_end().to_string();
        Ok(vec![
            ChatEvent::Delta { text },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ])
    }

    fn env(&self) -> &[&str] {
        // Documentary — the passthrough is automatic (empty strip table). Names
        // the env `agy` authenticates with so a reader sees why it's NOT stripped.
        PASSTHROUGH_ENV_KEYS
    }
}

/// Render a chat request as a single string prompt for the CLI. Embeds the
/// system prompt as a leading `[system]` block — `agy -p` has no
/// `--system-prompt` flag. Same shape as the gemini dialect.
fn render_prompt_for_cli(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len() + 1);
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
    use std::path::PathBuf;

    fn dialect() -> AntigravityDialect {
        AntigravityDialect::new("agy".into(), Duration::from_secs(300))
    }

    #[test]
    fn argv_is_agy_shape_with_prompt_as_arg() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("say hi"),
                "gemini-2.5-pro", &RequestContext::test_default(),
            )
            .unwrap();
        let argv = d.argv(&inv);
        // `agy -p "<prompt>" --model <model> --dangerously-skip-permissions`.
        assert_eq!(argv[0], "-p");
        assert!(argv[1].contains("say hi"));
        assert_eq!(argv[2], "--model");
        assert_eq!(argv[3], "gemini-2.5-pro");
        assert_eq!(argv[4], "--dangerously-skip-permissions");
    }

    #[test]
    fn argv_appends_add_dir_when_cwd_present() {
        let d = dialect();
        let mut inv = d
            .invocation(
                &ChatRequest::user("hi"),
                "gemini-2.5-pro", &RequestContext::test_default(),
            )
            .unwrap();
        inv.cwd = Some(PathBuf::from("/tmp/worktree"));
        let argv = d.argv(&inv);
        let i = argv.iter().position(|a| a == "--add-dir").expect("--add-dir present");
        assert_eq!(argv[i + 1], "/tmp/worktree");
    }

    #[test]
    fn argv_omits_add_dir_without_cwd() {
        let d = dialect();
        let mut inv = d
            .invocation(
                &ChatRequest::user("hi"),
                "gemini-2.5-pro", &RequestContext::test_default(),
            )
            .unwrap();
        inv.cwd = None;
        let argv = d.argv(&inv);
        assert!(!argv.iter().any(|a| a == "--add-dir"));
    }

    #[test]
    fn channel_is_arg_and_mode_is_text() {
        let d = dialect();
        assert_eq!(d.prompt_channel(), PromptChannel::Arg);
        assert_eq!(d.output_mode(), OutputMode::Text);
    }

    
    #[test]
    fn invocation_does_not_strip_auth_env() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("x"),
                "gemini-2.5-pro", &RequestContext::test_default(),
            )
            .unwrap();
        // agy authenticates via these — they must NOT be stripped.
        assert!(inv.stripped_env.is_empty());
    }

    #[test]
    fn env_lists_passthrough_auth_keys() {
        let d = dialect();
        assert!(d.env().contains(&"GEMINI_API_KEY"));
        assert!(d.env().contains(&"ANTIGRAVITY_API_KEY"));
    }

    #[test]
    fn oversized_prompt_rejected_with_invalid_request() {
        let d = dialect();
        let big = "x".repeat(MAX_PROMPT_BYTES + 1);
        let err = d
            .invocation(
                &ChatRequest::user(big),
                "gemini-2.5-pro", &RequestContext::test_default(),
            )
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn parse_text_yields_delta_then_finished() {
        let d = dialect();
        let events = d.parse_text("the whole printed answer\n").unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "the whole printed answer"));
        assert!(matches!(
            &events[1],
            ChatEvent::Finished { stop_reason, .. } if *stop_reason == StopReason::EndTurn
        ));
    }

    #[test]
    fn invocation_embeds_system_as_prefix_block() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("x").with_system("be precise"),
                "gemini-2.5-pro", &RequestContext::test_default(),
            )
            .unwrap();
        assert!(inv.prompt.starts_with("[system]\nbe precise"));
        assert!(inv.prompt.contains("[user]\nx"));
    }
}
