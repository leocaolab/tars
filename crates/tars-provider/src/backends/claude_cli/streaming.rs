//! Stream-json mode for the Claude CLI subprocess, gated by
//! `TARS_CLAUDE_CLI_STREAM`. Owns the NDJSON event reader and the
//! human-readable per-event summary emitted to stderr for
//! observability. Lives in its own file because it carries the most
//! complexity (event taxonomy + line-by-line reader + stderr drain)
//! and isn't on the happy path of the buffered runner.

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
pub(super) async fn run_streaming(
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
        BufReader::new(stderr_pipe).read_to_end(&mut buf).await.ok();
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
                        Err(_) => {
                            // Non-JSON line on stdout — claude shouldn't
                            // emit these in stream-json mode, but if it
                            // does, surface them rather than swallowing.
                            eprintln!(
                                "[claude_cli {session_short}] !! non-json stdout line: {}",
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

    let wait_fut = tokio::time::timeout(inv.timeout, child.wait());

    let (read_res, wait_res) = tokio::join!(read_fut, wait_fut);
    let stderr_buf = stderr_task.await.unwrap_or_default();

    read_res?;

    let status = match wait_res {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("wait failed: {e}"),
            });
        }
        Err(_) => {
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
