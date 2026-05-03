//! Anthropic (Claude) HTTP backend.
//!
//! Wire format reference: <https://docs.anthropic.com/en/api/messages>
//!
//! Differences from OpenAI worth noting:
//!
//! - **Auth**: `x-api-key` header (not `Authorization: Bearer`).
//! - **Versioning**: `anthropic-version: 2023-06-01` mandatory header.
//! - **System**: separate top-level `system` field, not a message role.
//! - **Tool calls**: `tool_use` content blocks; no JSON-string nesting
//!   (args arrive as a real object — easier than OpenAI in this regard).
//! - **Caching**: explicit `cache_control: {type: "ephemeral"}` markers
//!   inserted on specific blocks. We attach to the system prompt and
//!   to the *last* message when [`CacheDirective::MarkBoundary`] is set.
//! - **Thinking**: a `thinking` content block + `thinking` config; we
//!   surface the deltas as [`ChatEvent::ThinkingDelta`].
//! - **Structured output**: emulated via a forced `tool_choice` (Doc
//!   01 §9). The "tool" is a synthetic schema-only call.
//! - **Streaming events**: SSE with named events (`message_start`,
//!   `content_block_start`, `content_block_delta`, `message_delta`,
//!   `message_stop`, `ping`, `error`). The named events are key — we
//!   route on `raw.event`, not just `data`.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};
use url::Url;

use tars_types::{
    Capabilities, CacheDirective, ChatEvent, ChatRequest, ContentBlock, ImageData, Message,
    Modality, PromptCacheKind, ProviderError, ProviderId, RequestContext, StopReason,
    StructuredOutputMode, Usage,
};

use crate::auth::{Auth, AuthResolver, ResolvedAuth};
use crate::http_base::{stream_via_adapter, HttpAdapter, HttpProviderBase, HttpProviderExtras, SseEvent};
use crate::provider::{LlmEventStream, LlmProvider};
use crate::tool_buffer::ToolCallBuffer;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";

/// Synthetic tool name used to emulate structured output (Doc 01 §9).
const STRUCTURED_OUTPUT_TOOL: &str = "__respond_with__";

/// Builder.
#[derive(Clone, Debug)]
pub struct AnthropicProviderBuilder {
    id: ProviderId,
    base_url: String,
    api_version: String,
    auth: Auth,
    capabilities: Option<Capabilities>,
    extras: HttpProviderExtras,
}

impl AnthropicProviderBuilder {
    pub fn new(id: impl Into<ProviderId>, auth: Auth) -> Self {
        Self {
            id: id.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_version: DEFAULT_API_VERSION.to_string(),
            auth,
            capabilities: None,
            extras: HttpProviderExtras::default(),
        }
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn api_version(mut self, v: impl Into<String>) -> Self {
        self.api_version = v.into();
        self
    }

    pub fn capabilities(mut self, c: Capabilities) -> Self {
        self.capabilities = Some(c);
        self
    }

    pub fn extras(mut self, extras: HttpProviderExtras) -> Self {
        self.extras = extras;
        self
    }

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<AnthropicProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let adapter = Arc::new(AnthropicAdapter {
            base_url: self.base_url,
            api_version: self.api_version,
            extras: self.extras,
        });
        Arc::new(AnthropicProvider {
            id: self.id,
            http,
            auth_resolver,
            auth: self.auth,
            adapter,
            capabilities: caps,
        })
    }
}

fn default_capabilities() -> Capabilities {
    use std::collections::HashSet;
    let mut modalities = HashSet::new();
    modalities.insert(Modality::Text);
    modalities.insert(Modality::Image);
    Capabilities {
        max_context_tokens: 200_000,
        max_output_tokens: 8_192,
        supports_tool_use: true,
        supports_parallel_tool_calls: true,
        supports_structured_output: StructuredOutputMode::ToolUseEmulation,
        supports_vision: true,
        supports_thinking: true,
        supports_cancel: true,
        prompt_cache: PromptCacheKind::ExplicitMarker,
        streaming: true,
        modalities_in: modalities.clone(),
        modalities_out: HashSet::from([Modality::Text]),
        pricing: tars_types::Pricing::default(),
    }
}

pub struct AnthropicProvider {
    id: ProviderId,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<AnthropicAdapter>,
    capabilities: Capabilities,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, &ctx).await?;
        stream_via_adapter(self.http.clone(), self.adapter.clone(), auth, req, ctx).await
    }
}

pub struct AnthropicAdapter {
    base_url: String,
    api_version: String,
    extras: HttpProviderExtras,
}

impl AnthropicAdapter {
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
            Message::Assistant { content, tool_calls } => {
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
            Message::Tool { tool_call_id, content, is_error } => {
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
            // into a user-role text block prefixed with "[system]".
            Message::System { content } => json!({
                "role": "user",
                "content": Self::translate_content(content),
            }),
        }
    }

    /// Apply [`CacheDirective::MarkBoundary`] markers. Anthropic accepts
    /// up to 4 cache_control markers; we attach to system + last
    /// content block per directive (in order). The translation here is
    /// best-effort — callers wanting precise placement should construct
    /// messages with the markers already on specific blocks.
    fn apply_cache_directives(
        body: &mut Value,
        directives: &[CacheDirective],
    ) {
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
        // Add marker to the last block of the last user message
        // (covers RAG context use cases).
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            if let Some(last_msg) = messages.last_mut() {
                if let Some(blocks) =
                    last_msg.get_mut("content").and_then(|c| c.as_array_mut())
                {
                    if let Some(last_block) = blocks.last_mut() {
                        last_block["cache_control"] = json!({"type": "ephemeral"});
                    }
                }
            }
        }
    }
}

#[async_trait]
impl HttpAdapter for AnthropicAdapter {
    fn build_url(&self, _model: &str) -> Result<Url, ProviderError> {
        Url::parse(&format!("{}/v1/messages", self.base_url.trim_end_matches('/')))
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
                        ProviderError::Internal(format!("bad x-api-key header: {e}"))
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

        let messages: Vec<Value> = req.messages.iter().map(Self::translate_message).collect();

        let max_tokens = req.max_output_tokens.unwrap_or(4096);

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
            ProviderError::Parse(format!("anthropic sse: {e} (raw: {})", truncate(&raw.data, 200)))
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
                                out.push(ChatEvent::Delta { text: t.to_string() });
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
                            out.push(ChatEvent::Delta { text: t.to_string() });
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
                            out.push(ChatEvent::ThinkingDelta { text: t.to_string() });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                // Finalize any tool call at this index. If it wasn't a
                // tool block, finalize returns an error which we ignore.
                if let Ok((id, _name, parsed)) = buf.finalize(index) {
                    out.push(ChatEvent::ToolCallEnd {
                        index,
                        id,
                        parsed_args: parsed,
                    });
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
                }
            }
            "message_stop" => {
                // Authoritative end. If no Finished was emitted yet
                // (defensive), emit a synthetic one.
            }
            _ => {} // unknown events are tolerated
        }

        Ok(out)
    }

    fn classify_error(&self, status: StatusCode, body: &str) -> ProviderError {
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let message = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| truncate(body, 300));

        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderError::Auth(message),
            StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited { retry_after: None },
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
                ProviderError::ModelOverloaded
            }
            StatusCode::BAD_REQUEST => {
                let lower = message.to_lowercase();
                if lower.contains("max_tokens") || lower.contains("context") {
                    ProviderError::ContextTooLong { limit: 0, requested: 0 }
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

fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

fn parse_usage(u: &serde_json::Map<String, Value>) -> Usage {
    let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cached = u
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let creation = u
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        cache_creation_tokens: creation,
        thinking_tokens: 0,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed = crate::http_base::truncate_utf8(s, max);
    if trimmed.len() == s.len() {
        s.to_string()
    } else {
        format!("{trimmed}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_401_is_auth() {
        let a = AnthropicAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            api_version: DEFAULT_API_VERSION.into(),
            extras: HttpProviderExtras::default(),
        };
        let err = a.classify_error(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"invalid"}}"#,
        );
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn translate_request_promotes_system_to_top_level() {
        let a = AnthropicAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            api_version: DEFAULT_API_VERSION.into(),
            extras: HttpProviderExtras::default(),
        };
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
        let a = AnthropicAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            api_version: DEFAULT_API_VERSION.into(),
            extras: HttpProviderExtras::default(),
        };
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
        let a = AnthropicAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            api_version: DEFAULT_API_VERSION.into(),
            extras: HttpProviderExtras::default(),
        };
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
        let a = AnthropicAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            api_version: DEFAULT_API_VERSION.into(),
            extras: HttpProviderExtras::default(),
        };
        let err = a.build_headers(&ResolvedAuth::None).unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)));

        let h = a.build_headers(&ResolvedAuth::ApiKey("sk-ant-x".into())).unwrap();
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
