//! `bash.run` — run a shell command in the agent's working directory.
//!
//! The action tool a coding Agent needs to build / test / grep / git.
//! Runs `sh -c <command>` in [`ToolContext::cwd`] (the agent's worktree),
//! captures stdout+stderr, and bounds the blast with a timeout + an
//! output cap. Cooperative-cancel aware.
//!
//! Side effect: **Irreversible** by default (Doc 04 §4.4) — an arbitrary
//! command can do anything; whether it may run at all is the Agent's
//! permission layer's call (Doc 05 IAM), not this tool's. This tool just
//! executes what it's handed and reports honestly.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap captured output so a runaway `cat huge` doesn't blow the context.
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

pub struct BashTool {
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct BashArgs {
    /// The shell command line, run via `sh -c`.
    command: String,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the per-call timeout. Chainable.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Keep the TAIL — errors / test failures land at the end of output.
    let start = s.len() - max;
    let mut i = start;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    format!("…[{} bytes truncated]…\n{}", start, &s[i..])
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash.run"
    }

    fn description(&self) -> &str {
        "Run a shell command (via `sh -c`) in the working directory and \
         return its combined stdout/stderr + exit code. Use for build, \
         test, git, and other actions on the tree. To SEARCH code, prefer \
         fs.grep (contents) and fs.glob (file names) — they are faster and \
         scoped to the workspace; avoid `grep`/`find` here."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "BashArgs",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": { "type": "string", "description": "Shell command line, run via `sh -c`." }
                    },
                    "required": ["command"]
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: BashArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        // Confine the shell's process tree via `ctx.sandbox` (Doc 22: "naked
        // spawn → sandboxed"). The default policy is `DangerFullAccess`, which
        // `wrap` returns as a passthrough — so behaviour is unchanged until a
        // caller threads a confining `SandboxMode` (M4). Fail-closed: a
        // requested-but-unbuildable sandbox errors here, never spawns bare.
        let workdir = ctx.cwd.clone().unwrap_or_else(|| std::path::PathBuf::from("."));
        let (program, argv) = ctx
            .sandbox
            .wrap("sh", &["-c".to_string(), parsed.command.clone()], &workdir)
            .map_err(|e| ToolError::Execute(format!("exec sandbox: {e}")))?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(argv);
        if let Some(cwd) = ctx.cwd.as_deref() {
            cmd.current_dir(cwd);
        }
        cmd.kill_on_drop(true);

        let run = async {
            cmd.output()
                .await
                .map_err(|e| ToolError::Execute(format!("spawn `sh -c`: {e}")))
        };

        let output = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            res = tokio::time::timeout(self.timeout, run) => match res {
                Ok(o) => o?,
                Err(_) => {
                    return Ok(ToolResult::error(format!(
                        "command timed out after {}s: {}",
                        self.timeout.as_secs(),
                        truncate_tail(&parsed.command, 120)
                    )));
                }
            },
        };

        let code = output.status.code();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!(
            "exit: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            stdout.trim_end(),
            stderr.trim_end(),
        );
        let body = truncate_tail(&combined, MAX_OUTPUT_BYTES);
        // The exit code is in `body` ("exit: N") for the model to react to. We
        // deliberately DON'T flag a non-zero exit as `is_error`: that bool means
        // "the tool failed to run" (missing file, spawn failure, timeout), which
        // the drive-loop's consecutive-all-error abort treats as broken tooling.
        // A command's OWN non-zero exit is a normal signal — `grep` with no match
        // exits 1, a failing test/build exits non-zero — and an agent exploring
        // with a few empty greps would otherwise be aborted as "stuck". Surface
        // the exit code as DATA in the title, not as a tool error.
        let title = match code {
            Some(0) => "ran command".to_string(),
            Some(c) => format!("ran command (exit {c})"),
            None => "ran command (killed by signal)".to_string(),
        };
        Ok(ToolResult::titled_success(title, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn ctx(cwd: Option<PathBuf>) -> ToolContext {
        ToolContext {
            cancel: CancellationToken::new(),
            cwd,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn runs_in_cwd_and_reports_exit_zero() {
        let dir = std::env::temp_dir().join(format!("tars_bash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let r = BashTool::new()
            .execute(
                json!({ "command": "ls marker.txt" }),
                ctx(Some(dir.clone())),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("marker.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // M2/M6: BashTool actually confined by a WorkspaceWrite sandbox (macOS).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_confines_bash_writes_to_worktree() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let wt = base.join(format!("tars_bash_jail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt);
        std::fs::create_dir_all(&wt).unwrap();
        let wt = std::fs::canonicalize(&wt).unwrap();
        let outside = base.join(format!("tars_bash_escape_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&outside);

        let jailed = || {
            let mut c = ctx(Some(wt.clone()));
            c.sandbox = crate::SandboxPolicy::workspace_write(&wt);
            c
        };

        // write inside worktree → allowed
        BashTool::new()
            .execute(json!({ "command": "echo ok > inside.txt" }), jailed())
            .await
            .unwrap();
        assert!(wt.join("inside.txt").exists(), "write inside worktree must succeed");

        // write OUTSIDE worktree → blocked by the sandbox
        let _ = BashTool::new()
            .execute(json!({ "command": format!("echo pwned > {}", outside.display()) }), jailed())
            .await;
        assert!(!outside.exists(), "sandbox MUST block bash writes outside the worktree");

        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_in_body_not_flagged_as_tool_error() {
        // A command's own non-zero exit (grep no-match, failing test/build) is a
        // normal signal the model reads from the body — NOT a tool failure. If it
        // were flagged `is_error`, a few empty exploratory greps would trip the
        // drive-loop's consecutive-all-error abort and kill a working agent.
        let r = BashTool::new()
            .execute(json!({ "command": "exit 3" }), ctx(None))
            .await
            .unwrap();
        assert!(!r.is_error, "non-zero exit is data, not a tool error");
        assert!(r.content.contains("exit: 3"), "exit code stays in the body");
    }
}
