//! Production [`SubprocessRunner`]: spawns `claude` for real and
//! parses its `--output-format json` payload. The stream-json path
//! lives in [`super::streaming`]; the buffered path is right here.
//! Holds the JSON-shape helpers (`extract_result_text`,
//! `extract_usage`, `truncate`) shared with the streaming code.

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use tars_types::{ProviderError, Usage};

use crate::child_reaper::ReaperGuard;

use super::argv::{SubprocessInvocation, SubprocessRunner, build_argv_with, streaming_enabled};
use super::streaming::run_streaming;

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

        // Put the child in its OWN process group (becomes group leader) so
        // the signal-time reaper can SIGKILL the whole subtree as a unit
        // (claude may itself fork helpers). kill_on_drop still covers the
        // graceful path; the process group covers the signal path.
        #[cfg(unix)]
        cmd.process_group(0);

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

        // Register the PID so a SIGINT/SIGTERM reaper in the host can
        // SIGKILL this child's process group. The guard deregisters on
        // EVERY exit path of this function (early `?`, timeout, success),
        // mirroring kill_on_drop's graceful coverage.
        let _reaper_guard = child.id().map(ReaperGuard::new);

        // Write the prompt on stdin and close it. stdin must be present
        // (Stdio::piped above); if it isn't, fail loudly rather than
        // silently skip the write and let the child block on an EOF that
        // never comes until the timeout fires.
        let mut stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Internal("claude child has no stdin pipe (Stdio::piped above)".into())
        })?;
        {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(inv.prompt.as_bytes()).await.map_err(|e| {
                ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!(
                        "stdin write failed after {} prompt bytes: {e}",
                        inv.prompt.len()
                    ),
                }
            })?;
        }
        // dropping `stdin` here closes the pipe so the child sees EOF
        drop(stdin);

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
                // Timed out. `wait_with_output()` owns `child`, so the
                // child is killed deterministically the moment the timed-
                // out future is dropped (we set `kill_on_drop(true)` at
                // spawn) — i.e. as this match arm returns. No leaked
                // process survives the timeout.
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

/// CLI puts the response in `.result`. Python uses `payload.get("result") or ""`
/// to coerce JSON-null to empty string — same behavior here.
pub(super) fn extract_result_text(payload: &Value) -> String {
    payload
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub(super) fn extract_usage(payload: &Value) -> Usage {
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

pub(super) fn truncate(s: &str, max: usize) -> String {
    let trimmed = crate::http_base::truncate_utf8(s, max);
    if trimmed.len() == s.len() {
        s.to_string()
    } else {
        format!("{trimmed}…")
    }
}
