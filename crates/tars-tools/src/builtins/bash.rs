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
         test, grep, git, and other actions on the tree."
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

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&parsed.command);
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
        // A non-zero exit is a logical failure the model should react to,
        // not a tool error — surface it as an `is_error` result.
        if code == Some(0) {
            Ok(ToolResult::titled_success("ran command", body))
        } else {
            Ok(ToolResult::titled_error(
                format!(
                    "command exited {}",
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "by signal".into())
                ),
                body,
            ))
        }
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

    #[tokio::test]
    async fn nonzero_exit_is_logical_error() {
        let r = BashTool::new()
            .execute(json!({ "command": "exit 3" }), ctx(None))
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("exit: 3"));
    }
}
