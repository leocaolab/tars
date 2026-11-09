//! Builds claude's argv: the tools it may use, its effort, and the flags every run passes.

use super::invocation::SubprocessInvocation;

/// Names the variables never passed to `claude`, upper-cased: with any of them it bills an API
/// account instead of the subscription. They are matched whatever their case, since Windows keeps
/// a variable's case and `Anthropic_Api_Key` would pass an exact match.
pub(crate) const STRIPPED_ENV_KEYS_UPPER: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Names the variables `claude` needs beyond the system ones: its home override and the
/// long-lived subscription token `claude setup-token` makes for machines without a keychain.
pub(crate) const NEEDED_ENV_KEYS: &[&str] = &["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"];

/// Returns whether `name` is one of [`STRIPPED_ENV_KEYS_UPPER`], whatever its case.
pub(crate) fn is_stripped_env(name: &str) -> bool {
    STRIPPED_ENV_KEYS_UPPER
        .iter()
        .any(|key| key.eq_ignore_ascii_case(name))
}

/// The tools `claude` may use. The default is none: without tools it cannot start its own agent
/// loop, so a call is plain inference; the token cost of that loop is measured in
/// `docs/architecture/01-llm-provider.md`, "CLI Subprocess Tuning".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ClaudeToolAccess {
    /// Passes `--tools ""`.
    #[default]
    Disabled,
    /// Omits `--tools`, so the CLI has all of its tools.
    Default,
    /// Passes the names joined by commas; a name containing a comma is dropped. A list left
    /// empty passes `--tools ""`, as `Disabled` does.
    Allow(Vec<String>),
}

/// The tools a run of `claude` is given, after the names `--tools` cannot carry are dropped.
#[derive(Debug, PartialEq, Eq)]
enum ToolGrant {
    None,
    All,
    /// The comma-joined names; never empty.
    Named(String),
}

impl ToolGrant {
    /// Returns whether claude can run a tool, and so needs the permission bypass. A run with
    /// none gets no bypass: claude may still load MCP servers from the user's config, and those
    /// must not run unprompted.
    fn runs_tools(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Returns the tools `access` gives a run. An allow-list left with no name, because it was empty
/// or every name held a comma, gives none, the same as `Disabled`.
fn tool_grant(access: &ClaudeToolAccess) -> ToolGrant {
    match access {
        ClaudeToolAccess::Disabled => ToolGrant::None,
        ClaudeToolAccess::Default => ToolGrant::All,
        ClaudeToolAccess::Allow(names) => {
            // A comma in a name would split it into two tools; no real tool name has one.
            let kept: Vec<&str> = names
                .iter()
                .filter(|name| {
                    let carried = !name.contains(',');
                    if !carried {
                        tracing::warn!(
                            tool = %name,
                            "claude_cli: dropping tool name containing ',' — it would corrupt the --tools CSV",
                        );
                    }
                    carried
                })
                .map(String::as_str)
                .collect();
            if kept.is_empty() {
                ToolGrant::None
            } else {
                ToolGrant::Named(kept.join(","))
            }
        }
    }
}

/// The value of claude's `--effort`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeCliEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ClaudeCliEffort {
    pub(crate) fn as_arg(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Returns whether `TARS_CLAUDE_CLI_STREAM` turns on claude's `stream-json` output, whose events
/// are logged as they arrive. Unset, or not UTF-8 (which is logged), means off.
pub(crate) fn streaming_enabled() -> bool {
    match tars_types::env::claude_cli_streaming() {
        Ok(opt) => opt.unwrap_or(false),
        Err(e) => {
            tracing::warn!(error = %e, "TARS_CLAUDE_CLI_STREAM set but unreadable (non-UTF-8); ignoring");
            false
        }
    }
}

/// Returns `claude`'s argv, reading `TARS_CLAUDE_CLI_STREAM`. Only tests call it.
#[allow(dead_code)]
pub(crate) fn build_argv(inv: &SubprocessInvocation) -> Vec<String> {
    build_argv_with(inv, streaming_enabled())
}

/// The `--settings` of every `claude` run, under which none of the machine user's hooks run.
const NO_HOOKS: &str = r#"{"disableAllHooks":true}"#;

/// Returns `claude`'s argv, without the executable, in `json` or, when `streaming`, `stream-json`
/// output. Tests pass `streaming` because setting the variable needs `unsafe`, which the workspace
/// forbids.
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
        // A user's Stop hook that waits for mail (up to 2 h) once kept a `claude -p` alive long
        // after it answered, until the run was reaped as failed and the answer lost.
        "--settings".into(),
        NO_HOOKS.into(),
    ];
    if streaming {
        // Without `--verbose`, stream-json prints only the final payload; partial messages add
        // the text and thinking deltas.
        argv.push("--include-partial-messages".into());
        argv.push("--verbose".into());
    }

    let grant = tool_grant(&inv.tools);
    match &grant {
        ToolGrant::None => {
            argv.push("--tools".into());
            argv.push(String::new());
        }
        ToolGrant::All => {}
        ToolGrant::Named(csv) => {
            argv.push("--tools".into());
            argv.push(csv.clone());
        }
    }

    // In `-p` mode nobody answers the permission prompt, so without the bypass every edit and
    // bash call is refused and the agent reports no changes. `extra_args` can set a stricter
    // mode.
    if grant.runs_tools() {
        argv.push("--permission-mode".into());
        argv.push("bypassPermissions".into());
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
        argv.push("--system-prompt-file".into());
        argv.push(sys.path().display().to_string());
    }

    argv.extend(inv.extra_args.iter().cloned());

    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allow_list_with_no_usable_name_grants_no_tools() {
        assert_eq!(
            tool_grant(&ClaudeToolAccess::Allow(vec![])),
            ToolGrant::None
        );
        assert_eq!(
            tool_grant(&ClaudeToolAccess::Allow(vec!["Read,Bash".into()])),
            ToolGrant::None
        );
    }

    #[test]
    fn an_allow_list_keeps_the_names_without_a_comma() {
        assert_eq!(
            tool_grant(&ClaudeToolAccess::Allow(vec!["Read".into(), "a,b".into()])),
            ToolGrant::Named("Read".into())
        );
    }

    #[test]
    fn only_a_grant_with_tools_asks_for_the_permission_bypass() {
        assert!(!ToolGrant::None.runs_tools());
        assert!(ToolGrant::All.runs_tools());
        assert!(ToolGrant::Named("Read".into()).runs_tools());
    }
}
