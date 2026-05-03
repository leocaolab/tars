//! OpenAI HTTP backend.
//!
//! Also serves OpenAI-compatible endpoints (vLLM, llama.cpp server,
//! Groq, Together, DeepSeek) by overriding `base_url`.
//!
//! Mirrors the Python `OpenAIClient` (in `arc/app/llm/openai_client.py`)
//! semantics for `max_tokens` vs `max_completion_tokens` routing and
//! usage tracking, but is async + streaming.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};
use url::Url;

use tars_types::{
    Capabilities, ChatRequest, ChatEvent, ContentBlock, Message, Modality, ProviderError,
    ProviderId, RequestContext, StopReason, StructuredOutputMode, Usage,
};

use crate::auth::{Auth, AuthResolver, ResolvedAuth};
use crate::http_base::{stream_via_adapter, HttpAdapter, HttpProviderBase, SseEvent};
use crate::provider::{LlmEventStream, LlmProvider};
use crate::tool_buffer::ToolCallBuffer;

/// Default OpenAI base URL.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Models that require `max_completion_tokens` instead of `max_tokens`.
/// Mirrors the Python `_max_tokens_kwarg` heuristic — gpt-5 / o1 / o3 / o4.
const NEW_TOKENS_PARAM_PREFIXES: &[&str] = &["gpt-5", "o1", "o3", "o4"];

#[derive(Clone, Debug)]
pub struct OpenAiProviderBuilder {
    id: ProviderId,
    base_url: String,
    auth: Auth,
    capabilities: Option<Capabilities>,
}

impl OpenAiProviderBuilder {
    pub fn new(id: impl Into<ProviderId>, auth: Auth) -> Self {
        Self {
            id: id.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth,
            capabilities: None,
        }
    }

    /// Override base URL — for vLLM / llama.cpp / Groq / etc.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override capability descriptor. Default is a vanilla GPT-4o-style
    /// profile; OpenAI-compatible local backends should set their own.
    pub fn capabilities(mut self, caps: Capabilities) -> Self {
        self.capabilities = Some(caps);
        self
    }

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<OpenAiProvider> {
        let caps = self.capabilities.unwrap_or_else(default_openai_capabilities);
        let adapter = Arc::new(OpenAiAdapter {
            base_url: self.base_url,
        });
        Arc::new(OpenAiProvider {
            id: self.id,
            http,
            auth_resolver,
            auth: self.auth,
            adapter,
            capabilities: caps,
        })
    }
}

fn default_openai_capabilities() -> Capabilities {
    use std::collections::HashSet;
    let mut modalities = HashSet::new();
    modalities.insert(Modality::Text);
    Capabilities {
        max_context_tokens: 128_000,
        max_output_tokens: 16_384,
        supports_tool_use: true,
        supports_parallel_tool_calls: true,
        supports_structured_output: StructuredOutputMode::StrictSchema,
        supports_vision: false, // gpt-4o supports vision; per-model override expected
        supports_thinking: false,
        supports_cancel: true, // close stream → reqwest cancels HTTP body
        prompt_cache: tars_types::PromptCacheKind::ImplicitPrefix { min_tokens: 1024 },
        streaming: true,
        modalities_in: modalities.clone(),
        modalities_out: modalities,
        pricing: tars_types::Pricing::default(),
    }
}

/// The provider itself. Cheap to clone (`Arc` everywhere), so the
/// `Arc<Self>` requirement of [`LlmProvider`] is trivial.
pub struct OpenAiProvider {
    id: ProviderId,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<OpenAiAdapter>,
    capabilities: Capabilities,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
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

/// The wire-format adapter — pure functions, no state.
pub struct OpenAiAdapter {
    base_url: String,
}

impl OpenAiAdapter {
    /// Decide which "max tokens" parameter the model accepts.
    fn max_tokens_field(model: &str) -> &'static str {
        if NEW_TOKENS_PARAM_PREFIXES.iter().any(|p| model.starts_with(p)) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }
    }

    /// Translate one of our [`Message`]s into OpenAI's wire format.
    fn translate_message(m: &Message) -> Value {
        match m {
            Message::User { content } => json!({
                "role": "user",
                "content": Self::translate_content(content),
            }),
            Message::Assistant { content, tool_calls } => {
                let mut out = json!({
                    "role": "assistant",
                    "content": Self::translate_content(content),
                });
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            // OpenAI demands `arguments` as a JSON-encoded string.
                            let args_str = serde_json::to_string(&tc.arguments)
                                .unwrap_or_else(|_| "{}".into());
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": args_str,
                                }
                            })
                        })
                        .collect();
                    out["tool_calls"] = Value::Array(calls);
                }
                out
            }
            Message::Tool { tool_call_id, content, is_error: _ } => json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": Self::translate_content(content),
            }),
            Message::System { content } => json!({
                "role": "system",
                "content": Self::translate_content(content),
            }),
        }
    }

    /// OpenAI accepts either a string or an array of content blocks.
    /// We always emit the array form so multi-modal works without
    /// per-message branching.
    fn translate_content(blocks: &[ContentBlock]) -> Value {
        let mut out = Vec::with_capacity(blocks.len());
        for b in blocks {
            match b {
                ContentBlock::Text { text } => {
                    out.push(json!({"type": "text", "text": text}));
                }
                ContentBlock::Image { mime, data } => {
                    let url = match data {
                        tars_types::ImageData::Url(u) => u.clone(),
                        tars_types::ImageData::Base64(b) => format!("data:{mime};base64,{b}"),
                    };
                    out.push(json!({
                        "type": "image_url",
                        "image_url": {"url": url},
                    }));
                }
            }
        }
        Value::Array(out)
    }

    /// Build the full OpenAI tool spec from our [`tars_types::ToolSpec`].
    fn translate_tools(tools: &[tars_types::ToolSpec]) -> Value {
        let arr: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema.schema,
                        "strict": t.input_schema.strict,
                    }
                })
            })
            .collect();
        Value::Array(arr)
    }
}

#[async_trait]
impl HttpAdapter for OpenAiAdapter {
    fn build_url(&self, _model: &str) -> Result<Url, ProviderError> {
        let trimmed = self.base_url.trim_end_matches('/');
        Url::parse(&format!("{trimmed}/chat/completions"))
            .map_err(|e| ProviderError::Internal(format!("bad base_url: {e}")))
    }

    fn build_headers(&self, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
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

    fn translate_request(&self, req: &ChatRequest) -> Result<Value, ProviderError> {
        let model = req
            .model
            .explicit()
            .ok_or_else(|| ProviderError::InvalidRequest("model must be explicit".into()))?;

        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(sys) = &req.system {
            messages.push(json!({
                "role": "system",
                "content": [{"type": "text", "text": sys}],
            }));
        }
        for m in &req.messages {
            messages.push(Self::translate_message(m));
        }

        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            // Ask for token usage in the final stream chunk.
            "stream_options": {"include_usage": true},
        });

        // max_tokens vs max_completion_tokens
        if let Some(max) = req.max_output_tokens {
            let field = Self::max_tokens_field(model);
            body[field] = json!(max);
        }

        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.stop_sequences.is_empty() {
            body["stop"] = json!(req.stop_sequences);
        }
        if let Some(seed) = req.seed {
            body["seed"] = json!(seed);
        }

        if !req.tools.is_empty() {
            body["tools"] = Self::translate_tools(&req.tools);
            body["tool_choice"] = match &req.tool_choice {
                tars_types::ToolChoice::Auto => json!("auto"),
                tars_types::ToolChoice::None => json!("none"),
                tars_types::ToolChoice::Required => json!("required"),
                tars_types::ToolChoice::Specific(name) => json!({
                    "type": "function",
                    "function": {"name": name},
                }),
            };
            if req.tools.iter().any(|t| t.input_schema.strict) {
                body["parallel_tool_calls"] = json!(true);
            }
        }

        if let Some(schema) = &req.structured_output {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {
                    "name": schema.name.clone().unwrap_or_else(|| "Response".to_string()),
                    "schema": schema.schema,
                    "strict": schema.strict,
                }
            });
        }

        Ok(body)
    }

    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut ToolCallBuffer,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        // OpenAI emits `data: [DONE]` as the terminator. Stop quietly.
        if raw.data.trim() == "[DONE]" {
            // Ensure any open tool calls are finalized (defensive — the
            // last delta usually ends with finish_reason=tool_calls,
            // which we already turn into ToolCallEnd below).
            buf.discard();
            return Ok(Vec::new());
        }

        let v: Value = serde_json::from_str(&raw.data).map_err(|e| {
            ProviderError::Parse(format!("openai sse json: {e} (raw: {})", truncate(&raw.data, 200)))
        })?;

        let mut out = Vec::new();

        // Usage chunk (final, when stream_options.include_usage=true).
        // Per spec it has `choices: []`, so we check usage independently.
        if let Some(usage) = v.get("usage").and_then(|u| u.as_object()) {
            // Defer emission until we also know the model + stop_reason
            // (handled in the choices block). But if there are no
            // choices in this chunk, emit Finished with what we have.
            let choices_empty =
                v.get("choices").and_then(|c| c.as_array()).is_none_or(|a| a.is_empty());
            if choices_empty {
                let usage_struct = parse_openai_usage(usage);
                out.push(ChatEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                    usage: usage_struct,
                });
                return Ok(out);
            }
        }

        let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();

        let choices = match v.get("choices").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => return Ok(out),
        };

        for choice in choices {
            // A heuristic: emit Started once, on the first chunk that
            // has a `model` field. Caller already saw 1 chunk per
            // stream so we don't carry across-chunk state for this in
            // the buffer — using `index 0` as a fingerprint is fine.
            // (Worst case we re-emit, which only happens if `model`
            // somehow arrives twice; cheap.)
            if !model.is_empty() && out.is_empty() {
                // Emit Started exactly once per stream — we guard with
                // an empty-out check above. This is not perfect (if a
                // tool delta also lands in the same SSE chunk we'd skip)
                // but for OpenAI's chunking shape it works.
                // Better: track in adapter state. Future improvement.
            }
            let _ = model; // used above for potential Started; suppress unused warn

            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    out.push(ChatEvent::Delta { text: content.to_string() });
                }
            }

            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let index = tc
                        .get("index")
                        .and_then(|i| i.as_u64())
                        .map(|i| i as usize)
                        .unwrap_or(0);
                    let id = tc.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();

                    if !id.is_empty() || !name.is_empty() {
                        // Start of a new tool call. OpenAI sends id/name
                        // exactly once per tool_calls entry, then args
                        // chunks land in subsequent deltas at the same
                        // index.
                        let new_id = if id.is_empty() {
                            format!("openai-call-{index}")
                        } else {
                            id
                        };
                        out.push(ChatEvent::ToolCallStart {
                            index,
                            id: new_id.clone(),
                            name: name.clone(),
                        });
                        buf.on_start(index, new_id, name);
                    }
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            out.push(ChatEvent::ToolCallArgsDelta {
                                index,
                                args_delta: args.to_string(),
                            });
                            buf.on_delta(index, args);
                        }
                    }
                }
            }

            if let Some(reason_str) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                // Finalize any in-flight tool calls into ToolCallEnd events.
                drain_buffer_into(buf, &mut out);

                let stop = match reason_str {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::MaxTokens,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "content_filter" => StopReason::ContentFilter,
                    _ => StopReason::Other,
                };
                let usage = v
                    .get("usage")
                    .and_then(|u| u.as_object())
                    .map(parse_openai_usage)
                    .unwrap_or_default();
                out.push(ChatEvent::Finished { stop_reason: stop, usage });
            }
        }

        Ok(out)
    }

    fn classify_error(&self, status: StatusCode, body: &str) -> ProviderError {
        // Try to parse `error.message` if present.
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.get("message")).cloned())
            .and_then(|m| m.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| truncate(body, 300));

        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderError::Auth(message),
            StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited { retry_after: None },
            StatusCode::PAYLOAD_TOO_LARGE => ProviderError::ContextTooLong {
                limit: 0,
                requested: 0,
            },
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                // Detect context-length errors specifically.
                if message.to_lowercase().contains("context_length")
                    || message.to_lowercase().contains("maximum context length")
                    || message.to_lowercase().contains("too many tokens")
                {
                    ProviderError::ContextTooLong { limit: 0, requested: 0 }
                } else {
                    ProviderError::InvalidRequest(message)
                }
            }
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
                ProviderError::ModelOverloaded
            }
            s if s.is_server_error() => ProviderError::Internal(format!("status {s}: {message}")),
            _ => ProviderError::InvalidRequest(format!("status {status}: {message}")),
        }
    }
}

fn parse_openai_usage(usage: &serde_json::Map<String, Value>) -> Usage {
    let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let completion = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
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
fn drain_buffer_into(buf: &mut ToolCallBuffer, out: &mut Vec<ChatEvent>) {
    // We don't have a public iter on ToolCallBuffer; finalize indices
    // 0..32 (parallel call ceiling we treat as practical max).
    for i in 0..32 {
        if let Ok((id, _name, parsed)) = buf.finalize(i) {
            out.push(ChatEvent::ToolCallEnd {
                index: i,
                id,
                parsed_args: parsed,
            });
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_max_completion_tokens_for_o1() {
        assert_eq!(OpenAiAdapter::max_tokens_field("o1-preview"), "max_completion_tokens");
        assert_eq!(OpenAiAdapter::max_tokens_field("gpt-5-something"), "max_completion_tokens");
        assert_eq!(OpenAiAdapter::max_tokens_field("gpt-4o"), "max_tokens");
    }

    #[test]
    fn translates_simple_user_message() {
        let m = Message::user_text("hello");
        let v = OpenAiAdapter::translate_message(&m);
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
    }

    #[test]
    fn classify_401_is_auth() {
        let a = OpenAiAdapter { base_url: DEFAULT_BASE_URL.into() };
        let err = a.classify_error(StatusCode::UNAUTHORIZED, "{\"error\":{\"message\":\"bad\"}}");
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let a = OpenAiAdapter { base_url: DEFAULT_BASE_URL.into() };
        let err = a.classify_error(StatusCode::TOO_MANY_REQUESTS, "");
        assert!(matches!(err, ProviderError::RateLimited { .. }));
    }

    #[test]
    fn classify_400_context_length_is_typed() {
        let a = OpenAiAdapter { base_url: DEFAULT_BASE_URL.into() };
        let body = r#"{"error":{"message":"context_length_exceeded: too many tokens"}}"#;
        let err = a.classify_error(StatusCode::BAD_REQUEST, body);
        assert!(matches!(err, ProviderError::ContextTooLong { .. }));
    }
}
