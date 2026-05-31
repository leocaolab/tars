//! Pure helpers for OpenAI: batch status / result translation,
//! one-shot chat-completion → `ChatResponse` rebuilding, usage
//! parsing, and tool-call buffer draining. All stateless, no I/O.

use reqwest::header::HeaderMap;
use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderValue;
use serde_json::{Value, json};

use tars_types::{
    BatchItemId, BatchResultItem, BatchStatus, ChatEvent, ChatResponse, ChatResponseBuilder,
    ProviderError, StopReason, Usage,
};

use crate::auth::ResolvedAuth;
use crate::tool_buffer::ToolCallBuffer;

/// Authorization header only (no `Content-Type: application/json`) —
/// reqwest sets the right multipart `Content-Type` automatically; we
/// must not preset JSON or the boundary string gets clobbered.
pub(super) fn openai_auth_only_headers(
    auth: &ResolvedAuth,
) -> Result<HeaderMap, ProviderError> {
    let mut h = HeaderMap::new();
    match auth {
        ResolvedAuth::Bearer(t) | ResolvedAuth::ApiKey(t) => {
            let value = HeaderValue::from_str(&format!("Bearer {t}"))
                .map_err(|e| ProviderError::Internal(format!("bad auth header: {e}")))?;
            h.insert(AUTHORIZATION, value);
        }
        ResolvedAuth::None => {}
    }
    Ok(h)
}

/// OpenAI's `status` field on the batch object, translated to our
/// vendor-neutral [`BatchStatus`].
///
/// OpenAI vocab:
///   validating | in_progress | finalizing → InProgress
///   completed → Completed
///   failed → Failed
///   expired → Expired
///   cancelling → InProgress
///   cancelled → Cancelled
pub(super) fn translate_openai_batch_status(v: &Value) -> Result<BatchStatus, ProviderError> {
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .ok_or_else(|| ProviderError::Parse("batch status: missing `status`".into()))?;

    let counts = v.get("request_counts").cloned().unwrap_or_else(|| json!({}));
    // Counts arrive as u64 on the wire but `BatchStatus` carries u32.
    // Clamp instead of using a silent `as u32` truncation (which would
    // wrap a >u32::MAX count into a small bogus value).
    let clamp_u32 = |n: u64| n.min(u32::MAX as u64) as u32;
    let total = counts
        .get("total")
        .and_then(|n| n.as_u64())
        .map(clamp_u32);
    let completed = clamp_u32(counts.get("completed").and_then(|n| n.as_u64()).unwrap_or(0));
    let failed = clamp_u32(counts.get("failed").and_then(|n| n.as_u64()).unwrap_or(0));
    // Saturating add: the sum of two clamped u32s can exceed u32::MAX,
    // which would panic (debug) or wrap (release) with plain `+`.
    let processed = completed.saturating_add(failed);

    match status {
        "validating" | "in_progress" | "finalizing" | "cancelling" => {
            Ok(BatchStatus::InProgress {
                processed,
                total,
                eta: None,
            })
        }
        "completed" => Ok(BatchStatus::Completed),
        "expired" => Ok(BatchStatus::Expired),
        "cancelled" => Ok(BatchStatus::Cancelled),
        "failed" => {
            // OpenAI surfaces batch-level reasons via the `errors`
            // object, whose `data` field is an array of
            // `{code, message, param, line}` entries (it is a list
            // wrapper, not a bare array). We pull the first entry's
            // `message` and collapse to a one-message Failed.
            let message = v
                .get("errors")
                .and_then(|e| e.get("data"))
                .and_then(|d| d.as_array())
                .and_then(|arr| arr.first())
                .and_then(|first| first.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("batch failed");
            Ok(BatchStatus::Failed {
                kind: "batch_failed".into(),
                message: message.to_string(),
            })
        }
        other => Err(ProviderError::Parse(format!(
            "batch status: unknown `status` value: {other:?}"
        ))),
    }
}

/// Parse OpenAI's output file JSONL into [`BatchResultItem`]s.
pub(super) fn parse_openai_batch_results(
    text: &str,
) -> Result<Vec<BatchResultItem>, ProviderError> {
    let mut items = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| {
            ProviderError::Parse(format!(
                "batch results line {}: not JSON: {e}",
                idx + 1
            ))
        })?;
        let custom_id = v
            .get("custom_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                ProviderError::Parse(format!(
                    "batch results line {}: missing custom_id",
                    idx + 1
                ))
            })?
            .to_string();

        // Item-level error takes precedence if present.
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            let code = err
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("error");
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("(no message)");
            let pe = match code {
                "invalid_request" | "invalid_request_error" => {
                    ProviderError::InvalidRequest(msg.to_string())
                }
                "rate_limit_exceeded" => ProviderError::RateLimited { retry_after: None },
                _ => ProviderError::Internal(format!("openai batch item error ({code}): {msg}")),
            };
            items.push(BatchResultItem {
                item_id: BatchItemId::new(custom_id),
                result: Err(pe),
            });
            continue;
        }

        let response = v.get("response").ok_or_else(|| {
            ProviderError::Parse(format!(
                "batch results line {}: missing response and no error",
                idx + 1
            ))
        })?;
        let body = response.get("body").ok_or_else(|| {
            ProviderError::Parse(format!(
                "batch results line {}: response missing body",
                idx + 1
            ))
        })?;
        items.push(BatchResultItem {
            item_id: BatchItemId::new(custom_id),
            result: openai_chat_completion_to_chat_response(body),
        });
    }
    Ok(items)
}

/// Convert one OpenAI chat-completion response body into [`ChatResponse`]
/// by replaying through [`ChatResponseBuilder`]. Same shape as the
/// streaming end-state, just delivered all-at-once.
///
/// **Known gap (Phase 3)**: tool_calls in batch responses are skipped.
/// Same V1 limitation as the Anthropic backend (`anthropic_message_to_chat_response`).
fn openai_chat_completion_to_chat_response(body: &Value) -> Result<ChatResponse, ProviderError> {
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("openai");
    let mut acc = ChatResponseBuilder::new();
    acc.apply(ChatEvent::started(model));

    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| ProviderError::Parse("openai batch response: choices empty".into()))?;
    let message = choice.get("message").ok_or_else(|| {
        ProviderError::Parse("openai batch response: choice missing message".into())
    })?;

    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            acc.apply(ChatEvent::Delta {
                text: text.to_string(),
            });
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(|f| f.as_str()) {
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::ContentFilter,
        Some("tool_calls") => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    };

    let u = body.get("usage").cloned().unwrap_or_else(|| json!({}));
    let usage_u64 = |k: &str| u.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
    // OpenAI nested cached count: usage.prompt_tokens_details.cached_tokens
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let usage = Usage {
        input_tokens: usage_u64("prompt_tokens"),
        output_tokens: usage_u64("completion_tokens"),
        cached_input_tokens: cached,
        cache_creation_tokens: 0,
        thinking_tokens: 0,
    };
    acc.apply(ChatEvent::Finished { stop_reason, usage });
    Ok(acc.finish())
}

pub(super) fn parse_openai_usage(usage: &serde_json::Map<String, Value>) -> Usage {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Usage {
        input_tokens: prompt,
        output_tokens: completion,
        cached_input_tokens: cached,
        cache_creation_tokens: 0,
        thinking_tokens: 0,
    }
}

/// Drain whatever indices the buffer has into ToolCallEnd events.
/// Replaces the broken finalization loop in `parse_event`.
///
/// Audit `tars-provider-src-backends-openai-29`: previously swallowed
/// finalize errors with `if let Ok(...)`, leaving consumers in an
/// inconsistent state when args were malformed. Now propagates.
/// Indices that were never started simply don't show up in the
/// inflight map and yield a benign `not started` error we filter out.
pub(super) fn drain_buffer_into(
    buf: &mut ToolCallBuffer,
    out: &mut Vec<ChatEvent>,
) -> Result<(), ProviderError> {
    // We don't have a public iter on ToolCallBuffer; finalize indices
    // 0..32 (parallel call ceiling we treat as practical max).
    for i in 0..32 {
        match buf.finalize(i) {
            Ok((id, _name, parsed)) => {
                out.push(ChatEvent::ToolCallEnd {
                    index: i,
                    id,
                    parsed_args: parsed,
                });
            }
            Err(ProviderError::Parse(msg)) if msg.contains("not started") => {
                // Index was never used in this stream — fine.
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
