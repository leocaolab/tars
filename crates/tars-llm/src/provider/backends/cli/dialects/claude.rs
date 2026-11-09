//! Talks to Claude Code's `claude -p`: the prompt on stdin, the system prompt in a file, and the
//! JSON result read for its text and usage. Spawning is [`RealSubprocessRunner`]'s.
//!
//! [`RealSubprocessRunner`]: super::super::subprocess::RealSubprocessRunner

use std::time::Duration;

use serde_json::Value;

use crate::ProviderError;
use crate::provider::backends::anthropic::map_stop_reason;
use crate::provider::error::IoOp;
use tars_types::{Caller, ChatEvent, ChatRequest, StopReason};

use super::super::claude_argv::{
    ClaudeCliEffort, ClaudeToolAccess, NEEDED_ENV_KEYS, STRIPPED_ENV_KEYS_UPPER, build_argv_with,
    streaming_enabled,
};
use super::super::dialect::{CliDialect, OutputMode, PromptChannel};
use super::super::invocation::SubprocessInvocation;
use super::super::prompt::serialize_messages_for_cli;
use super::super::subprocess::{extract_result_text, extract_usage};
use super::super::system_prompt_file::SystemPromptFile;
use std::sync::Arc;

/// The claude settings every run uses: executable, timeout, tools, effort and extra flags.
#[derive(Clone, Debug)]
pub struct ClaudeCliDialect {
    executable: String,
    timeout: Duration,
    tools: ClaudeToolAccess,
    bare: bool,
    effort: Option<ClaudeCliEffort>,
    exclude_dynamic_sections: bool,
    extra_args: Vec<String>,
}

impl ClaudeCliDialect {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        executable: String,
        timeout: Duration,
        tools: ClaudeToolAccess,
        bare: bool,
        effort: Option<ClaudeCliEffort>,
        exclude_dynamic_sections: bool,
        extra_args: Vec<String>,
    ) -> Self {
        Self {
            executable,
            timeout,
            tools,
            bare,
            effort,
            exclude_dynamic_sections,
            extra_args,
        }
    }
}

impl CliDialect for ClaudeCliDialect {
    fn argv(&self, inv: &SubprocessInvocation) -> Vec<String> {
        build_argv_with(inv, streaming_enabled())
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &Caller,
    ) -> Result<SubprocessInvocation, ProviderError> {
        let model = model.to_string();

        let prompt = serialize_messages_for_cli(req);
        // A file, not an argument: see `SystemPromptFile`.
        let system = req
            .system
            .clone()
            .map(SystemPromptFile::write)
            .transpose()
            .map_err(|source| ProviderError::Io {
                op: IoOp::WriteSystemPromptFile,
                source,
            })?
            .map(Arc::new);

        Ok(SubprocessInvocation {
            executable: self.executable.clone(),
            model,
            system,
            prompt,
            timeout: ctx.call_budget(self.timeout),
            stripped_env: STRIPPED_ENV_KEYS_UPPER
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tools: self.tools.clone(),
            bare: self.bare,
            effort: self.effort,
            exclude_dynamic_sections: self.exclude_dynamic_sections,
            extra_args: self.extra_args.clone(),
            // claude's own tools work in the request's directory, not the process cwd.
            cwd: ctx.cwd.clone(),
            sandbox: ctx.sandbox.clone(),
        })
    }

    fn prompt_channel(&self) -> PromptChannel {
        PromptChannel::Stdin
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::JsonEvents
    }

    fn env(&self) -> &[&str] {
        NEEDED_ENV_KEYS
    }

    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        // claude's home holds its config, its sessions and the directory its Bash tool creates
        // on first use.
        claude_state_dirs()
    }

    fn parse_line(&self, raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        let text = extract_result_text(raw);
        let usage = extract_usage(raw);
        // claude passes on the API's `stop_reason`. A release that leaves it out reports a run
        // that completed, which ended its turn.
        let stop_reason = raw
            .get("stop_reason")
            .and_then(Value::as_str)
            .map_or(StopReason::EndTurn, map_stop_reason);
        Ok(vec![
            ChatEvent::Delta { text },
            ChatEvent::Finished {
                stop_reason,
                usage: Some(usage),
            },
        ])
    }
}

/// Returns claude's home: the config-dir override as given, else `.claude` under the home
/// directory, else nothing. With the override set, claude keeps its settings, sessions and plugins
/// there instead of in `~/.claude`, so `~/.claude` is not granted. Taking both as arguments lets
/// tests avoid setting variables, which needs `unsafe`.
fn resolve_claude_state_dirs(
    config_dir_override: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    config_dir_override
        .or_else(|| home.map(|h| h.join(".claude")))
        .into_iter()
        .collect()
}

/// Returns claude's home (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) for the jail to make writable.
/// `RealSubprocessRunner` calls it too, since claude does not run through `SharedCliRunner`.
pub(crate) fn claude_state_dirs() -> Vec<std::path::PathBuf> {
    resolve_claude_state_dirs(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from),
        tars_types::env::home_dir(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn dialect() -> ClaudeCliDialect {
        ClaudeCliDialect::new(
            "claude".into(),
            Duration::from_secs(300),
            ClaudeToolAccess::Disabled,
            false,
            None,
            true,
            Vec::new(),
        )
    }

    #[test]
    fn argv_matches_build_argv_with_exactly() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("hi"),
                "test-model",
                &Caller::test_default(),
            )
            .unwrap();
        assert_eq!(d.argv(&inv), build_argv_with(&inv, streaming_enabled()));
    }

    // The timeout and the stripped environment are tested for every dialect in
    // `tests/cli_conformance.rs`.

    #[test]
    fn invocation_carries_claude_config_and_context() {
        let d = ClaudeCliDialect::new(
            "claude".into(),
            Duration::from_secs(42),
            ClaudeToolAccess::Default,
            true,
            Some(ClaudeCliEffort::High),
            false,
            vec!["--debug".into()],
        );
        let wt = std::path::PathBuf::from("/tmp/wt");
        let inv = d
            .invocation(
                &ChatRequest::user("x").with_system("brief"),
                "sonnet",
                &Caller::test_default().with_cwd(wt.clone()),
            )
            .unwrap();
        assert_eq!(inv.model, "sonnet");
        let system = inv.system.as_ref().unwrap();
        assert_eq!(std::fs::read_to_string(system.path()).unwrap(), "brief");
        assert_eq!(inv.timeout, Duration::from_secs(42));
        assert!(inv.bare);
        assert_eq!(inv.effort, Some(ClaudeCliEffort::High));
        assert!(!inv.exclude_dynamic_sections);
        assert_eq!(inv.extra_args, vec!["--debug".to_string()]);
        assert_eq!(inv.cwd, Some(wt));
    }

    #[test]
    fn parse_line_maps_result_and_usage() {
        let d = dialect();
        let payload = json!({
            "result": "hello from claude",
            "is_error": false,
            "usage": { "input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 3 }
        });
        let events = d.parse_line(&payload).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            ChatEvent::Delta { text } if text == "hello from claude"
        ));
        match &events[1] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                // Anthropic's 12 excludes the 3 cache reads; tars counts 15.
                assert_eq!(usage.unwrap().input_tokens, 15);
                assert_eq!(usage.unwrap().output_tokens, 5);
                assert_eq!(usage.unwrap().cached_input_tokens, 3);
                assert_eq!(usage.unwrap().cache_creation_tokens, 0);
                assert_eq!(usage.unwrap().thinking_tokens, 0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_reads_the_stop_reason_claude_reports() {
        let events = dialect()
            .parse_line(&json!({"result": "cut", "is_error": false, "stop_reason": "max_tokens"}))
            .unwrap();
        assert!(matches!(
            &events[1],
            ChatEvent::Finished {
                stop_reason: StopReason::MaxTokens,
                ..
            }
        ));
    }

    #[test]
    fn parse_line_null_result_is_empty_delta() {
        let d = dialect();
        let events = d
            .parse_line(&json!({"result": null, "is_error": false}))
            .unwrap();
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text.is_empty()));
    }

    #[test]
    fn state_dirs_prefers_config_dir_override_verbatim() {
        let dirs = resolve_claude_state_dirs(
            Some(PathBuf::from("/custom/claude/home")),
            Some(PathBuf::from("/home/alice")),
        );
        assert_eq!(dirs, vec![PathBuf::from("/custom/claude/home")]);
    }

    #[test]
    fn state_dirs_resolves_dot_claude_under_home_when_no_override() {
        let dirs = resolve_claude_state_dirs(None, Some(PathBuf::from("/home/alice")));
        assert_eq!(dirs, vec![PathBuf::from("/home/alice/.claude")]);
    }

    #[test]
    fn state_dirs_empty_when_no_override_and_no_home() {
        assert!(resolve_claude_state_dirs(None, None).is_empty());
    }
}
