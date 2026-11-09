//! Translates requests into Anthropic's Messages body, its named stream events into
//! [`ChatEvent`]s, and its HTTP errors into [`ProviderError`]s.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use url::Url;

use crate::ProviderError;
use crate::pipeline::middleware::OutputReserve;
use crate::provider::error::{
    CredentialError, CredentialKind, ParseProblem, ProviderRefusal, RequestRefusal, status_error,
};
use tars_types::{CacheDirective, ChatEvent, ChatRequest, ContentBlock, ImageData, Message};

use crate::provider::auth::ResolvedAuth;
use crate::provider::http_base::{HttpAdapter, HttpOverrides, SseEvent, parse_url};
use crate::provider::tool_buffer::{StreamDecodeState, StreamEnd};

use super::mapping::{map_stop_reason, parse_usage, truncate};

/// Names the schema-only tool a structured-output request is forced to call; a caller's tool
/// may not take it.
pub(super) const STRUCTURED_OUTPUT_TOOL: &str = "__respond_with__";

/// Returns an event's content-block `index`.
///
/// # Errors
///
/// Fails with `Parse` when it is missing or does not fit `usize`, rather than land in the wrong
/// slot and merge two blocks.
fn block_index(v: &Value) -> Result<usize, ProviderError> {
    let parse_error = |problem| ProviderError::Parse {
        at: "anthropic sse",
        line: None,
        problem: Box::new(problem),
        raw: Some(truncate(&v.to_string(), 200)),
    };
    let raw = v
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| parse_error(ParseProblem::Missing("index")))?;
    usize::try_from(raw).map_err(|_| parse_error(ParseProblem::IndexOverflow(raw)))
}

pub struct AnthropicAdapter {
    pub(super) base_url: String,
    pub(super) api_version: String,
    pub(super) extras: HttpOverrides,
    /// Is sent as `max_tokens`, which the API requires, for a request that sets no limit.
    output_reserve: OutputReserve,
}

impl AnthropicAdapter {
    pub(super) fn new(
        base_url: String,
        api_version: String,
        extras: HttpOverrides,
        output_reserve: OutputReserve,
    ) -> Self {
        Self {
            base_url,
            api_version,
            extras,
            output_reserve,
        }
    }

    fn translate_block(b: &ContentBlock) -> Value {
        match b {
            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
            ContentBlock::Image { mime, data } => {
                let source = match data {
                    ImageData::Url(u) => json!({"type": "url", "url": u}),
                    ImageData::Base64(b) => json!({
                        "type": "base64",
                        "media_type": mime,
                        "data": b,
                    }),
                };
                json!({"type": "image", "source": source})
            }
        }
    }

    fn translate_content(blocks: &[ContentBlock]) -> Value {
        Value::Array(blocks.iter().map(Self::translate_block).collect())
    }

    fn translate_message(m: &Message) -> Value {
        match m {
            Message::User { content } => json!({
                "role": "user",
                "content": Self::translate_content(content),
            }),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks: Vec<Value> = content.iter().map(Self::translate_block).collect();
                for tc in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                    }));
                }
                json!({"role": "assistant", "content": blocks})
            }
            // Anthropic has no tool role: a tool result is a user message.
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                let mut result_block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": Self::translate_content(content),
                });
                if *is_error {
                    result_block["is_error"] = json!(true);
                }
                json!({
                    "role": "user",
                    "content": [result_block],
                })
            }
            // Anthropic has no system role either: an inline system message becomes a user
            // message marked "[system]", so it does not read as the user speaking.
            Message::System { content } => {
                let mut blocks: Vec<Value> = content.iter().map(Self::translate_block).collect();
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": "[system] (empty)"}));
                } else {
                    blocks.insert(0, json!({"type": "text", "text": "[system]"}));
                }
                json!({
                    "role": "user",
                    "content": Value::Array(blocks),
                })
            }
        }
    }

    /// Marks, when any directive is `MarkBoundary`, the last system block and the last block of
    /// the last user message as `ephemeral` cache points, with the longest TTL asked for
    /// ([`cache_control`]). However many directives there are, at most these two markers are set.
    fn apply_cache_directives(body: &mut Value, directives: &[CacheDirective]) {
        let longest = directives
            .iter()
            .filter_map(|d| match d {
                CacheDirective::MarkBoundary { ttl } => Some(*ttl),
                CacheDirective::Auto | CacheDirective::UseExplicit { .. } => None,
            })
            .max();
        let Some(ttl) = longest else {
            return;
        };
        let marker = cache_control(ttl);
        if let Some(system_blocks) = body.get_mut("system").and_then(|s| s.as_array_mut())
            && let Some(last) = system_blocks.last_mut()
        {
            last["cache_control"] = marker.clone();
        }
        // Not the last message: after an assistant turn that would cache the model's output
        // instead of the user's context.
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut())
            && let Some(last_user) = messages
                .iter_mut()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            && let Some(blocks) = last_user.get_mut("content").and_then(|c| c.as_array_mut())
            && let Some(last_block) = blocks.last_mut()
        {
            last_block["cache_control"] = marker;
        }
    }

    /// Returns `{base_url}/v1/messages/batches{suffix}` with the configured `query_params`:
    /// `""` for the collection, `/{id}` for one job, `/{id}/results` and `/{id}/cancel` below it.
    pub(crate) fn batch_url(&self, suffix: &str) -> Result<Url, ProviderError> {
        let mut url = parse_url(format!(
            "{}/v1/messages/batches{suffix}",
            self.base_url.trim_end_matches('/')
        ))?;
        self.extras.apply_query_params(&mut url);
        Ok(url)
    }
}

#[async_trait]
impl HttpAdapter for AnthropicAdapter {
    fn build_url(&self, _model: &str) -> Result<Url, ProviderError> {
        parse_url(format!(
            "{}/v1/messages",
            self.base_url.trim_end_matches('/')
        ))
    }

    fn build_headers(&self, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            "anthropic-version",
            HeaderValue::from_str(&self.api_version).map_err(|source| {
                ProviderError::InvalidHeader {
                    header: "anthropic-version",
                    source,
                }
            })?,
        );
        match auth {
            ResolvedAuth::ApiKey(k) | ResolvedAuth::Bearer(k) => {
                h.insert(
                    "x-api-key",
                    HeaderValue::from_str(k).map_err(|source| {
                        ProviderError::Auth(CredentialError::Header {
                            header: "x-api-key",
                            source,
                        })
                    })?,
                );
            }
            ResolvedAuth::None => {
                return Err(ProviderError::Auth(CredentialError::Kind {
                    expected: CredentialKind::ApiKey,
                    got: CredentialKind::None,
                }));
            }
        }
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest, model: &str) -> Result<Value, ProviderError> {
        if req.messages.is_empty() {
            return Err(ProviderError::InvalidRequest(RequestRefusal::EmptyMessages));
        }

        let max_tokens = req
            .max_output_tokens
            .unwrap_or(self.output_reserve.output_tokens.get());
        if max_tokens == 0 {
            return Err(ProviderError::InvalidRequest(
                RequestRefusal::ZeroMaxOutputTokens,
            ));
        }

        // Anthropic answers duplicate tool names with a bare 400.
        let mut seen = std::collections::HashSet::new();
        for t in &req.tools {
            if !seen.insert(t.name.as_str()) {
                return Err(ProviderError::InvalidRequest(
                    RequestRefusal::DuplicateToolName {
                        name: t.name.clone(),
                    },
                ));
            }
        }
        if req.structured_output.is_some()
            && req.tools.iter().any(|t| t.name == STRUCTURED_OUTPUT_TOOL)
        {
            return Err(ProviderError::InvalidRequest(
                RequestRefusal::ReservedToolName {
                    name: STRUCTURED_OUTPUT_TOOL.to_string(),
                },
            ));
        }

        let messages: Vec<Value> = req.messages.iter().map(Self::translate_message).collect();

        let mut body = json!({
            "model": model,
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": true,
        });

        if let Some(sys) = &req.system {
            // Blocks, so a cache marker can go on one.
            body["system"] = json!([{"type": "text", "text": sys}]);
        }

        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(req.stop_sequences);
        }

        let mut tools_to_send: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema.schema(),
                })
            })
            .collect();

        if let Some(schema) = &req.structured_output {
            tools_to_send.push(json!({
                "name": STRUCTURED_OUTPUT_TOOL,
                "description": "Return the response strictly conforming to the schema.",
                "input_schema": schema.schema(),
            }));
            body["tool_choice"] = json!({
                "type": "tool",
                "name": STRUCTURED_OUTPUT_TOOL,
            });
        } else if !tools_to_send.is_empty() {
            // Structured output overrides the caller's tool choice.
            body["tool_choice"] = match &req.tool_choice {
                tars_types::ToolChoice::Auto => json!({"type": "auto"}),
                tars_types::ToolChoice::None => json!({"type": "none"}),
                tars_types::ToolChoice::Required => json!({"type": "any"}),
                tars_types::ToolChoice::Specific(name) => {
                    if !req.tools.iter().any(|t| &t.name == name) {
                        return Err(ProviderError::InvalidRequest(
                            RequestRefusal::UnknownToolChoice { name: name.clone() },
                        ));
                    }
                    json!({"type": "tool", "name": name})
                }
            };
        }
        if !tools_to_send.is_empty() {
            body["tools"] = Value::Array(tools_to_send);
        }

        match req.thinking {
            None => {}
            Some(tars_types::ThinkingMode::Off) => {
                body["thinking"] = json!({"type": "disabled"});
            }
            Some(tars_types::ThinkingMode::Auto) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": 4096});
            }
            Some(tars_types::ThinkingMode::Budget(b)) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": b});
            }
        }

        Self::apply_cache_directives(&mut body, &req.cache_directives);

        Ok(body)
    }

    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut StreamDecodeState,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        if raw.data.is_empty() {
            return Ok(Vec::new());
        }
        if raw.event == "ping" {
            return Ok(Vec::new());
        }
        if raw.event == "error" {
            return Err(stream_error_event(&raw.data));
        }

        let v: Value = serde_json::from_str(&raw.data).map_err(|e| ProviderError::Parse {
            at: "anthropic sse",
            line: None,
            problem: Box::new(ParseProblem::Json(e)),
            raw: Some(truncate(&raw.data, 200)),
        })?;

        let mut out = Vec::new();
        match raw.event.as_str() {
            "message_start" => {
                let model = v
                    .pointer("/message/model")
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                let cache_hit = v
                    .pointer("/message/usage")
                    .and_then(|u| u.as_object())
                    .map(|u| tars_types::CacheHitInfo {
                        cached_input_tokens: u
                            .get("cache_read_input_tokens")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0),
                        used_explicit_handle: false,
                    })
                    .unwrap_or_default();
                out.push(ChatEvent::Started {
                    actual_model: model,
                    cache_hit,
                });
            }
            "content_block_start" => {
                let index = block_index(&v)?;
                let cb = v
                    .get("content_block")
                    .ok_or_else(|| event_without("content_block", &raw.data))?;
                match cb.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        // Without them the tool result cannot be routed back.
                        let id = cb
                            .get("id")
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| tool_use_without("id"))?
                            .to_string();
                        let name = cb
                            .get("name")
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| tool_use_without("name"))?
                            .to_string();
                        out.push(ChatEvent::ToolCallStart {
                            index,
                            id: id.clone(),
                            name: name.clone(),
                        });
                        buf.on_start(index, id, name);
                    }
                    Some("text") => {
                        // A text block can start with text already in it.
                        if let Some(t) = cb.get("text").and_then(|s| s.as_str())
                            && !t.is_empty()
                        {
                            out.push(ChatEvent::Delta {
                                text: t.to_string(),
                            });
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = cb.get("thinking").and_then(|s| s.as_str())
                            && !t.is_empty()
                        {
                            out.push(ChatEvent::ThinkingDelta {
                                text: t.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = block_index(&v)?;
                let delta = v
                    .get("delta")
                    .ok_or_else(|| event_without("delta", &raw.data))?;
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(|s| s.as_str()) {
                            out.push(ChatEvent::Delta {
                                text: t.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(p) = delta.get("partial_json").and_then(|s| s.as_str()) {
                            out.push(ChatEvent::ToolCallArgsDelta {
                                index,
                                args_delta: p.to_string(),
                            });
                            buf.on_delta(index, p);
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = delta.get("thinking").and_then(|s| s.as_str()) {
                            out.push(ChatEvent::ThinkingDelta {
                                text: t.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = block_index(&v)?;
                // Text and thinking blocks were never in the buffer. A tool call whose arguments do
                // not parse is held: the stop reason, which says whether the output was cut off,
                // normally comes in the `message_delta` after the block closes.
                if buf.is_inflight(index)
                    && let Some(call) = buf.finalize_or_hold(index, None)?
                {
                    out.push(ChatEvent::ToolCallEnd {
                        index,
                        id: call.id,
                        parsed_args: call.arguments,
                        thought_signature: call.thought_signature,
                    });
                }
            }
            "message_delta" => {
                let stop = v
                    .pointer("/delta/stop_reason")
                    .and_then(|s| s.as_str())
                    .map(map_stop_reason);
                // A delta without a stop reason (usage only) leaves the held call, and the calls
                // in flight, for the one that has it. Those calls end here, before `Finished`.
                if let Some(stop) = stop {
                    if let Some(call) = buf.take_held() {
                        return Err(call.into_error(Some(stop)));
                    }
                    out.extend(buf.end_inflight(stop)?);
                }
                let usage = v.get("usage").and_then(|u| u.as_object()).cloned();
                // Kept, so a `message_stop` that has to end the stream itself reports them.
                buf.record_finish_state(stop, usage.as_ref().map(parse_usage));
                if let (Some(stop), Some(u)) = (stop, usage) {
                    out.push(ChatEvent::Finished {
                        stop_reason: stop,
                        usage: Some(parse_usage(&u)),
                    });
                    buf.mark_finished();
                }
            }
            "message_stop" => {
                out.extend(buf.end_open_calls(StreamEnd::Terminator)?);
                // A `message_delta` with a stop reason but no usage sent no `Finished`; it is sent
                // here, with the usage the deltas carried. A stream that never said why the turn
                // stopped ends without one, so it is not taken as finished, and the usage it
                // reported goes out as `UsageProgress`.
                if !buf.finished_emitted() {
                    match buf.recorded_end() {
                        Some(finished @ ChatEvent::Finished { .. }) => {
                            out.push(finished);
                            buf.mark_finished();
                        }
                        usage => {
                            tracing::warn!(
                                "anthropic: message_stop with no stop reason in the stream; ending without Finished",
                            );
                            out.extend(usage);
                        }
                    }
                }
            }
            _ => {}
        }

        Ok(out)
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> ProviderError {
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let error_type = v.pointer("/error/type").and_then(Value::as_str);
        let message = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| truncate(body, 300));
        response_error(
            status.as_u16(),
            error_type,
            message,
            crate::provider::http_base::parse_retry_after(headers),
        )
        .stated(crate::provider::http_base::stated_by_headers(headers))
    }

    fn extras(&self) -> &HttpOverrides {
        &self.extras
    }
}

/// Returns the error of an Anthropic error response from its status and its `error.type`;
/// `retry_after` is its `Retry-After`. A 500 whose type is `timeout_error` is the request
/// reaching Anthropic's time limit, not a server failure.
fn response_error(
    status: u16,
    error_type: Option<&str>,
    message: String,
    retry_after: Option<Duration>,
) -> ProviderError {
    match (status, error_type) {
        (529, _) => ProviderError::ModelOverloaded,
        (500, Some("timeout_error")) => ProviderError::ProviderTimedOut(message),
        (400, _) => bad_request(message),
        _ => status_error(status, message, retry_after),
    }
}

/// Returns the error of a 400: the context window when the message says so, else the request.
fn bad_request(message: String) -> ProviderError {
    let lower = message.to_lowercase();
    // Anthropic says "prompt is too long: N tokens > M maximum"; `max_tokens` alone also appears
    // in schema errors ("max_tokens_to_sample: field required").
    let is_overflow = lower.contains("prompt is too long")
        || lower.contains("too many tokens")
        || (lower.contains("context") && lower.contains("token"));
    if is_overflow {
        let (requested, limit) = overflow_counts(&message).unzip();
        ProviderError::ContextTooLong {
            limit,
            requested,
            message,
        }
    } else {
        ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal {
            status: Some(400),
            message,
        }))
    }
}

/// Returns the error of a `tool_use` content block that lacks `field` or has it empty: without
/// it the tool result cannot be routed back.
/// Returns the error of a content-block event that lacks `field`, carrying the event's text.
fn event_without(field: &'static str, data: &str) -> ProviderError {
    ProviderError::Parse {
        at: "anthropic sse",
        line: None,
        problem: Box::new(ParseProblem::Missing(field)),
        raw: Some(truncate(data, 200)),
    }
}

fn tool_use_without(field: &'static str) -> ProviderError {
    ProviderError::Parse {
        at: "anthropic content_block_start tool_use",
        line: None,
        problem: Box::new(ParseProblem::Missing(field)),
        raw: None,
    }
}

/// Returns the error of an `error` event in an open stream, by its `error.type`. An event that
/// does not parse, or names a type this build does not know, is a `Parse` error carrying its text.
fn stream_error_event(data: &str) -> ProviderError {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            return ProviderError::Parse {
                at: "anthropic sse error event",
                line: None,
                problem: Box::new(ParseProblem::Json(e)),
                raw: Some(truncate(data, 200)),
            };
        }
    };
    let message = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map_or_else(|| truncate(data, 200), str::to_string);
    match v.pointer("/error/type").and_then(Value::as_str) {
        Some(error_type) => stream_error(error_type, message),
        None => ProviderError::Parse {
            at: "anthropic sse error event",
            line: None,
            problem: Box::new(ParseProblem::Missing("error.type")),
            raw: Some(truncate(data, 200)),
        },
    }
}

/// Returns the error an `error.type` names in an open stream, which has no HTTP status.
fn stream_error(error_type: &str, message: String) -> ProviderError {
    error_of_type(error_type, message).unwrap_or_else(|message| ProviderError::Parse {
        at: "anthropic sse error event",
        line: None,
        problem: Box::new(ParseProblem::UnexpectedValue {
            field: "error.type",
            value: error_type.to_string(),
        }),
        raw: Some(message),
    })
}

/// Returns the error an Anthropic `error.type` names where no HTTP status comes with it (an
/// error event in an open stream, an errored batch item), or the message back for a type this
/// build does not know.
pub(super) fn error_of_type(error_type: &str, message: String) -> Result<ProviderError, String> {
    Ok(match error_type {
        "overloaded_error" => ProviderError::ModelOverloaded,
        "api_error" => ProviderError::ServerError {
            status: None,
            message,
        },
        "timeout_error" => ProviderError::ProviderTimedOut(message),
        "rate_limit_error" => ProviderError::RateLimited { retry_after: None },
        "authentication_error" | "permission_error" => {
            ProviderError::Auth(CredentialError::Refused(ProviderRefusal {
                status: None,
                message,
            }))
        }
        "invalid_request_error" | "not_found_error" | "request_too_large" => {
            ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal {
                status: None,
                message,
            }))
        }
        _ => return Err(message),
    })
}

/// Returns the `cache_control` marker for a cache point asked to live `ttl`. Anthropic keeps an
/// entry 5 minutes or 1 hour: up to 5 minutes is its default and sends no `ttl`, and anything
/// longer asks for the hour, the most it keeps.
fn cache_control(ttl: Duration) -> Value {
    if ttl <= Duration::from_secs(5 * 60) {
        json!({"type": "ephemeral"})
    } else {
        json!({"type": "ephemeral", "ttl": "1h"})
    }
}

/// Returns the requested and maximum token counts of "prompt is too long: N tokens > M maximum".
/// Any other wording, such as "input length and `max_tokens` exceed context limit: I + T > L",
/// gives neither: its numbers mean something else.
fn overflow_counts(message: &str) -> Option<(u32, u32)> {
    let (before, after) = message.split_once(" tokens > ")?;
    let requested = before.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    let (limit, rest) = after.split_once(' ')?;
    if !rest.starts_with("maximum") {
        return None;
    }
    Some((requested.parse().ok()?, limit.parse().ok()?))
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    #[test]
    fn a_cache_point_past_five_minutes_asks_for_the_hour() {
        assert_eq!(
            cache_control(Duration::from_secs(300)),
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            cache_control(Duration::from_secs(301)),
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn overflow_counts_are_read_only_from_the_tokens_over_maximum_wording() {
        assert_eq!(
            overflow_counts("prompt is too long: 210000 tokens > 200000 maximum"),
            Some((210_000, 200_000))
        );
        assert_eq!(overflow_counts("prompt is too long"), None);
        assert_eq!(overflow_counts("prompt is too long: 5 tokens"), None);
        assert_eq!(
            overflow_counts(
                "input length and `max_tokens` exceed context limit: 197202 + 21333 > 200000, \
                 decrease input length or `max_tokens` and try again"
            ),
            None
        );
    }

    #[test]
    fn an_input_plus_max_tokens_overflow_is_context_too_long_without_counts() {
        let a = AnthropicAdapter::new(
            "https://api.anthropic.com".into(),
            "2023-06-01".into(),
            HttpOverrides::default(),
            OutputReserve::default(),
        );
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"input length and `max_tokens` exceed context limit: 197202 + 21333 > 200000, decrease input length or `max_tokens` and try again"}}"#;
        let err = a.classify_error(
            StatusCode::BAD_REQUEST,
            &reqwest::header::HeaderMap::new(),
            body,
        );
        assert!(
            matches!(
                err,
                ProviderError::ContextTooLong {
                    limit: None,
                    requested: None,
                    ..
                }
            ),
            "{err:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::StopReason;

    const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
    const DEFAULT_API_VERSION: &str = "2023-06-01";

    fn adapter() -> AnthropicAdapter {
        AnthropicAdapter::new(
            DEFAULT_BASE_URL.into(),
            DEFAULT_API_VERSION.into(),
            HttpOverrides::default(),
            OutputReserve::default(),
        )
    }

    #[test]
    fn a_usage_only_delta_reaches_the_finished_that_message_stop_sends() {
        let a = adapter();
        let mut buf = StreamDecodeState::new();
        let ev = |event: &str, data: &str| SseEvent {
            event: event.into(),
            data: data.into(),
        };
        for (event, data) in [
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"output_tokens":9}}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
        ] {
            a.parse_event(&ev(event, data), &mut buf).unwrap();
        }
        let out = a
            .parse_event(&ev("message_stop", r#"{"type":"message_stop"}"#), &mut buf)
            .unwrap();
        match out.as_slice() {
            [ChatEvent::Finished { stop_reason, usage }] => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(usage.unwrap().output_tokens, 9);
            }
            other => panic!("expected one Finished, got {other:?}"),
        }
    }

    fn events_of(
        a: &AnthropicAdapter,
        events: &[(&str, &str)],
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        let mut buf = StreamDecodeState::new();
        let mut out = Vec::new();
        for (event, data) in events {
            let raw = SseEvent {
                event: (*event).into(),
                data: (*data).into(),
            };
            out.extend(a.parse_event(&raw, &mut buf)?);
        }
        Ok(out)
    }

    const TOOL_START: (&str, &str) = (
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c1","name":"write_file"}}"#,
    );
    const MESSAGE_STOP: (&str, &str) = ("message_stop", r#"{"type":"message_stop"}"#);

    #[test]
    fn usage_without_a_stop_reason_reaches_the_consumer_without_finished() {
        let out = events_of(
            &adapter(),
            &[
                (
                    "message_delta",
                    r#"{"type":"message_delta","usage":{"output_tokens":5}}"#,
                ),
                MESSAGE_STOP,
            ],
        )
        .unwrap();
        match out.as_slice() {
            [ChatEvent::UsageProgress { usage }] => assert_eq!(usage.output_tokens, 5),
            other => panic!("expected only the usage, got {other:?}"),
        }
    }

    #[test]
    fn an_open_call_with_no_arguments_at_max_tokens_is_a_truncation() {
        let max_tokens = (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
        );
        assert!(matches!(
            events_of(&adapter(), &[TOOL_START, max_tokens]),
            Err(ProviderError::TruncatedToolCall { ref raw, .. }) if raw.is_empty()
        ));
    }

    #[test]
    fn an_open_call_ends_before_the_finished_its_stop_reason_sends() {
        let tool_use = (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
        );
        let out = events_of(&adapter(), &[TOOL_START, tool_use, MESSAGE_STOP]).unwrap();
        match out.as_slice() {
            [
                ChatEvent::ToolCallStart { .. },
                ChatEvent::ToolCallEnd { parsed_args, .. },
                ChatEvent::Finished {
                    stop_reason: StopReason::ToolUse,
                    ..
                },
            ] => assert!(parsed_args.is_empty()),
            other => panic!("expected the call to end before Finished, got {other:?}"),
        }
    }

    fn kind_of(status: u16, error_type: Option<&str>) -> tars_types::LlmErrorKind {
        response_error(status, error_type, "m".into(), None).kind()
    }

    #[test]
    fn a_500_is_a_server_error_unless_its_type_says_the_request_timed_out() {
        use tars_types::LlmErrorKind as K;
        assert_eq!(kind_of(500, Some("api_error")), K::ServerError);
        assert_eq!(kind_of(500, None), K::ServerError);
        assert_eq!(kind_of(500, Some("timeout_error")), K::ProviderTimedOut);
        assert_eq!(kind_of(504, Some("timeout_error")), K::ServerError);
        assert_eq!(kind_of(502, None), K::ServerError);
    }

    #[test]
    fn an_overload_is_its_own_error_and_not_a_server_error() {
        use tars_types::LlmErrorKind as K;
        assert_eq!(kind_of(529, Some("overloaded_error")), K::ModelOverloaded);
        assert_eq!(kind_of(503, None), K::ModelOverloaded);
    }

    #[test]
    fn a_408_is_a_request_timeout_and_a_409_an_invalid_request() {
        use tars_types::LlmErrorKind as K;
        assert_eq!(kind_of(408, None), K::RequestTimeout);
        assert_eq!(kind_of(409, None), K::InvalidRequest);
    }

    #[test]
    fn a_stream_error_event_is_classified_by_its_type() {
        use tars_types::LlmErrorKind as K;
        let kind = |t| stream_error(t, "m".into()).kind();
        assert_eq!(kind("overloaded_error"), K::ModelOverloaded);
        assert_eq!(kind("api_error"), K::ServerError);
        assert_eq!(kind("timeout_error"), K::ProviderTimedOut);
        assert_eq!(kind("rate_limit_error"), K::RateLimited);
        assert_eq!(kind("authentication_error"), K::Auth);
        assert_eq!(kind("invalid_request_error"), K::InvalidRequest);
        assert_eq!(kind("a_type_from_next_year"), K::Parse);
    }

    #[test]
    fn a_stream_api_error_is_a_transient_server_error_without_a_status() {
        let e = stream_error_event(
            r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#,
        );
        match &e {
            ProviderError::ServerError { status, message } => {
                assert_eq!((*status, message.as_str()), (None, "Internal server error"));
            }
            other => panic!("expected a server error, got {other:?}"),
        }
        assert_eq!(e.class(), tars_types::ErrorClass::Transient);
    }

    #[test]
    fn a_stream_error_event_that_does_not_parse_carries_its_text() {
        let e = stream_error_event("{not json");
        assert!(
            matches!(&e, ProviderError::Parse { raw: Some(r), .. } if r.contains("{not json")),
            "{e:?}"
        );
    }

    #[test]
    fn x_should_retry_decides_the_class_over_the_status() {
        use tars_types::ErrorClass;
        let a = adapter();
        let classify = |status, should_retry: Option<&str>| {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Some(v) = should_retry {
                headers.insert("x-should-retry", v.parse().unwrap());
            }
            a.classify_error(status, &headers, r#"{"error":{"type":"x","message":"m"}}"#)
                .class()
        };
        assert_eq!(classify(StatusCode::CONFLICT, None), ErrorClass::Permanent);
        assert_eq!(
            classify(StatusCode::CONFLICT, Some("true")),
            ErrorClass::Transient
        );
        assert_eq!(
            classify(StatusCode::INTERNAL_SERVER_ERROR, None),
            ErrorClass::Transient
        );
        assert_eq!(
            classify(StatusCode::INTERNAL_SERVER_ERROR, Some("false")),
            ErrorClass::Permanent
        );
        assert_eq!(
            classify(StatusCode::INTERNAL_SERVER_ERROR, Some("true")),
            ErrorClass::Transient
        );
    }

    #[test]
    fn classify_401_is_auth() {
        let a = adapter();
        let err = a.classify_error(
            StatusCode::UNAUTHORIZED,
            &reqwest::header::HeaderMap::new(),
            r#"{"error":{"message":"invalid"}}"#,
        );
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn classify_429_with_retry_after_ms_populates_field() {
        let a = adapter();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after-ms", "1500".parse().unwrap());
        let err = a.classify_error(StatusCode::TOO_MANY_REQUESTS, &headers, "");
        match err {
            ProviderError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(std::time::Duration::from_millis(1500)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn thinking_off_is_sent_as_disabled_and_an_unset_thinking_is_not_sent() {
        let a = adapter();
        let mut req = ChatRequest::user("hi");
        let unset = a.translate_request(&req, "claude-opus-4-7").unwrap();
        assert!(unset.get("thinking").is_none(), "{unset}");

        req.thinking = Some(tars_types::ThinkingMode::Off);
        let off = a.translate_request(&req, "claude-opus-4-7").unwrap();
        assert_eq!(off["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn translate_request_promotes_system_to_top_level() {
        let a = adapter();
        let req = ChatRequest::user("hello").with_system("you are concise");
        let body = a.translate_request(&req, "claude-opus-4-7").unwrap();
        assert!(body["system"].is_array());
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "you are concise");
        assert_eq!(body["model"], "claude-opus-4-7");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn cache_marker_attaches_to_last_message_block() {
        let a = adapter();
        let mut req = ChatRequest::user("context").with_system("sys");
        req.cache_directives.push(CacheDirective::MarkBoundary {
            ttl: std::time::Duration::from_secs(300),
        });
        let body = a.translate_request(&req, "claude-opus-4-7").unwrap();
        let last_block = &body["messages"][0]["content"][0];
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn structured_output_injects_forced_tool() {
        let a = adapter();
        let mut req = ChatRequest::user("give json");
        req.structured_output = Some(tars_types::JsonSchema::strict(
            "Resp",
            tars_types::json_object! {"type":"object"},
        ));
        let body = a.translate_request(&req, "claude-opus-4-7").unwrap();
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], STRUCTURED_OUTPUT_TOOL);
        let tools = body["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == STRUCTURED_OUTPUT_TOOL));
    }

    #[test]
    fn build_headers_requires_api_key() {
        let a = adapter();
        let err = a.build_headers(&ResolvedAuth::None).unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)));

        let h = a
            .build_headers(&ResolvedAuth::ApiKey("sk-ant-x".into()))
            .unwrap();
        assert_eq!(h.get("x-api-key").unwrap(), "sk-ant-x");
        assert_eq!(h.get("anthropic-version").unwrap(), DEFAULT_API_VERSION);
    }

    fn missing_field(result: Result<Vec<ChatEvent>, ProviderError>) -> Option<&'static str> {
        match result {
            Err(ProviderError::Parse { problem, raw, .. }) if raw.is_some() => match *problem {
                ParseProblem::Missing(field) => Some(field),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn a_content_block_event_without_an_index_is_a_parse_error_not_slot_zero() {
        let start = (
            "content_block_start",
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"c1","name":"f"}}"#,
        );
        assert_eq!(
            missing_field(events_of(&adapter(), &[start])),
            Some("index")
        );
    }

    #[test]
    fn a_content_block_start_without_its_block_is_a_parse_error() {
        let start = (
            "content_block_start",
            r#"{"type":"content_block_start","index":0}"#,
        );
        assert_eq!(
            missing_field(events_of(&adapter(), &[start])),
            Some("content_block")
        );
    }

    #[test]
    fn a_content_block_delta_without_its_delta_is_a_parse_error() {
        let delta = (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0}"#,
        );
        assert_eq!(
            missing_field(events_of(&adapter(), &[TOOL_START, delta])),
            Some("delta")
        );
    }

    #[test]
    fn a_message_start_without_a_model_reports_no_model() {
        let start = (
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":3}}}"#,
        );
        match events_of(&adapter(), &[start]).unwrap().as_slice() {
            [ChatEvent::Started { actual_model, .. }] => assert_eq!(*actual_model, None),
            other => panic!("expected one Started, got {other:?}"),
        }
    }

    #[test]
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::StopSequence);
        assert_eq!(map_stop_reason("???"), StopReason::Other);
    }

    #[test]
    fn a_request_without_a_limit_sends_the_output_reserve_and_one_with_a_limit_sends_it() {
        let reserve = OutputReserve {
            output_tokens: std::num::NonZeroU32::new(1_000).expect("1000 is not zero"),
        };
        let a = AnthropicAdapter::new(
            DEFAULT_BASE_URL.into(),
            DEFAULT_API_VERSION.into(),
            HttpOverrides::default(),
            reserve,
        );
        let mut req = ChatRequest::user("hi");
        let unset = a.translate_request(&req, "claude-opus-4-7").unwrap();
        assert_eq!(unset["max_tokens"], json!(1_000));

        req.max_output_tokens = Some(77);
        let set = a.translate_request(&req, "claude-opus-4-7").unwrap();
        assert_eq!(set["max_tokens"], json!(77));
    }
}
