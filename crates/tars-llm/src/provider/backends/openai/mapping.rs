//! Reads OpenAI's batch objects, batch result files, whole completions and usage. Nothing here
//! does IO.

use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use serde_json::Value;

use crate::provider::error::{
    CredentialError, ParseProblem, ProviderRefusal, RequestRefusal, status_error,
};
use crate::provider::http_base::truncate;
use crate::provider::tool_buffer::{StreamDecodeState, StreamEnd};
use crate::{BatchResultItem, BatchStatus, ProviderError};
use tars_types::{BatchItemId, ChatEvent, ChatResponse, ChatResponseBuilder, StopReason, Usage};

use crate::provider::auth::ResolvedAuth;

use super::dialect::OpenAiDialect;

/// Caps, in bytes, the raw payload a parse error carries: enough to see what came back, not a
/// whole body.
const RAW_ERR_CAP: usize = 300;

/// Returns only the authorization header. A multipart upload needs reqwest's own `Content-Type`,
/// which carries the boundary; a preset JSON one would replace it.
///
/// # Errors
///
/// Fails with `Auth` when the token is not a valid header value.
pub(super) fn openai_auth_only_headers(auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
    let mut h = HeaderMap::new();
    match auth {
        ResolvedAuth::Bearer(t) | ResolvedAuth::ApiKey(t) => {
            let value = HeaderValue::from_str(&format!("Bearer {t}")).map_err(|source| {
                ProviderError::Auth(CredentialError::Header {
                    header: "authorization",
                    source,
                })
            })?;
            h.insert(AUTHORIZATION, value);
        }
        ResolvedAuth::None => {}
    }
    Ok(h)
}

/// Returns the status of an OpenAI batch object; `validating`, `in_progress`, `finalizing` and
/// `cancelling` all count as in progress.
///
/// # Errors
///
/// Fails with `Parse` when `status` is missing or unknown.
pub(super) fn translate_openai_batch_status(v: &Value) -> Result<BatchStatus, ProviderError> {
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .ok_or_else(|| ProviderError::Parse {
            at: "openai batch status",
            line: None,
            problem: Box::new(ParseProblem::Missing("status")),
            raw: Some(truncate(&v.to_string(), RAW_ERR_CAP)),
        })?;

    // OpenAI reports `request_counts` only in some states; a count it does not report is `None`.
    // `BatchStatus` counts in u32; a larger count is clamped, not wrapped.
    let count = |k: &str| {
        v.get("request_counts")
            .and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    let total = count("total");
    let processed = count("completed")
        .zip(count("failed"))
        .map(|(completed, failed)| completed.saturating_add(failed));

    match status {
        "validating" | "in_progress" | "finalizing" | "cancelling" => Ok(BatchStatus::InProgress {
            processed,
            total,
            eta: None,
        }),
        "completed" => Ok(BatchStatus::Completed),
        "expired" => Ok(BatchStatus::Expired),
        "cancelled" => Ok(BatchStatus::Cancelled),
        "failed" => {
            // `errors` is a list object whose `data` holds `{code, message, param, line}`
            // entries; only the first message is kept.
            let message = v
                .get("errors")
                .and_then(|e| e.get("data"))
                .and_then(|d| d.as_array())
                .and_then(|arr| arr.first())
                .and_then(|first| first.get("message"))
                .and_then(|m| m.as_str())
                .map_or_else(|| v.to_string(), str::to_string);
            Ok(BatchStatus::Failed {
                kind: "batch_failed".into(),
                message,
            })
        }
        other => Err(ProviderError::Parse {
            at: "openai batch status",
            line: None,
            problem: Box::new(ParseProblem::UnexpectedValue {
                field: "status",
                value: other.to_string(),
            }),
            raw: None,
        }),
    }
}

/// Returns one item per non-blank line of a batch output or error file, each completion read by
/// `dialect`, so its quirks apply as they do when streaming. An item-level error, or a response
/// whose `status_code` is not 2xx (the lines of the error file), becomes that item's `Err`.
///
/// # Errors
///
/// Fails with `Parse` on a line that is not JSON, lacks `custom_id` or has an empty one, or has
/// neither an error nor a response body.
pub(super) fn parse_openai_batch_results(
    dialect: &dyn OpenAiDialect,
    text: &str,
) -> Result<Vec<BatchResultItem>, ProviderError> {
    let mut items = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let unparsed = |problem| ProviderError::Parse {
            at: "openai batch results",
            line: Some(idx + 1),
            problem: Box::new(problem),
            raw: Some(truncate(line, RAW_ERR_CAP)),
        };
        let v: Value = serde_json::from_str(line).map_err(|e| unparsed(ParseProblem::Json(e)))?;
        let item_id: BatchItemId = v
            .get("custom_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| unparsed(ParseProblem::Missing("custom_id")))?
            .parse()
            .map_err(|e| unparsed(ParseProblem::ItemId(e)))?;

        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            items.push(BatchResultItem {
                item_id,
                result: Err(batch_item_error(err, None)),
            });
            continue;
        }

        // A line without an error must have a response.
        let response = v
            .get("response")
            .ok_or_else(|| unparsed(ParseProblem::Missing("response")))?;
        let body = response
            .get("body")
            .ok_or_else(|| unparsed(ParseProblem::Missing("response.body")))?;
        let status = response.get("status_code").and_then(Value::as_u64);
        let result = match status {
            Some(code) if !(200..300).contains(&code) => match u16::try_from(code) {
                // A gateway's error body may have no `error` object; then the body is the error.
                Ok(code) => Err(batch_item_error(
                    body.get("error").unwrap_or(body),
                    Some(code),
                )),
                Err(_) => Err(unparsed(ParseProblem::UnexpectedValue {
                    field: "response.status_code",
                    value: code.to_string(),
                })),
            },
            _ => dialect.parse_response(body),
        };
        items.push(BatchResultItem { item_id, result });
    }
    Ok(items)
}

/// Returns the error of one batch item from OpenAI's `{code, type, message}` error object and,
/// for an item the API answered, its response's HTTP status: a code this build knows decides,
/// then the status; with neither the item fails in OpenAI's words, with no code when the object
/// names none.
fn batch_item_error(err: &Value, status: Option<u16>) -> ProviderError {
    let code = err
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| err.get("type").and_then(Value::as_str));
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| err.to_string(), str::to_string);
    match code {
        Some("invalid_request" | "invalid_request_error") => {
            ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal {
                status: None,
                message: msg,
            }))
        }
        Some("rate_limit_exceeded") => ProviderError::RateLimited { retry_after: None },
        Some("batch_expired") => ProviderError::BatchItemExpired,
        Some("batch_cancelled") => ProviderError::BatchItemCanceled,
        _ => match status {
            Some(status) => status_error(status, msg, None),
            None => ProviderError::BatchItemFailed {
                code: code.map(str::to_string),
                message: msg,
            },
        },
    }
}

/// Returns the stop reason an OpenAI `finish_reason` names, `Other` for one it does not know.
pub(super) fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        _ => StopReason::Other,
    }
}

/// Returns the error of a whole completion that does not read, carrying the body.
fn completion_error(body: &Value, problem: ParseProblem) -> ProviderError {
    ProviderError::Parse {
        at: "openai batch response",
        line: None,
        problem: Box::new(problem),
        raw: Some(truncate(&body.to_string(), RAW_ERR_CAP)),
    }
}

/// Returns the events one part of an assistant message says, in the order a model writes them:
/// its reasoning, its text, then each tool call's start and arguments. A stream chunk's `delta`
/// and a whole completion's `message` have one shape, so both paths read a reply here and get
/// the same content.
///
/// # Errors
///
/// Fails with `Missing` on a tool call that names only one of its id and its name.
pub(super) fn message_events(part: &Value) -> Result<Vec<ChatEvent>, ParseProblem> {
    let mut out = Vec::new();

    // Reasoning comes as `reasoning_content` (DeepSeek-R1, many LM Studio models) or
    // `reasoning` (mlx_lm.server, some Qwen variants); the first wins.
    let reasoning = part
        .get("reasoning_content")
        .and_then(Value::as_str)
        .or_else(|| part.get("reasoning").and_then(Value::as_str));
    if let Some(text) = reasoning.filter(|t| !t.is_empty()) {
        out.push(ChatEvent::ThinkingDelta {
            text: text.to_string(),
        });
    }
    if let Some(text) = part
        .get("content")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        out.push(ChatEvent::Delta {
            text: text.to_string(),
        });
    }

    let calls = part.get("tool_calls").and_then(Value::as_array);
    for (position, tc) in calls.into_iter().flatten().enumerate() {
        // A missing `index` falls back to the position in this part; 0 would merge parallel calls
        // into one. A whole message carries no `index`.
        let index = match tc.get("index").and_then(Value::as_u64) {
            Some(i) => usize::try_from(i).map_err(|_| ParseProblem::IndexOverflow(i))?,
            None => position,
        };
        let function = tc.get("function");
        let id = non_empty_str(tc.get("id"));
        let name = non_empty_str(function.and_then(|f| f.get("name")));

        // OpenAI sends a call's id and name once, in its first chunk; later chunks for the same
        // `index` carry only arguments. An invented id would hide the server's error.
        match (id, name) {
            (Some(id), Some(name)) => out.push(ChatEvent::ToolCallStart {
                index,
                id: id.to_string(),
                name: name.to_string(),
            }),
            (Some(_), None) => return Err(ParseProblem::Missing("function.name")),
            (None, Some(_)) => return Err(ParseProblem::Missing("id")),
            (None, None) => {}
        }
        if let Some(args) = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
        {
            out.push(ChatEvent::ToolCallArgsDelta {
                index,
                args_delta: args.to_string(),
            });
        }
    }
    Ok(out)
}

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Opens in `buf` each tool call `events` start and adds the arguments they carry, so the calls
/// can be ended and their arguments parsed once whole.
pub(super) fn track_tool_calls(buf: &mut StreamDecodeState, events: &[ChatEvent]) {
    for ev in events {
        match ev {
            ChatEvent::ToolCallStart { index, id, name } => {
                buf.on_start(*index, id.clone(), name.clone());
            }
            ChatEvent::ToolCallArgsDelta { index, args_delta } => buf.on_delta(*index, args_delta),
            _ => {}
        }
    }
}

/// Returns a whole (non-streamed) completion as a response, its message read as one stream delta
/// would be ([`message_events`]) and its usage by `parse_usage`. Its tool calls end as a stream's
/// do: at its `finish_reason`, or, with none, as at a stream's `[DONE]`. One with no
/// `finish_reason` has no stop reason, as a stream that never said it.
///
/// # Errors
///
/// Fails with `Parse` when there is no first choice or it has no message, or a tool call names
/// only one of its id and its name; with the error a stream gives for a tool call whose
/// arguments are not a JSON object.
pub(super) fn openai_chat_completion_to_chat_response(
    body: &Value,
    parse_usage: impl Fn(&serde_json::Map<String, Value>) -> Usage,
) -> Result<ChatResponse, ProviderError> {
    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| completion_error(body, ParseProblem::Missing("choices[0]")))?;
    let message = choice
        .get("message")
        .ok_or_else(|| completion_error(body, ParseProblem::Missing("choices[0].message")))?;
    let events = message_events(message).map_err(|problem| completion_error(body, problem))?;

    let mut calls = StreamDecodeState::new();
    track_tool_calls(&mut calls, &events);
    let stop_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(map_finish_reason);
    let ends = match stop_reason {
        Some(stop) => calls.end_inflight(stop)?,
        None => calls.end_open_calls(StreamEnd::Terminator)?,
    };
    let usage = body
        .get("usage")
        .and_then(|u| u.as_object())
        .map(parse_usage);

    let mut acc = ChatResponseBuilder::new();
    if let Some(model) = body.get("model").and_then(|m| m.as_str()) {
        acc.apply(ChatEvent::started(model))?;
    }
    for ev in events.into_iter().chain(ends) {
        acc.apply(ev)?;
    }
    if let Some(end) = ChatEvent::stream_end(stop_reason, usage) {
        acc.apply(end)?;
    }
    Ok(acc.finish()?)
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
    // Part of `completion_tokens`, already billed as output.
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Usage {
        input_tokens: prompt,
        output_tokens: completion,
        cached_input_tokens: cached,
        cache_creation_tokens: 0,
        thinking_tokens: reasoning,
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_openai_usage_surfaces_reasoning_tokens() {
        let u = json!({
            "prompt_tokens": 22,
            "completion_tokens": 224,
            "completion_tokens_details": { "reasoning_tokens": 130 },
            "prompt_tokens_details": { "cached_tokens": 0 }
        });
        let usage = parse_openai_usage(u.as_object().unwrap());
        assert_eq!(
            usage.output_tokens, 224,
            "output stays the full completion (billed)"
        );
        assert_eq!(
            usage.thinking_tokens, 130,
            "reasoning surfaced, not hardcoded 0"
        );
    }

    #[test]
    fn parse_openai_usage_no_reasoning_is_zero() {
        let u = json!({ "prompt_tokens": 10, "completion_tokens": 5 });
        let usage = parse_openai_usage(u.as_object().unwrap());
        assert_eq!(usage.thinking_tokens, 0);
    }
}

#[cfg(test)]
mod raw_carry_tests {
    use super::super::dialect::StandardDialect;
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_batch_line_error_carries_raw() {
        let bogus = r#"{"custom_id": "abc", "response": OOPS_NOT_JSON}"#;
        let err = parse_openai_batch_results(&StandardDialect, bogus)
            .expect_err("malformed JSONL line must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("OOPS_NOT_JSON"),
            "error must carry the raw payload, got: {msg}"
        );
    }

    #[test]
    fn an_empty_custom_id_is_a_parse_error_carrying_raw() {
        let line = r#"{"custom_id": "", "error": {"message": "EMPTY_ID_MARKER"}}"#;
        let err = parse_openai_batch_results(&StandardDialect, line)
            .expect_err("an empty custom_id is no item id");
        assert!(
            matches!(&err, ProviderError::Parse { raw: Some(m), .. } if m.contains("EMPTY_ID_MARKER")),
            "{err:?}"
        );
    }

    #[test]
    fn batch_line_missing_response_carries_raw() {
        let line = r#"{"custom_id": "req-42", "surprise": "no response here"}"#;
        let err = parse_openai_batch_results(&StandardDialect, line)
            .expect_err("line without response/error must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("no response here"),
            "error must carry the raw payload, got: {msg}"
        );
    }

    #[test]
    fn chat_completion_missing_message_carries_raw() {
        let body = json!({
            "model": "gpt-4o",
            "choices": [{ "finish_reason": "stop", "sentinel_marker": "MARKER_42" }]
        });
        let err = openai_chat_completion_to_chat_response(&body, parse_openai_usage)
            .expect_err("choice without message must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("MARKER_42"),
            "error must carry the raw body, got: {msg}"
        );
    }

    #[test]
    fn raw_in_error_is_truncated() {
        let huge = format!("{{not json {}", "x".repeat(10_000));
        let err = parse_openai_batch_results(&StandardDialect, &huge)
            .expect_err("malformed line must be an error");
        let msg = err.to_string();
        assert!(
            msg.len() < huge.len(),
            "raw must be truncated, error len {} vs payload {}",
            msg.len(),
            huge.len()
        );
        assert!(
            msg.contains('…'),
            "truncation ellipsis expected, got: {msg}"
        );
    }
}

#[cfg(test)]
mod batch_error_file_tests {
    use super::*;
    use crate::provider::backends::openai::StandardDialect;
    use serde_json::json;

    #[test]
    fn a_line_of_the_error_file_becomes_that_items_error() {
        let line = r#"{"custom_id":"a","response":{"status_code":400,"body":{"error":{"message":"bad model","type":"invalid_request_error","code":null}}},"error":null}"#;
        let items = parse_openai_batch_results(&StandardDialect, line).unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            matches!(
                &items[0].result,
                Err(ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal { message, .. })))
                    if message == "bad model"
            ),
            "{:?}",
            items[0].result
        );
    }

    #[test]
    fn a_2xx_line_is_read_as_a_completion() {
        let line = r#"{"custom_id":"a","response":{"status_code":200,"body":{"model":"m","choices":[{"message":{"content":"hi"},"finish_reason":"stop"}]}},"error":null}"#;
        let items = parse_openai_batch_results(&StandardDialect, line).unwrap();
        assert_eq!(items[0].result.as_ref().unwrap().text, "hi");
    }

    #[test]
    fn a_completion_with_no_finish_reason_is_unfinished() {
        let body = json!({
            "model": "m",
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "prompt_tokens": 1000, "completion_tokens": 500 }
        });
        let r = openai_chat_completion_to_chat_response(&body, parse_openai_usage);
        assert!(
            matches!(
                r,
                Err(ProviderError::Unfinished(
                    tars_types::Unfinished::NoStopReason
                ))
            ),
            "{r:?}"
        );
    }

    #[test]
    fn a_finish_reason_the_mapping_does_not_know_is_other() {
        assert_eq!(map_finish_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_finish_reason("function_call"), StopReason::ToolUse);
        assert_eq!(map_finish_reason("something_new"), StopReason::Other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::openai::StandardDialect;

    fn only_item(line: &str) -> Result<ChatResponse, ProviderError> {
        let mut items = parse_openai_batch_results(&StandardDialect, line).unwrap();
        assert_eq!(items.len(), 1);
        items.remove(0).result
    }

    #[test]
    fn a_batch_that_reports_no_counts_has_no_progress_rather_than_zero() {
        let v = serde_json::json!({"status": "in_progress"});
        assert_eq!(
            translate_openai_batch_status(&v).unwrap(),
            BatchStatus::InProgress {
                processed: None,
                total: None,
                eta: None
            }
        );
    }

    #[test]
    fn a_failed_item_whose_body_has_no_error_object_carries_the_body() {
        let line =
            r#"{"custom_id":"a","response":{"status_code":502,"body":{"detail":"upstream gone"}}}"#;
        let err = only_item(line).unwrap_err();
        assert!(err.to_string().contains("upstream gone"), "{err}");
    }

    #[test]
    fn a_status_code_past_u16_is_a_parse_error_of_that_item() {
        let line = r#"{"custom_id":"a","response":{"status_code":70000,"body":{}}}"#;
        let err = only_item(line).unwrap_err();
        assert!(
            matches!(
                &err,
                ProviderError::Parse { problem, .. }
                    if matches!(**problem, ParseProblem::UnexpectedValue { field: "response.status_code", .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn an_item_error_naming_no_code_or_type_has_no_code() {
        let line = r#"{"custom_id":"a","error":{"message":"boom"}}"#;
        let err = only_item(line).unwrap_err();
        assert!(
            matches!(&err, ProviderError::BatchItemFailed { code: None, message } if message == "boom"),
            "{err:?}"
        );
    }

    #[test]
    fn a_completion_without_a_model_reports_no_model() {
        let body = serde_json::json!({
            "choices": [{ "message": { "content": "hi" }, "finish_reason": "stop" }]
        });
        let r = openai_chat_completion_to_chat_response(&body, parse_openai_usage).unwrap();
        assert_eq!(r.actual_model, None);
    }

    #[test]
    fn a_failed_batch_without_a_message_carries_the_batch_object() {
        let v = serde_json::json!({"status": "failed", "id": "batch_9"});
        match translate_openai_batch_status(&v).unwrap() {
            BatchStatus::Failed { message, .. } => {
                assert!(message.contains("batch_9"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }
}
