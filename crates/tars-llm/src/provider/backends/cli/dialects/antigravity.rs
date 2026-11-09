//! Talks to Antigravity's `agy -p "<prompt>"`, which (as of v1.0.16) has no JSON output and
//! prints the bare answer, so no usage is reported. Its login variables, `GEMINI_API_KEY` and
//! `ANTIGRAVITY_API_KEY`, are passed on: this dialect strips nothing.

use std::time::Duration;

use crate::ProviderError;
use tars_types::{Caller, ChatEvent, ChatRequest, ContentBlock, Message, StopReason, Usage};

use super::super::dialect::{CliDialect, OutputFraming, OutputMode, PromptChannel};
use super::super::invocation::SubprocessInvocation;
use super::super::prompt::check_prompt_arg;

/// Names the login variables `agy` reads.
pub(crate) const PASSTHROUGH_ENV_KEYS: &[&str] = &["GEMINI_API_KEY", "ANTIGRAVITY_API_KEY"];

/// The value of `agy --effort`, which has only these two. It is always passed: `gemini-3.1-pro`
/// refuses a call without it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AntigravityEffort {
    Low,
    High,
}

impl AntigravityEffort {
    fn as_flag(self) -> &'static str {
        match self {
            AntigravityEffort::Low => "low",
            AntigravityEffort::High => "high",
        }
    }
}

/// The `agy` settings every run uses.
#[derive(Clone, Debug)]
pub struct AntigravityDialect {
    executable: String,
    timeout: Duration,
    effort: AntigravityEffort,
}

impl AntigravityDialect {
    pub fn new(executable: String, timeout: Duration, effort: AntigravityEffort) -> Self {
        Self {
            executable,
            timeout,
            effort,
        }
    }
}

/// Returns `agy`'s argv, without the executable; `--add-dir` only when there is a `cwd`.
pub(crate) fn build_agy_argv(inv: &SubprocessInvocation, effort: AntigravityEffort) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "-p".into(),
        inv.prompt.clone(),
        "--model".into(),
        inv.model.clone(),
        "--effort".into(),
        effort.as_flag().into(),
        "--dangerously-skip-permissions".into(),
    ];
    if let Some(cwd) = &inv.cwd {
        argv.push("--add-dir".into());
        argv.push(cwd.to_string_lossy().into_owned());
    }
    argv
}

impl CliDialect for AntigravityDialect {
    fn argv(&self, inv: &SubprocessInvocation) -> Vec<String> {
        build_agy_argv(inv, self.effort)
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &Caller,
    ) -> Result<SubprocessInvocation, ProviderError> {
        let model = model.to_string();

        let prompt = render_prompt_for_cli(req);
        check_prompt_arg(&prompt, "agy")?;

        Ok(SubprocessInvocation::neutral(
            self.executable.clone(),
            model,
            prompt,
            ctx.call_budget(self.timeout),
            std::collections::HashSet::new(),
            ctx.cwd.clone(),
            ctx.sandbox.clone(),
        ))
    }

    fn prompt_channel(&self) -> PromptChannel {
        PromptChannel::Arg
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::Text
    }

    fn output_framing(&self) -> OutputFraming {
        OutputFraming::RawText
    }

    fn parse_text(&self, stdout: &str) -> Result<Vec<ChatEvent>, ProviderError> {
        // Only the end is trimmed: leading whitespace can belong to the answer.
        let text = stdout.trim_end().to_string();
        Ok(vec![
            ChatEvent::Delta { text },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage::default()),
            },
        ])
    }

    fn env(&self) -> &[&str] {
        PASSTHROUGH_ENV_KEYS
    }
}

/// Returns the request as one prompt under `[role]` lines, the system prompt first as a
/// `[system]` block, since `agy -p` has no system-prompt flag.
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
        AntigravityDialect::new(
            "agy".into(),
            Duration::from_secs(300),
            AntigravityEffort::High,
        )
    }

    #[test]
    fn argv_is_agy_shape_with_prompt_as_arg() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("say hi"),
                "gemini-2.5-pro",
                &Caller::test_default(),
            )
            .unwrap();
        let argv = d.argv(&inv);
        assert_eq!(argv[0], "-p");
        assert!(argv[1].contains("say hi"));
        assert_eq!(argv[2], "--model");
        assert_eq!(argv[3], "gemini-2.5-pro");
        assert_eq!(argv[4], "--effort");
        assert_eq!(argv[5], "high");
        assert_eq!(argv[6], "--dangerously-skip-permissions");
    }

    #[test]
    fn argv_appends_add_dir_when_cwd_present() {
        let d = dialect();
        let mut inv = d
            .invocation(
                &ChatRequest::user("hi"),
                "gemini-2.5-pro",
                &Caller::test_default(),
            )
            .unwrap();
        inv.cwd = Some(PathBuf::from("/tmp/worktree"));
        let argv = d.argv(&inv);
        let i = argv
            .iter()
            .position(|a| a == "--add-dir")
            .expect("--add-dir present");
        assert_eq!(argv[i + 1], "/tmp/worktree");
    }

    #[test]
    fn argv_omits_add_dir_without_cwd() {
        let d = dialect();
        let mut inv = d
            .invocation(
                &ChatRequest::user("hi"),
                "gemini-2.5-pro",
                &Caller::test_default(),
            )
            .unwrap();
        inv.cwd = None;
        let argv = d.argv(&inv);
        assert!(!argv.iter().any(|a| a == "--add-dir"));
    }

    #[test]
    fn env_lists_passthrough_auth_keys() {
        let d = dialect();
        assert!(d.env().contains(&"GEMINI_API_KEY"));
        assert!(d.env().contains(&"ANTIGRAVITY_API_KEY"));
    }

    #[test]
    fn parse_text_yields_delta_then_finished() {
        let d = dialect();
        let events = d.parse_text("the whole printed answer\n").unwrap();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], ChatEvent::Delta { text } if text == "the whole printed answer")
        );
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
                "gemini-2.5-pro",
                &Caller::test_default(),
            )
            .unwrap();
        assert!(inv.prompt.starts_with("[system]\nbe precise"));
        assert!(inv.prompt.contains("[user]\nx"));
    }
}
