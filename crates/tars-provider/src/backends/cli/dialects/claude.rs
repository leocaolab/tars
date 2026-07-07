//! [`ClaudeCliDialect`] — the claude behavior expressed as a
//! [`CliDialect`] (Doc 32 §5 C3). Reproduces today's `claude_cli`
//! behavior byte-for-byte: the `claude -p --model X --output-format
//! stream-json|json --permission-mode bypassPermissions …` argv, prompt
//! on stdin, JSON output, and the `result`/`usage` → `ChatEvent` mapping.

use std::time::Duration;

use serde_json::Value;

use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext, StopReason};

use super::super::argv::{
    ClaudeCliEffort, ClaudeCliTools, STRIPPED_ENV_KEYS_UPPER, SubprocessInvocation, build_argv_with,
    serialize_messages_for_cli, streaming_enabled,
};
use super::super::dialect::{CliDialect, CliInvocation, OutputMode, PromptChannel};
use super::super::subprocess::{extract_result_text, extract_usage};

/// Claude Code CLI dialect. Holds the per-invocation claude configuration
/// (executable, timeout, tool policy, effort, …) that `claude_cli`'s
/// builder used to keep on the provider; the shared `AgentCliBackend`
/// stays free of any claude specifics.
#[derive(Clone, Debug)]
pub struct ClaudeCliDialect {
    executable: String,
    timeout: Duration,
    tools: ClaudeCliTools,
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
        tools: ClaudeCliTools,
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
    fn argv(&self, inv: &CliInvocation) -> Vec<String> {
        // Identical to the pre-refactor production argv: `build_argv_with`
        // reads the streaming toggle once (as the runner also does).
        build_argv_with(inv, streaming_enabled())
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        ctx: &RequestContext,
    ) -> Result<CliInvocation, ProviderError> {
        let model = req
            .model
            .explicit()
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "model must be explicit before reaching CLI provider".into(),
                )
            })?
            .to_string();

        let prompt = serialize_messages_for_cli(req);
        let system = req.system.clone();

        Ok(SubprocessInvocation {
            executable: self.executable.clone(),
            model,
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
            // When `--tools default` lets claude run its own Read/Edit/Bash,
            // those must operate in the request's working dir (the fix
            // worktree), not arc's process cwd. `None` → inherit parent cwd.
            cwd: ctx.cwd.clone(),
            // OS-confinement policy (G10) — jails claude's spawn under the
            // resolved `[sandbox]`/`--sandbox` policy (env gate stays a fallback).
            sandbox: ctx.sandbox.clone(),
        })
    }

    fn prompt_channel(&self) -> PromptChannel {
        PromptChannel::Stdin
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::JsonEvents
    }

    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        // claude_cli's own home (`$CLAUDE_CONFIG_DIR`, default `~/.claude`)
        // holds its config, sessions, and the per-session shell-snapshot /
        // session-env dir its Bash tool creates on first use. Under the
        // delegate write-jail that `mkdir` was the live `EPERM on session-env
        // directory creation` — the fixer could edit the worktree but not run
        // `cargo build`/`cargo test` to self-verify its own fix. Grant
        // `~/.claude` too (skipped centrally if absent), mirroring codex's
        // `~/.codex`. The env/home reads live here; the resolution rule is
        // factored into the pure `resolve_claude_state_dirs` below.
        resolve_claude_state_dirs(
            std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from),
            tars_types::env::home_dir(),
        )
    }

    fn parse_line(&self, raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        // The runner reconstructs claude's `--output-format json` payload
        // (or the stream-json `result` event, `type` stripped) into a single
        // object. `result` is the answer text; `usage` the token counts.
        // The backend prepends `Started` and applies the output-budget clamp,
        // so the natural stop reason here is always `EndTurn`.
        let text = extract_result_text(raw);
        let usage = extract_usage(raw);
        Ok(vec![
            ChatEvent::Delta { text },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage,
            },
        ])
    }
}

/// Resolve claude's writable state dir from `(config-dir override, home dir)`.
/// `$CLAUDE_CONFIG_DIR` wins when set; otherwise `~/.claude`; otherwise nothing
/// (no `$HOME` → no dir to grant). Non-existent paths are filtered centrally in
/// `subprocess::build_sandboxed_command`, so this stays a pure mapping. Pulled
/// out of [`ClaudeCliDialect::state_dirs`] so it's unit-testable without
/// process-global env mutation (the workspace forbids `unsafe`; Rust 2024 makes
/// `env::set_var` `unsafe`) — same idiom as `build_argv_with`.
fn resolve_claude_state_dirs(
    config_dir_override: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    config_dir_override
        .or_else(|| home.map(|h| h.join(".claude")))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use tars_types::{ModelHint, ModelTier};

    fn dialect() -> ClaudeCliDialect {
        ClaudeCliDialect::new(
            "claude".into(),
            Duration::from_secs(300),
            ClaudeCliTools::Disabled,
            false,
            None,
            true,
            Vec::new(),
        )
    }

    #[test]
    fn argv_matches_build_argv_with_exactly() {
        // The dialect's argv is the SAME bytes the pre-refactor production
        // path produced (E2E-1 argv identity): `dialect.argv` must equal
        // `build_argv_with(inv, streaming_enabled())`.
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                &RequestContext::test_default(),
            )
            .unwrap();
        assert_eq!(d.argv(&inv), build_argv_with(&inv, streaming_enabled()));
    }

    #[test]
    fn invocation_carries_claude_config_and_context() {
        let d = ClaudeCliDialect::new(
            "claude".into(),
            Duration::from_secs(42),
            ClaudeCliTools::Default,
            true,
            Some(ClaudeCliEffort::High),
            false,
            vec!["--debug".into()],
        );
        let wt = std::path::PathBuf::from("/tmp/wt");
        let inv = d
            .invocation(
                &ChatRequest::user(ModelHint::Explicit("sonnet".into()), "x").with_system("brief"),
                &RequestContext::test_default().with_cwd(wt.clone()),
            )
            .unwrap();
        assert_eq!(inv.model, "sonnet");
        assert_eq!(inv.system.as_deref(), Some("brief"));
        assert_eq!(inv.timeout, Duration::from_secs(42));
        assert!(inv.bare);
        assert_eq!(inv.effort, Some(ClaudeCliEffort::High));
        assert!(!inv.exclude_dynamic_sections);
        assert_eq!(inv.extra_args, vec!["--debug".to_string()]);
        assert_eq!(inv.cwd, Some(wt));
        assert!(inv.stripped_env.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn invocation_requires_explicit_model() {
        let d = dialect();
        let err = d
            .invocation(
                &ChatRequest::user(ModelHint::Tier(ModelTier::Default), "hi"),
                &RequestContext::test_default(),
            )
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
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
                // Anthropic-shape `input_tokens` (12) is fresh-only and
                // disjoint from cache read (3); tars folds them into a
                // TOTAL-prompt `input_tokens` (12 + 3 = 15) with cache as a
                // subset, per the `Usage` convention.
                assert_eq!(usage.input_tokens, 15);
                assert_eq!(usage.output_tokens, 5);
                assert_eq!(usage.cached_input_tokens, 3);
                assert_eq!(usage.cache_creation_tokens, 0);
                assert_eq!(usage.thinking_tokens, 0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_null_result_is_empty_delta() {
        let d = dialect();
        let events = d.parse_line(&json!({"result": null, "is_error": false})).unwrap();
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text.is_empty()));
    }

    #[test]
    fn state_dirs_prefers_config_dir_override_verbatim() {
        // `$CLAUDE_CONFIG_DIR` set → grant exactly that path, NOT joined with
        // `.claude` (the override already names the config home). The home dir
        // is ignored when the override is present.
        let dirs = resolve_claude_state_dirs(
            Some(PathBuf::from("/custom/claude/home")),
            Some(PathBuf::from("/home/alice")),
        );
        assert_eq!(dirs, vec![PathBuf::from("/custom/claude/home")]);
    }

    #[test]
    fn state_dirs_resolves_dot_claude_under_home_when_no_override() {
        // No `$CLAUDE_CONFIG_DIR` → fall back to `$HOME/.claude`.
        let dirs = resolve_claude_state_dirs(None, Some(PathBuf::from("/home/alice")));
        assert_eq!(dirs, vec![PathBuf::from("/home/alice/.claude")]);
    }

    #[test]
    fn state_dirs_empty_when_no_override_and_no_home() {
        // Neither knob available (e.g. `$HOME` unset in a container) → grant
        // nothing rather than a bogus relative path; the jail simply omits it.
        assert!(resolve_claude_state_dirs(None, None).is_empty());
    }

    #[test]
    fn channel_is_stdin_and_mode_is_json_events() {
        let d = dialect();
        assert_eq!(d.prompt_channel(), PromptChannel::Stdin);
        assert_eq!(d.output_mode(), OutputMode::JsonEvents);
        assert!(d.env().is_empty());
    }
}
