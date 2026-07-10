//! Stream-json mode for the delegate CLI subprocess, gated by
//! `TARS_CLAUDE_CLI_STREAM`. Owns the NDJSON event reader and the
//! human-readable per-event summary emitted to stderr for
//! observability. Lives in its own file because it carries the most
//! complexity (event taxonomy + line-by-line reader + stderr drain)
//! and isn't on the happy path of the buffered runner.
//!
//! Lifted verbatim from `claude_cli/streaming.rs` (Doc 32 §7) as part of
//! the shared CLI-delegate machinery.

use serde_json::Value;

use tars_types::ProviderError;

use super::argv::SubprocessInvocation;
use super::subprocess::truncate;

/// Drive `claude -p --output-format stream-json` and stream events to
/// stderr while reconstructing the `result` event for the return value.
///
/// `claude` emits NDJSON: one JSON object per line, one of:
///   - `system/init`, `system/status`               — lifecycle
///   - `rate_limit_event`                            — quota
///   - `stream_event/message_start`                  — API responded
///   - `stream_event/content_block_start|stop`       — thinking/text/tool boundary
///   - `stream_event/content_block_delta`            — partial chunks
///   - `stream_event/message_delta|message_stop`     — usage / done
///   - `assistant`                                   — assembled message
///   - `result`                                      — final aggregate (THE return value)
///
/// On EOF without a `result` event we fail loud — that's broken-invariant
/// territory, never a silent empty Value.
pub(crate) async fn run_streaming(
    child: &mut tokio::process::Child,
    inv: &SubprocessInvocation,
) -> Result<Value, ProviderError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::CliSubprocessDied {
            exit_code: None,
            stderr: "stream-json: stdout pipe missing on spawned child".into(),
        })?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ProviderError::CliSubprocessDied {
            exit_code: None,
            stderr: "stream-json: stderr pipe missing on spawned child".into(),
        })?;

    // Drain stderr in a separate task so the child can't block on a full
    // pipe (claude prints rate limit / debug to stderr).
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Err(e) = BufReader::new(stderr_pipe).read_to_end(&mut buf).await {
            // A failed/partial stderr read means the diagnostic we'd
            // attach to a non-zero exit / timeout may be missing; log it
            // rather than silently dropping the cause.
            tracing::warn!(error = %e, "claude_cli stream: failed to read child stderr");
        }
        buf
    });

    // Reader for stdout NDJSON events.
    let mut reader = BufReader::new(stdout).lines();
    let mut final_result: Option<Value> = None;
    let mut session_short: String = "????????".into();

    let read_fut = async {
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    let parsed: Result<Value, _> = serde_json::from_str(&line);
                    match parsed {
                        Ok(ev) => {
                            // Capture short session id from the init event
                            // so subsequent log lines are correlatable.
                            if session_short == "????????" {
                                if let Some(sid) = ev.get("session_id").and_then(|v| v.as_str()) {
                                    session_short = sid.chars().take(8).collect();
                                }
                            }
                            emit_event_summary(&ev, &session_short);
                            if ev.get("type").and_then(|v| v.as_str()) == Some("result") {
                                final_result = Some(ev);
                            }
                        }
                        Err(e) => {
                            // Non-JSON line on stdout — claude shouldn't
                            // emit these in stream-json mode, but if it
                            // does, surface them (with the parse error)
                            // rather than swallowing.
                            eprintln!(
                                "[claude_cli {session_short}] !! non-json stdout line ({e}): {}",
                                truncate(&line, 200)
                            );
                        }
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    return Err(ProviderError::CliSubprocessDied {
                        exit_code: None,
                        stderr: format!("stream read failed: {e}"),
                    });
                }
            }
        }
        Ok::<(), ProviderError>(())
    };

    // The timeout must bound BOTH the stdout drain and the child's exit. A
    // wedged child holds stdout open without writing, so `read_fut` never
    // resolves; timing out only `child.wait()` and then `join!`-ing the pair
    // would wait for the reader forever and never reach the timeout arm.
    let collect = async {
        let (read_res, wait_res) = tokio::join!(read_fut, child.wait());
        (read_res, wait_res)
    };

    let Ok((read_res, wait_res)) = tokio::time::timeout(inv.timeout, collect).await else {
        // Kill FIRST: the stderr drain only reaches EOF once the child is gone,
        // so awaiting `stderr_task` on a wedged child would hang the very path
        // meant to bound it. (`kill_on_drop` covers the spawn helper's `Child`;
        // here we hold a `&mut` and return without dropping it.)
        if let Err(e) = child.start_kill() {
            tracing::warn!(error = %e, "claude_cli stream: failed to kill child after timeout");
        }
        let stderr_buf = stderr_task.await.unwrap_or_default();
        let stderr_s = truncate(&String::from_utf8_lossy(&stderr_buf), 500);
        // We killed the child; it didn't die on its own. Report the wall-clock
        // abort as `TimedOut`, not `CliSubprocessDied` (whose name would blame
        // the child for our kill). `budget` is the invocation budget; `detail`
        // carries the same diagnostics the old stderr string did.
        return Err(ProviderError::TimedOut {
            budget: inv.timeout,
            detail: format!(
                "stream-json child killed after wall-clock timeout (model={}, prompt_chars={}, stderr: {stderr_s})",
                inv.model,
                inv.prompt.len()
            ),
        });
    };

    let stderr_buf = stderr_task.await.unwrap_or_default();

    read_res?;

    let status = match wait_res {
        Ok(s) => s,
        Err(e) => {
            let stderr_s = truncate(&String::from_utf8_lossy(&stderr_buf), 500);
            return Err(ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("wait failed: {e} (stderr: {stderr_s})"),
            });
        }
    };

    if !status.success() {
        let stderr_s = String::from_utf8_lossy(&stderr_buf).to_string();
        // UTF-8-safe truncation — see the buffered path; `[..500]` can
        // panic on a multi-byte boundary.
        let truncated = truncate(&stderr_s, 500);
        return Err(ProviderError::CliSubprocessDied {
            exit_code: status.code(),
            stderr: truncated,
        });
    }

    // Strip the `type` wrapper from the result event so callers see the
    // same shape as buffered `--output-format json` mode (which returns
    // the result object directly, not wrapped in {type: "result", ...}).
    let mut result = final_result.ok_or_else(|| {
        ProviderError::Parse(
            "stream-json mode: child exited without emitting a `result` event".into(),
        )
    })?;
    if let Some(obj) = result.as_object_mut() {
        obj.remove("type");
    }

    if !result.is_object() {
        return Err(ProviderError::Parse(format!(
            "stream-json result is not an object: {result:?}"
        )));
    }

    if result
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let detail = result
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("(no detail in result event)");
        return Err(ProviderError::CliSubprocessDied {
            exit_code: status.code(),
            stderr: format!("claude reported is_error: {detail}"),
        });
    }

    Ok(result)
}

/// One-line stderr summary for a stream-json event. Mirrors the
/// information a user would see watching `claude` interactively (init,
/// thinking, text generation, result). Designed to be cheap (no
/// allocation when stdlib formatting suffices) and human-readable.
fn emit_event_summary(ev: &Value, sid: &str) {
    let evtype = ev.get("type").and_then(|v| v.as_str()).unwrap_or("?");

    match evtype {
        "system" => {
            let sub = ev.get("subtype").and_then(|v| v.as_str()).unwrap_or("?");
            let model = ev.get("model").and_then(|v| v.as_str()).unwrap_or("");
            if sub == "init" {
                eprintln!("[claude_cli {sid}] init model={model}");
            } else {
                let status = ev.get("status").and_then(|v| v.as_str()).unwrap_or("");
                eprintln!("[claude_cli {sid}] {sub} {status}");
            }
        }
        "rate_limit_event" => {
            let status = ev
                .get("rate_limit_info")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            eprintln!("[claude_cli {sid}] rate_limit {status}");
        }
        "stream_event" => {
            let inner = ev
                .get("event")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            match inner {
                "message_start" => {
                    let ttft = ev.get("ttft_ms").and_then(|v| v.as_i64()).unwrap_or(-1);
                    eprintln!("[claude_cli {sid}] message_start ttft={ttft}ms");
                }
                "content_block_start" => {
                    let kind = ev
                        .get("event")
                        .and_then(|v| v.get("content_block"))
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    eprintln!("[claude_cli {sid}] block_start kind={kind}");
                }
                "content_block_delta" => {
                    let delta = ev.get("event").and_then(|v| v.get("delta"));
                    let dtype = delta
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    // Pull whichever field carries the payload for this delta variant.
                    let chunk = delta
                        .and_then(|v| {
                            v.get("thinking")
                                .or_else(|| v.get("text"))
                                .or_else(|| v.get("partial_json"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("");
                    if !chunk.is_empty() {
                        eprintln!("[claude_cli {sid}] {dtype}: {}", truncate(chunk, 200));
                    }
                }
                "content_block_stop" => { /* low-signal, skip */ }
                "message_delta" => { /* usage update, low-signal mid-call */ }
                "message_stop" => {
                    eprintln!("[claude_cli {sid}] message_stop");
                }
                other => {
                    eprintln!("[claude_cli {sid}] stream_event/{other}");
                }
            }
        }
        "assistant" | "user" => { /* fully-assembled message — already streamed via deltas */ }
        "result" => {
            let dur = ev.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(-1);
            let usage = ev.get("usage");
            let tin = usage
                .and_then(|v| v.get("input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let tout = usage
                .and_then(|v| v.get("output_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let tcached = usage
                .and_then(|v| v.get("cache_read_input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cost = ev
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let subtype = ev.get("subtype").and_then(|v| v.as_str()).unwrap_or("?");
            eprintln!(
                "[claude_cli {sid}] result {subtype} dur={dur}ms in={tin} out={tout} cached={tcached} cost=${cost:.4}"
            );
        }
        other => {
            eprintln!("[claude_cli {sid}] {other}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    fn inv(timeout: Duration) -> SubprocessInvocation {
        SubprocessInvocation::neutral(
            "sh".to_string(),
            "test-model".to_string(),
            "prompt".to_string(),
            timeout,
            HashSet::new(),
            None,
            tars_sandbox::SandboxPolicy::default(),
        )
    }

    /// A wedged child holds stdout open and writes nothing, so the NDJSON
    /// reader never sees EOF. The invocation timeout must still fire, kill the
    /// child, and return — bounding BOTH the drain and the child's exit.
    ///
    /// Before the fix, the timeout wrapped only `child.wait()` and the pair was
    /// `join!`-ed. `join!` waits for every future, so the reader's forever-pending
    /// await swallowed the elapsed timer and this test hung.
    #[tokio::test]
    async fn timeout_fires_while_child_holds_stdout_open() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");

        let started = Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            run_streaming(&mut child, &inv(Duration::from_millis(200))),
        )
        .await
        .expect("run_streaming must return, not hang past its own timeout")
        .expect_err("a child that never writes must not succeed");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must fire near the 200ms budget, took {:?}",
            started.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "expected a timeout error, got: {msg}");
        // We killed the child; it did not die on its own — the abort is a
        // `TimedOut`, NOT a `CliSubprocessDied`, and `budget` is the invocation
        // budget the call was given (200ms here).
        match err {
            ProviderError::TimedOut { budget, .. } => {
                assert_eq!(
                    budget,
                    Duration::from_millis(200),
                    "budget must equal the invocation timeout"
                );
            }
            other => panic!("expected ProviderError::TimedOut, got: {other:?}"),
        }
    }

    /// The child is actually killed, not merely abandoned — a subprocess lives
    /// outside the future, so dropping the future would leave it running.
    #[tokio::test]
    async fn timed_out_child_is_killed() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .expect("spawn sh");

        let _ = run_streaming(&mut child, &inv(Duration::from_millis(200))).await;
        // `start_kill` was issued on the timeout path; reaping must now succeed
        // promptly rather than blocking for the child's full `sleep 30`.
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("killed child must be reapable without waiting out its sleep")
            .expect("wait");
        assert!(!status.success(), "a killed child must not report success");
    }
}
