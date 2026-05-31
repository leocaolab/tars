//! Pure helpers shared by [`super::adapter`] and [`super::provider`]:
//! stop-reason mapping, usage parsing, body truncation, and the
//! batch-API JSON converters (`translate_batch_status`,
//! `parse_batch_results`, `message_to_chat_response`). Stateless, no
//! I/O — the JSON conversion layer the L5 Tribunal split out of the
//! original god-module so the adapter and provider can be read
//! without scrolling through token-level minutiae.

use serde_json::{Value, json};

use tars_types::{
    BatchItemId, BatchResultItem, BatchStatus, ChatEvent, ChatResponse, ChatResponseBuilder,
    ProviderError, StopReason, Usage,
};

/// Map Anthropic's `stop_reason` wire string to the canonical
/// [`StopReason`]. Cross-provider conformance suite relies on these
/// mappings — keep in sync with the adapter's tests.
pub(super) fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

/// Normalize Anthropic's `usage` map into the canonical [`Usage`]
/// shape. Anthropic reports `input_tokens` **disjoint** from
/// `cache_read` / `cache_creation`. Our `Usage` is OpenAI-style:
/// `input_tokens` is the *total* prompt and includes the cached and
/// creation subsets. Normalize at the boundary so `Pricing::cost_for`
/// and `total_tokens` work uniformly across providers.
pub(super) fn parse_usage(u: &serde_json::Map<String, Value>) -> Usage {
    let api_input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cached = u
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let creation = u
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Anthropic doesn't currently break out thinking tokens in the
    // usage block (they're folded into output_tokens) but probe a
    // couple of likely spellings to future-proof.
    let thinking = u
        .get("thinking_tokens")
        .or_else(|| u.get("output_thinking_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Usage {
        input_tokens: api_input.saturating_add(cached).saturating_add(creation),
        output_tokens: output,
        cached_input_tokens: cached,
        cache_creation_tokens: creation,
        thinking_tokens: thinking,
    }
}

/// UTF-8-safe truncation; appends an ellipsis if anything was dropped.
/// Re-exported from the shared HTTP base so both backends share one copy.
pub(super) use crate::http_base::truncate;

/// Translate Anthropic's batch status JSON into our vendor-neutral
/// [`BatchStatus`]. The vendor reports `processing_status` plus a
/// `request_counts` breakdown — we collapse "ended" into one of
/// Completed / Cancelled / Expired based on the count distribution.
pub(super) fn translate_batch_status(v: &Value) -> Result<BatchStatus, ProviderError> {
    let status = v
        .get("processing_status")
        .and_then(|s| s.as_str())
        .ok_or_else(|| ProviderError::Parse("batch status: missing `processing_status`".into()))?;

    let counts = v.get("request_counts").cloned().unwrap_or_else(|| json!({}));
    let get = |k: &str| counts.get(k).and_then(|n| n.as_u64()).unwrap_or(0) as u32;
    let processing = get("processing");
    let succeeded = get("succeeded");
    let errored = get("errored");
    let canceled = get("canceled");
    let expired = get("expired");
    let processed = succeeded
        .saturating_add(errored)
        .saturating_add(canceled)
        .saturating_add(expired);
    let total = Some(processed.saturating_add(processing));

    match status {
        "in_progress" => Ok(BatchStatus::InProgress {
            processed,
            total,
            eta: None,
        }),
        "canceling" => Ok(BatchStatus::InProgress {
            processed,
            total,
            eta: None,
        }),
        "ended" => {
            // Collapse the count distribution into one terminal state.
            // Per-item issues surface in results() — the overall job
            // is Completed unless every item ended the same non-success
            // way (all cancelled / all expired).
            if processed > 0 && canceled == processed {
                Ok(BatchStatus::Cancelled)
            } else if processed > 0 && expired == processed {
                Ok(BatchStatus::Expired)
            } else {
                Ok(BatchStatus::Completed)
            }
        }
        other => Err(ProviderError::Parse(format!(
            "batch status: unknown `processing_status` value: {other:?}"
        ))),
    }
}

/// Parse Anthropic's results JSONL into [`BatchResultItem`]s. Each
/// line has `custom_id` + `result.type` ∈ {succeeded, errored,
/// canceled, expired}; we translate to a per-item
/// `Result<ChatResponse, ProviderError>`.
pub(super) fn parse_batch_results(text: &str) -> Result<Vec<BatchResultItem>, ProviderError> {
    let mut items = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| {
            ProviderError::Parse(format!("batch results line {}: not JSON: {e}", idx + 1))
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
        let result_val = v.get("result").ok_or_else(|| {
            ProviderError::Parse(format!("batch results line {}: missing result", idx + 1))
        })?;
        let result_type = result_val.get("type").and_then(|t| t.as_str()).ok_or_else(|| {
            ProviderError::Parse(format!(
                "batch results line {}: missing result.type",
                idx + 1
            ))
        })?;

        let outcome: Result<ChatResponse, ProviderError> = match result_type {
            "succeeded" => {
                let message = result_val.get("message").ok_or_else(|| {
                    ProviderError::Parse(format!(
                        "batch results line {}: succeeded but missing message",
                        idx + 1
                    ))
                })?;
                message_to_chat_response(message)
            }
            "errored" => {
                let err = result_val.get("error").cloned().unwrap_or_else(|| json!({}));
                let err_type = err.get("type").and_then(|t| t.as_str()).unwrap_or("error");
                let err_msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no message)");
                Err(match err_type {
                    "invalid_request_error" => ProviderError::InvalidRequest(err_msg.to_string()),
                    "authentication_error" => ProviderError::Auth(err_msg.to_string()),
                    "rate_limit_error" => ProviderError::RateLimited { retry_after: None },
                    "overloaded_error" => ProviderError::ModelOverloaded,
                    _ => ProviderError::Internal(format!(
                        "anthropic batch item error ({err_type}): {err_msg}"
                    )),
                })
            }
            "canceled" => Err(ProviderError::Internal("item cancelled".into())),
            "expired" => Err(ProviderError::Internal("item expired".into())),
            other => Err(ProviderError::Parse(format!(
                "batch results line {}: unknown result.type {other:?}",
                idx + 1
            ))),
        };

        items.push(BatchResultItem {
            item_id: BatchItemId::new(custom_id),
            result: outcome,
        });
    }
    Ok(items)
}

/// Convert one Anthropic message-shape JSON into a [`ChatResponse`] by
/// replaying it through [`ChatResponseBuilder`]. Text content blocks
/// become `Delta` events; we set the terminal `Finished` from
/// `stop_reason` + `usage`.
///
/// **Known gap (Phase 2)**: `tool_use` content blocks are skipped.
/// Batch consumers that need tool calls in batch responses can either
/// (a) parse the raw `message` JSON themselves, or (b) wait for V2
/// when we extend `ChatEvent::ToolCallStart/Args/End` replay here.
pub(super) fn message_to_chat_response(msg: &Value) -> Result<ChatResponse, ProviderError> {
    let model = msg
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("anthropic");
    let mut acc = ChatResponseBuilder::new();
    acc.apply(ChatEvent::started(model));

    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    acc.apply(ChatEvent::Delta {
                        text: text.to_string(),
                    });
                }
            }
            // tool_use blocks: see fn doc-comment.
        }
    }

    // Reuse map_stop_reason so the unknown-reason fallback (Other) and
    // the full reason set stay in lockstep with the streaming adapter —
    // an inline copy here previously defaulted unknowns to EndTurn.
    let stop_reason = msg
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(map_stop_reason)
        .unwrap_or(StopReason::EndTurn);

    // Reuse parse_usage so input-token normalization (input += cache_read
    // + cache_creation) and thinking-token probing match the streaming
    // path — cross-provider pricing depends on a uniform Usage shape.
    let usage = msg
        .get("usage")
        .and_then(|u| u.as_object())
        .map(parse_usage)
        .unwrap_or_default();
    acc.apply(ChatEvent::Finished { stop_reason, usage });
    Ok(acc.finish())
}
