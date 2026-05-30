//! Protocol-translation layer for the Anthropic backend. Owns request
//! body construction (`translate_request`), SSE event parsing
//! (`parse_event`), error classification, and the `HttpAdapter` impl.
//! The provider lifecycle + batch-API I/O live in [`super::provider`];
//! the pure JSON converters live in [`super::mapping`].

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use url::Url;

use tars_types::{
    CacheDirective, ChatEvent, ChatRequest, ContentBlock, ImageData, Message, ProviderError,
    StopReason, Usage,
};

use crate::auth::ResolvedAuth;
use crate::http_base::{HttpAdapter, HttpProviderExtras, SseEvent};
use crate::tool_buffer::ToolCallBuffer;

use super::mapping::{map_stop_reason, parse_usage, truncate};

/// Synthetic tool name used to emulate structured output (Doc 01 §9).
pub(super) const STRUCTURED_OUTPUT_TOOL: &str = "__respond_with__";

pub struct AnthropicAdapter {
    pub(super) base_url: String,
    pub(super) api_version: String,
    pub(super) extras: HttpProviderExtras,
}

impl AnthropicAdapter {
    pub(super) fn new(base_url: String, api_version: String, extras: HttpProviderExtras) -> Self {
        Self {
            base_url,
            api_version,
            extras,
        }
    }

    /// Translate one of our content blocks into Anthropic's content shape.
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
            // Anthropic doesn't have a `tool` role — tool results are
            // user-role messages with `tool_result` content blocks.
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
            // Anthropic's `system` is top-level, not a message role.
            // If a System message arrives here it's typically because
            // a caller serialized a transcript verbatim — flatten it
            // into a user-role text block prefixed with "[system]" so
            // it isn't indistinguishable from a real user turn.
            Message::System { content } => {
                let mut blocks: Vec<Value> = content.iter().map(Self::translate_block).collect();
                blocks.insert(0, json!({"type": "text", "text": "[system]"}));
                json!({
                    "role": "user",
                    "content": Value::Array(blocks),
                })
            }
        }
    }

    /// Apply [`CacheDirective::MarkBoundary`] markers. Anthropic accepts
    /// up to 4 cache_control markers; we attach to system + last
    /// content block per directive (in order). The translation here is
    /// best-effort — callers wanting precise placement should construct
    /// messages with the markers already on specific blocks.
    fn apply_cache_directives(body: &mut Value, directives: &[CacheDirective]) {
        let want_marker = directives
            .iter()
            .any(|d| matches!(d, CacheDirective::MarkBoundary { .. }));
        if !want_marker {
            return;
        }
        // Add marker to the system prompt (cheapest cache placement).
        if let Some(system_blocks) = body.get_mut("system").and_then(|s| s.as_array_mut()) {
            if let Some(last) = system_blocks.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        // Add marker to the last block of the last *user* message
        // (covers RAG context use cases). If the conversation ends on
        // an assistant turn, attaching cache_control there would
        // cache assistant output instead of user-supplied context,
        // wasting the budget; walk back to the most recent user msg.
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            if let Some(last_user) = messages
                .iter_mut()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            {
                if let Some(blocks) = last_user.get_mut("content").and_then(|c| c.as_array_mut()) {
                    if let Some(last_block) = blocks.last_mut() {
                        last_block["cache_control"] = json!({"type": "ephemeral"});
                    }
                }
            }
        }
    }

    /// Build a `messages/batches` URL with the given suffix. Used by
    /// the `BatchSubmitter` impl on [`super::provider::AnthropicProvider`]
    /// — `""` is the collection (POST submit), `/{id}` is one job,
    /// `/{id}/results` and `/{id}/cancel` are sub-resources.
    pub(crate) fn batch_url(&self, suffix: &str) -> Result<Url, ProviderError> {
        Url::parse(&format!(
            "{}/v1/messages/batches{suffix}",
            self.base_url.trim_end_matches('/')
        ))
        .map_err(|e| ProviderError::Internal(format!("bad anthropic batch url: {e}")))
    }
}

#[async_trait]
impl HttpAdapter for AnthropicAdapter {
    fn build_url(&self, _model: &str) -> Result<Url, ProviderError> {
        Url::parse(&format!(
            "{}/v1/messages",
            self.base_url.trim_end_matches('/')
        ))
        .map_err(|e| ProviderError::Internal(format!("bad anthropic base_url: {e}")))
    }

    fn build_headers(&self, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            "anthropic-version",
            HeaderValue::from_str(&self.api_version)
                .map_err(|e| ProviderError::Internal(format!("bad version header: {e}")))?,
        );
        match auth {
            ResolvedAuth::ApiKey(k) | ResolvedAuth::Bearer(k) => {
                h.insert(
                    "x-api-key",
                    HeaderValue::from_str(k).map_err(|e| {
                        // Invalid header chars in the key are an auth-
                        // credential problem, not a backend bug.
                        ProviderError::Auth(format!("malformed x-api-key value: {e}"))
                    })?,
                );
            }
            ResolvedAuth::None => {
                return Err(ProviderError::Auth(
                    "Anthropic requires an x-api-key; got Auth::None".into(),
                ));
            }
        }
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest) -> Result<Value, ProviderError> {
        let model = req
            .model
            .explicit()
            .ok_or_else(|| ProviderError::InvalidRequest("model must be explicit".into()))?;

        if req.messages.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "anthropic: messages array must contain at least one message".into(),
            ));
        }

        // Anthropic rejects requests with duplicate tool names with a
        // 400; surface a clear error before the round-trip.
        let mut seen = std::collections::HashSet::new();
        for t in &req.tools {
            if !seen.insert(t.name.as_str()) {
                return Err(ProviderError::InvalidRequest(format!(
                    "anthropic: duplicate tool name `{}` in request",
                    t.name
                )));
            }
        }
        // Reserved synthetic tool name used for structured-output
        // emulation must not collide with a caller-supplied tool.
        if req.structured_output.is_some()
            && req.tools.iter().any(|t| t.name == STRUCTURED_OUTPUT_TOOL)
        {
            return Err(ProviderError::InvalidRequest(format!(
                "anthropic: tool name `{STRUCTURED_OUTPUT_TOOL}` is reserved for structured-output emulation"
            )));
        }

        let messages: Vec<Value> = req.messages.iter().map(Self::translate_message).collect();

        let max_tokens = req.max_output_tokens.unwrap_or(4096);
        if max_tokens == 0 {
            return Err(ProviderError::InvalidRequest(
                "anthropic: max_output_tokens must be > 0".into(),
            ));
        }

        let mut body = json!({
            "model": model,
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": true,
        });

        if let Some(sys) = &req.system {
            // Always emit `system` as an array of blocks so cache_control
            // can be attached uniformly.
            body["system"] = json!([{"type": "text", "text": sys}]);
        }

        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(req.stop_sequences);
        }

        // Tools.
        let mut tools_to_send: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema.schema,
                })
            })
            .collect();

        // Structured output emulation (Doc 01 §9): inject a hidden tool
        // and force its use.
        if let Some(schema) = &req.structured_output {
            tools_to_send.push(json!({
                "name": STRUCTURED_OUTPUT_TOOL,
                "description": "Return the response strictly conforming to the schema.",
                "input_schema": schema.schema,
            }));
            body["tool_choice"] = json!({
                "type": "tool",
                "name": STRUCTURED_OUTPUT_TOOL,
            });
        } else if !tools_to_send.is_empty() {
            // Apply caller's tool_choice only when not overridden by
            // structured output.
            body["tool_choice"] = match &req.tool_choice {
                tars_types::ToolChoice::Auto => json!({"type": "auto"}),
                tars_types::ToolChoice::None => json!({"type": "none"}),
                tars_types::ToolChoice::Required => json!({"type": "any"}),
                tars_types::ToolChoice::Specific(name) => {
                    if !req.tools.iter().any(|t| &t.name == name) {
                        return Err(ProviderError::InvalidRequest(format!(
                            "anthropic: tool_choice references unknown tool `{name}`"
                        )));
                    }
                    json!({"type": "tool", "name": name})
                }
            };
        }
        if !tools_to_send.is_empty() {
            body["tools"] = Value::Array(tools_to_send);
        }

        // Thinking.
        match req.thinking {
            tars_types::ThinkingMode::Off => {}
            tars_types::ThinkingMode::Auto => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": 4096});
            }
            tars_types::ThinkingMode::Budget(b) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": b});
            }
        }

        Self::apply_cache_directives(&mut body, &req.cache_directives);

        Ok(body)
    }

    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut ToolCallBuffer,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        if raw.data.is_empty() {
            return Ok(Vec::new());
        }
        // `ping` events carry no business payload.
        if raw.event == "ping" {
            return Ok(Vec::new());
        }
        if raw.event == "error" {
            // Provider-emitted error mid-stream (rare). Surface as ProviderError.
            let v: Value = serde_json::from_str(&raw.data).unwrap_or(Value::Null);
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("anthropic mid-stream error")
                .to_string();
            return Err(ProviderError::Internal(msg));
        }

        let v: Value = serde_json::from_str(&raw.data).map_err(|e| {
            ProviderError::Parse(format!(
                "anthropic sse: {e} (raw: {})",
                truncate(&raw.data, 200)
            ))
        })?;

        let mut out = Vec::new();
        match raw.event.as_str() {
            "message_start" => {
                let model = v
                    .pointer("/message/model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                let cache_hit = v
                    .pointer("/message/usage")
                    .and_then(|u| u.as_object())
                    .map(|u| tars_types::CacheHitInfo {
                        cached_input_tokens: u
                            .get("cache_read_input_tokens")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0),
                        used_explicit_handle: false,
                        replayed_from_cache: false,
                    })
                    .unwrap_or_default();
                out.push(ChatEvent::Started {
                    actual_model: model,
                    cache_hit,
                });
            }
            "content_block_start" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let cb = v.get("content_block").cloned().unwrap_or(Value::Null);
                match cb.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        let id = cb
                            .get("id")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = cb
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        out.push(ChatEvent::ToolCallStart {
                            index,
                            id: id.clone(),
                            name: name.clone(),
                        });
                        buf.on_start(index, id, name);
                    }
                    Some("text") => {
                        // Anthropic occasionally emits a `text` block with
                        // initial text already populated. Forward it.
                        if let Some(t) = cb.get("text").and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                out.push(ChatEvent::Delta {
                                    text: t.to_string(),
                                });
                            }
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = cb.get("thinking").and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                out.push(ChatEvent::ThinkingDelta {
                                    text: t.to_string(),
                                });
                            }
                        }
                    }
                    _ => {} // unknown block types silently ignored
                }
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(|s| s.as_str()) {
                            out.push(ChatEvent::Delta {
                                text: t.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        // Tool args fragment.
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
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                // Finalize any tool call at this index. Non-tool
                // content blocks (text / thinking) return an error
                // here because they were never registered with the
                // buffer; that's the expected path, log at trace.
                match buf.finalize(index) {
                    Ok((id, _name, parsed)) => {
                        out.push(ChatEvent::ToolCallEnd {
                            index,
                            id,
                            parsed_args: parsed,
                        });
                    }
                    Err(e) => {
                        tracing::trace!(
                            index,
                            error = %e,
                            "anthropic: content_block_stop finalize miss (likely text/thinking block)",
                        );
                    }
                }
            }
            "message_delta" => {
                // Carries `delta.stop_reason` and updated `usage`.
                let stop = v
                    .pointer("/delta/stop_reason")
                    .and_then(|s| s.as_str())
                    .map(map_stop_reason);
                let usage = v.get("usage").and_then(|u| u.as_object()).cloned();
                if let (Some(stop), Some(u)) = (stop, usage) {
                    out.push(ChatEvent::Finished {
                        stop_reason: stop,
                        usage: parse_usage(&u),
                    });
                    buf.mark_finished();
                }
            }
            // Authoritative end. message_delta may have failed to emit
            // Finished (missing stop_reason or usage in the delta
            // payload, mid-stream provider quirk), which would leave
            // consumers waiting forever. Emit a synthetic Finished as a
            // last resort.
            "message_stop" if !buf.finished_emitted() => {
                tracing::warn!(
                    "anthropic: message_stop without prior Finished; emitting synthetic terminator",
                );
                out.push(ChatEvent::Finished {
                    stop_reason: StopReason::Other,
                    usage: Usage::default(),
                });
                buf.mark_finished();
            }
            _ => {} // unknown events are tolerated
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
        let message = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| truncate(body, 300));

        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderError::Auth(message),
            StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
                retry_after: crate::http_base::parse_retry_after(headers),
            },
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
                ProviderError::ModelOverloaded
            }
            StatusCode::BAD_REQUEST => {
                let lower = message.to_lowercase();
                if lower.contains("max_tokens") || lower.contains("context") {
                    ProviderError::ContextTooLong {
                        limit: 0,
                        requested: 0,
                    }
                } else {
                    ProviderError::InvalidRequest(message)
                }
            }
            s if s.is_server_error() => ProviderError::Internal(format!("status {s}: {message}")),
            _ => ProviderError::InvalidRequest(format!("status {status}: {message}")),
        }
    }

    fn extras(&self) -> &HttpProviderExtras {
        &self.extras
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
    const DEFAULT_API_VERSION: &str = "2023-06-01";

    fn adapter() -> AnthropicAdapter {
        AnthropicAdapter::new(
            DEFAULT_BASE_URL.into(),
            DEFAULT_API_VERSION.into(),
            HttpProviderExtras::default(),
        )
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
        // Anthropic uses retry-after-ms (millisecond precision).
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
    fn translate_request_promotes_system_to_top_level() {
        let a = adapter();
        let req = ChatRequest::user(
            tars_types::ModelHint::Explicit("claude-opus-4-7".into()),
            "hello",
        )
        .with_system("you are concise");
        let body = a.translate_request(&req).unwrap();
        assert!(body["system"].is_array());
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "you are concise");
        assert_eq!(body["model"], "claude-opus-4-7");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn cache_marker_attaches_to_last_message_block() {
        let a = adapter();
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("claude-opus-4-7".into()),
            "context",
        )
        .with_system("sys");
        req.cache_directives.push(CacheDirective::MarkBoundary {
            ttl: std::time::Duration::from_secs(300),
        });
        let body = a.translate_request(&req).unwrap();
        // Last user content block carries cache_control.
        let last_block = &body["messages"][0]["content"][0];
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        // System block likewise.
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn structured_output_injects_forced_tool() {
        let a = adapter();
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("claude-opus-4-7".into()),
            "give json",
        );
        req.structured_output = Some(tars_types::JsonSchema::strict(
            "Resp",
            serde_json::json!({"type":"object"}),
        ));
        let body = a.translate_request(&req).unwrap();
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

    #[test]
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::StopSequence);
        assert_eq!(map_stop_reason("???"), StopReason::Other);
    }
}
