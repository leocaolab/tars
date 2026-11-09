//! Reads Anthropic's stop reasons, usage, batch objects, batch result files and whole messages.
//! Nothing here does IO.

use serde_json::{Value, json};

use super::adapter::error_of_type;
use crate::provider::error::ParseProblem;
use crate::{BatchResultItem, BatchStatus, ProviderError};
use tars_types::{BatchItemId, ChatEvent, ChatResponse, ChatResponseBuilder, StopReason, Usage};

/// Returns the stop reason of an Anthropic `stop_reason`; an unknown one is `Other`.
pub(super) fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

/// Returns the usage of an Anthropic usage object, whose `input_tokens` excludes the cached and
/// cache-creation tokens that [`Usage::input_tokens`] includes.
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
    // Anthropic counts thinking in `output_tokens`; neither field is documented, and both
    // are read in case one appears.
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

pub(super) use crate::provider::http_base::truncate;

/// Returns the status of an Anthropic batch object. An ended batch is `Cancelled` or `Expired`
/// only when every item was; otherwise it is `Completed`, and failed items show in its results.
/// A count above `u32::MAX` is clamped to it.
///
/// # Errors
///
/// Fails with `Parse` when `processing_status` is missing or unknown.
pub(super) fn translate_batch_status(v: &Value) -> Result<BatchStatus, ProviderError> {
    let status = v
        .get("processing_status")
        .and_then(|s| s.as_str())
        .ok_or_else(|| batch_status_error(ParseProblem::Missing("processing_status")))?;

    let counts = v
        .get("request_counts")
        .cloned()
        .unwrap_or_else(|| json!({}));
    // `BatchStatus` counts in u32; a larger count is clamped, not wrapped. An absent count is 0.
    let get = |k: &str| {
        let n = counts.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
        u32::try_from(n).unwrap_or(u32::MAX)
    };
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
            if processed > 0 && canceled == processed {
                Ok(BatchStatus::Cancelled)
            } else if processed > 0 && expired == processed {
                Ok(BatchStatus::Expired)
            } else {
                Ok(BatchStatus::Completed)
            }
        }
        other => Err(batch_status_error(ParseProblem::UnexpectedValue {
            field: "processing_status",
            value: other.to_string(),
        })),
    }
}

fn batch_status_error(problem: ParseProblem) -> ProviderError {
    ProviderError::Parse {
        at: "anthropic batch status",
        line: None,
        problem: Box::new(problem),
        raw: None,
    }
}

/// Returns the error of line `line` (counted from 1) of a batch results file.
fn results_line_error(line: usize, problem: ParseProblem) -> ProviderError {
    ProviderError::Parse {
        at: "anthropic batch results",
        line: Some(line),
        problem: Box::new(problem),
        raw: None,
    }
}

/// Returns one item per non-blank line of a batch result file; a `result.type` other than
/// `succeeded` becomes that item's `Err`.
///
/// # Errors
///
/// Fails with `Parse` on a line that is not JSON, lacks `custom_id`, `result` or `result.type`,
/// has an empty `custom_id`, or has an unknown type.
pub(super) fn parse_batch_results(text: &str) -> Result<Vec<BatchResultItem>, ProviderError> {
    let mut items = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let n = idx + 1;
        let v: Value =
            serde_json::from_str(line).map_err(|e| results_line_error(n, ParseProblem::Json(e)))?;
        let item_id: BatchItemId = v
            .get("custom_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| results_line_error(n, ParseProblem::Missing("custom_id")))?
            .parse()
            .map_err(|e| results_line_error(n, ParseProblem::ItemId(e)))?;
        let result_val = v
            .get("result")
            .ok_or_else(|| results_line_error(n, ParseProblem::Missing("result")))?;
        let result_type = result_val
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| results_line_error(n, ParseProblem::Missing("result.type")))?;

        let outcome: Result<ChatResponse, ProviderError> = match result_type {
            "succeeded" => {
                let message = result_val.get("message").ok_or_else(|| {
                    results_line_error(n, ParseProblem::Missing("result.message"))
                })?;
                message_to_chat_response(message)
            }
            "errored" => {
                let err = result_val
                    .get("error")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let err_type = err.get("type").and_then(|t| t.as_str()).unwrap_or("error");
                let err_msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map_or_else(|| err.to_string(), str::to_string);
                Err(error_of_type(err_type, err_msg).unwrap_or_else(|message| {
                    ProviderError::BatchItemFailed {
                        code: err_type.to_string(),
                        message,
                    }
                }))
            }
            "canceled" => Err(ProviderError::BatchItemCanceled),
            "expired" => Err(ProviderError::BatchItemExpired),
            other => Err(results_line_error(
                n,
                ParseProblem::UnexpectedValue {
                    field: "result.type",
                    value: other.to_string(),
                },
            )),
        };

        items.push(BatchResultItem {
            item_id,
            result: outcome,
        });
    }
    Ok(items)
}

/// Returns a whole message as a response, reading its text, thinking and `tool_use` blocks as a
/// stream of the same message would; one with no `stop_reason` has no stop reason, as a stream
/// that never said it.
///
/// # Errors
///
/// `Parse` carrying the block's text when a `tool_use` block has no id or name, or its `input`
/// is not an object: the call could not be routed back.
pub(super) fn message_to_chat_response(msg: &Value) -> Result<ChatResponse, ProviderError> {
    let model = msg
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("anthropic");
    let mut acc = ChatResponseBuilder::new();
    acc.apply(ChatEvent::started(model));

    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        for (index, block) in blocks.iter().enumerate() {
            for event in block_events(index, block)? {
                acc.apply(event);
            }
        }
    }

    let stop_reason = msg
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(map_stop_reason);
    let usage = msg
        .get("usage")
        .and_then(|u| u.as_object())
        .map(parse_usage);
    if let Some(end) = ChatEvent::stream_end(stop_reason, usage) {
        acc.apply(end);
    }
    Ok(acc.finish())
}

/// Returns the events of one content block of a whole message, at position `index`; a block of
/// another type gives none, as in the stream.
fn block_events(index: usize, block: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
    let text_of = |key: &str| block.get(key).and_then(|t| t.as_str()).map(str::to_string);
    let events = match block.get("type").and_then(|t| t.as_str()) {
        Some("text") => text_of("text")
            .map(|text| ChatEvent::Delta { text })
            .into_iter()
            .collect(),
        Some("thinking") => text_of("thinking")
            .map(|text| ChatEvent::ThinkingDelta { text })
            .into_iter()
            .collect(),
        Some("tool_use") => {
            let field = |key: &'static str| {
                text_of(key)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| tool_use_error(block, ParseProblem::Missing(key)))
            };
            let id = field("id")?;
            let name = field("name")?;
            let parsed_args = match block.get("input") {
                Some(Value::Object(args)) => args.clone(),
                None => serde_json::Map::new(),
                Some(other) => {
                    return Err(tool_use_error(
                        block,
                        ParseProblem::UnexpectedValue {
                            field: "input",
                            value: truncate(&other.to_string(), 200),
                        },
                    ));
                }
            };
            vec![
                ChatEvent::ToolCallStart {
                    index,
                    id: id.clone(),
                    name,
                },
                ChatEvent::ToolCallEnd {
                    index,
                    id,
                    parsed_args,
                    thought_signature: None,
                },
            ]
        }
        _ => Vec::new(),
    };
    Ok(events)
}

fn tool_use_error(block: &Value, problem: ParseProblem) -> ProviderError {
    ProviderError::Parse {
        at: "anthropic message tool_use",
        line: None,
        problem: Box::new(problem),
        raw: Some(truncate(&block.to_string(), 300)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_errored_item_without_a_message_carries_its_error_object() {
        let line = r#"{"custom_id": "a", "result": {"type": "errored", "error": {"type": "weird_error", "detail": 7}}}"#;
        let items = parse_batch_results(line).unwrap();
        let err = items[0].result.as_ref().expect_err("errored");
        assert!(
            matches!(err, ProviderError::BatchItemFailed { message, .. } if message.contains("\"detail\":7")),
            "{err:?}"
        );
    }

    #[test]
    fn an_empty_custom_id_is_a_parse_error_not_a_panic() {
        let line = r#"{"custom_id": "", "result": {"type": "expired"}}"#;
        let err = parse_batch_results(line).expect_err("an empty custom_id is no item id");
        assert!(matches!(err, ProviderError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn a_message_with_no_stop_reason_has_none_and_keeps_its_usage() {
        let msg = serde_json::json!({
            "model": "claude",
            "content": [{ "type": "text", "text": "hi" }],
            "usage": { "input_tokens": 10, "output_tokens": 4 }
        });
        let r = message_to_chat_response(&msg).unwrap();
        assert_eq!(r.stop_reason, None);
        assert_eq!(r.usage.output_tokens, 4);

        let msg =
            serde_json::json!({ "model": "claude", "content": [], "stop_reason": "max_tokens" });
        let r = message_to_chat_response(&msg).unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn a_tool_use_block_in_a_whole_message_becomes_a_tool_call() {
        let msg = serde_json::json!({
            "model": "claude-x",
            "content": [{"type": "tool_use", "id": "t1", "name": "__respond_with__", "input": {"answer": 42}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let r = message_to_chat_response(&msg).unwrap();
        assert_eq!(r.tool_calls.len(), 1, "{r:?}");
        assert_eq!(r.tool_calls[0].id, "t1");
        assert_eq!(r.tool_calls[0].name, "__respond_with__");
        assert_eq!(r.tool_calls[0].arguments["answer"], 42);
    }

    #[test]
    fn a_tool_use_block_without_an_id_is_a_parse_error_carrying_the_block() {
        let msg = serde_json::json!({
            "content": [{"type": "tool_use", "name": "grep", "input": {}}]
        });
        let err = message_to_chat_response(&msg).unwrap_err();
        let ProviderError::Parse { raw, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(
            raw.as_deref().is_some_and(|r| r.contains("grep")),
            "{err:?}"
        );
    }
}
