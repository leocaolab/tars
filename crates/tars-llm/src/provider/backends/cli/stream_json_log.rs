//! Reads claude's `stream-json` output (on when `TARS_CLAUDE_CLI_STREAM` is set): logs a one-line
//! summary of each event to `tracing` target `crate::cli::stream` and returns the final
//! `result` event. It does not hand events to the caller as they arrive, and it writes to no
//! terminal: the host's subscriber decides where the summaries go.

use std::pin::Pin;
use std::task::{Context, Poll};

use serde_json::Value;
use tokio::io::{AsyncRead, ReadBuf};

use super::cli_subprocess_died;
use crate::ProviderError;
use crate::provider::error::ParseProblem;
use tars_types::JsonKind;

use super::invocation::{ChildIo, SubprocessInvocation};
use super::subprocess::{
    ChildEnd, end_child, read_pipe, reported_error, truncate, wait_with_reads,
};
use crate::provider::wire::ReceivedBytes;
/// Reads a spawned `claude -p --output-format stream-json` until it exits and returns its
/// `result` event without the `type` key. Every byte read from stdout and stderr is noted into
/// `ex`, however the run ends; the process group `group` is killed at the end.
///
/// # Errors
///
/// Fails with `TimedOut` past the invocation's timeout (the child is killed), `CliSubprocessDied`
/// on a failed read, a non-zero exit or an `is_error` result, and `Parse` when no `result` event
/// came.
pub(crate) async fn run_streaming(
    child: &mut tokio::process::Child,
    group: Option<u32>,
    inv: &SubprocessInvocation,
    ex: &mut ChildIo,
) -> Result<Value, ProviderError> {
    let stdout = ReceivedBytes::default();
    let stderr = ReceivedBytes::default();
    let result = stream_child(child, group, inv, &stdout, &stderr).await;
    ex.stdout = stdout.take();
    ex.stderr = stderr.take();
    result
}

async fn stream_child(
    child: &mut tokio::process::Child,
    group: Option<u32>,
    inv: &SubprocessInvocation,
    stdout_seen: &ReceivedBytes,
    stderr_seen: &ReceivedBytes,
) -> Result<Value, ProviderError> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let stdout = child.stdout.take().ok_or_else(|| {
        cli_subprocess_died(
            None,
            "stream-json: stdout pipe missing on spawned child".into(),
        )
    })?;
    let stderr_pipe = child.stderr.take().ok_or_else(|| {
        cli_subprocess_died(
            None,
            "stream-json: stderr pipe missing on spawned child".into(),
        )
    })?;

    let mut stderr_pipe = Tee::new(stderr_pipe, stderr_seen.clone());
    let mut stderr_buf = Vec::new();
    let mut reader = BufReader::new(Tee::new(stdout, stdout_seen.clone())).lines();
    let mut final_result: Option<Value> = None;
    let mut session_short: String = "????????".into();

    let end = {
        let read_stdout = async {
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        let parsed: Result<Value, _> = serde_json::from_str(&line);
                        match parsed {
                            Ok(ev) => {
                                if session_short == "????????" {
                                    if let Some(sid) = ev.get("session_id").and_then(|v| v.as_str())
                                    {
                                        session_short = sid.chars().take(8).collect();
                                    }
                                }
                                emit_event_summary(&ev, &session_short);
                                if ev.get("type").and_then(|v| v.as_str()) == Some("result") {
                                    final_result = Some(ev);
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    target: "crate::cli::stream",
                                    "[claude_cli {session_short}] !! non-json stdout line ({e}): {}",
                                    truncate(&line, 200)
                                );
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Err(cli_subprocess_died(
                            None,
                            format!("stream read failed: {e}"),
                        ));
                    }
                }
            }
            Ok::<(), ProviderError>(())
        };
        let read_stderr = async {
            if let Err(e) = read_pipe(Some(&mut stderr_pipe), &mut stderr_buf).await {
                tracing::warn!(error = %e, "claude_cli stream: failed to read child stderr");
            }
        };
        // Both pipes are read while waiting, or a full stderr pipe would block the child.
        let reads = async { tokio::join!(read_stdout, read_stderr).0 };
        wait_with_reads(child, reads, inv.timeout).await
    };

    let (status, read_res) = match end {
        ChildEnd::TimedOut => {
            end_child(child, group, true).await;
            let stderr_s = truncate(&String::from_utf8_lossy(&stderr_buf), 500);
            return Err(ProviderError::TimedOut {
                budget: inv.timeout,
                detail: format!(
                    "stream-json child killed after wall-clock timeout (model={}, prompt_chars={}, stderr: {stderr_s})",
                    inv.model,
                    inv.prompt.len()
                ),
            });
        }
        ChildEnd::Exited { status, reads } => {
            end_child(child, group, false).await;
            // Pipes still held open past the grace are read as far as they got.
            (status, reads.unwrap_or(Ok(())))
        }
    };

    read_res?;

    let status = match status {
        Ok(s) => s,
        Err(e) => {
            let stderr_s = truncate(&String::from_utf8_lossy(&stderr_buf), 500);
            return Err(cli_subprocess_died(
                None,
                format!("wait failed: {e} (stderr: {stderr_s})"),
            ));
        }
    };

    if !status.success() {
        let stderr_s = String::from_utf8_lossy(&stderr_buf).to_string();
        let truncated = truncate(&stderr_s, 500);
        return Err(cli_subprocess_died(status.code(), truncated));
    }

    let mut result = final_result.ok_or_else(|| ProviderError::Parse {
        at: "claude stream-json output",
        line: None,
        problem: Box::new(ParseProblem::Missing("result")),
        raw: None,
    })?;
    if let Some(obj) = result.as_object_mut() {
        obj.remove("type");
    }

    if !result.is_object() {
        return Err(ProviderError::Parse {
            at: "claude stream-json result",
            line: None,
            problem: Box::new(ParseProblem::WrongType {
                expected: JsonKind::Object,
                got: JsonKind::of(&result),
            }),
            raw: Some(truncate(&result.to_string(), 300)),
        });
    }

    if let Some(detail) = reported_error(&result) {
        return Err(cli_subprocess_died(
            status.code(),
            format!("claude reported is_error: {detail}"),
        ));
    }

    Ok(result)
}

/// A reader that notes every byte it reads, before its caller parses any.
struct Tee<R> {
    inner: R,
    seen: ReceivedBytes,
}

impl<R> Tee<R> {
    fn new(inner: R, seen: ReceivedBytes) -> Self {
        Self { inner, seen }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Tee<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = polled {
            self.seen.push(&buf.filled()[before..]);
        }
        polled
    }
}

/// Logs one line about a stream-json event: the final result at info, the rest at debug.
fn emit_event_summary(ev: &Value, sid: &str) {
    let evtype = ev.get("type").and_then(|v| v.as_str()).unwrap_or("?");

    match evtype {
        "system" => {
            let sub = ev.get("subtype").and_then(|v| v.as_str()).unwrap_or("?");
            let model = ev.get("model").and_then(|v| v.as_str()).unwrap_or("");
            if sub == "init" {
                tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] init model={model}");
            } else {
                let status = ev.get("status").and_then(|v| v.as_str()).unwrap_or("");
                tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] {sub} {status}");
            }
        }
        "rate_limit_event" => {
            let status = ev
                .get("rate_limit_info")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] rate_limit {status}");
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
                    tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] message_start ttft={ttft}ms");
                }
                "content_block_start" => {
                    let kind = ev
                        .get("event")
                        .and_then(|v| v.get("content_block"))
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] block_start kind={kind}");
                }
                "content_block_delta" => {
                    let delta = ev.get("event").and_then(|v| v.get("delta"));
                    let dtype = delta
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let chunk = delta
                        .and_then(|v| {
                            v.get("thinking")
                                .or_else(|| v.get("text"))
                                .or_else(|| v.get("partial_json"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("");
                    if !chunk.is_empty() {
                        tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] {dtype}: {}", truncate(chunk, 200));
                    }
                }
                "content_block_stop" => {}
                "message_delta" => {}
                "message_stop" => {
                    tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] message_stop");
                }
                other => {
                    tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] stream_event/{other}");
                }
            }
        }
        // Their text was already logged delta by delta.
        "assistant" | "user" => {}
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
            tracing::info!(
                target: "crate::cli::stream",
                "[claude_cli {sid}] result {subtype} dur={dur}ms in={tin} out={tout} cached={tcached} cost=${cost:.4}"
            );
        }
        other => {
            tracing::debug!(target: "crate::cli::stream", "[claude_cli {sid}] {other}");
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
            run_streaming(
                &mut child,
                None,
                &inv(Duration::from_millis(200)),
                &mut ChildIo::default(),
            ),
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
        assert!(
            msg.contains("timed out"),
            "expected a timeout error, got: {msg}"
        );
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

    /// A child is not a future: one dropped without a kill keeps running.
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

        let _ = run_streaming(
            &mut child,
            None,
            &inv(Duration::from_millis(200)),
            &mut ChildIo::default(),
        )
        .await;
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("killed child must be reapable without waiting out its sleep")
            .expect("wait");
        assert!(!status.success(), "a killed child must not report success");
    }

    #[tokio::test]
    async fn the_result_comes_back_when_claude_exits_though_a_process_it_left_holds_stderr() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(
                "(sleep 20 1>&2 &)\n\
                 echo '{\"type\":\"result\",\"result\":\"done\",\"is_error\":false}'\nexit 0",
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let group = child.id();

        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_streaming(
                &mut child,
                group,
                &inv(Duration::from_secs(30)),
                &mut ChildIo::default(),
            ),
        )
        .await
        .expect("the run ends when the child exits")
        .expect("the result event came");
        assert_eq!(result["result"], "done");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
