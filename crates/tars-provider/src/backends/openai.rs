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
use crate::http_base::{stream_via_adapter, HttpAdapter, HttpProviderBase, HttpProviderExtras, SseEvent};
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
    extras: HttpProviderExtras,
}

impl OpenAiProviderBuilder {
    pub fn new(id: impl Into<ProviderId>, auth: Auth) -> Self {
        Self {
            id: id.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth,
            capabilities: None,
            extras: HttpProviderExtras::default(),
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

    /// Attach user-config-supplied http_headers / env_http_headers /
    /// query_params (Doc 01 §6.1 + codex-rs `ModelProviderInfo` parity).
    pub fn extras(mut self, extras: HttpProviderExtras) -> Self {
        self.extras = extras;
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
            extras: self.extras,
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
    extras: HttpProviderExtras,
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
                            // OpenAI demands `arguments` as a JSON-encoded
                            // string. `ToolCall::arguments` is documented
                            // to always be a `Value::Object` (enforced via
                            // debug_assert in `ToolCall::new`); serializing
                            // a Value never fails for valid in-memory
                            // values, so `expect` here is sound. Audit
                            // `tars-provider-src-backends-openai-1`:
                            // previously fell back to "{}" on error,
                            // silently sending wrong args to the model.
                            let args_str = serde_json::to_string(&tc.arguments)
                                .expect("ToolCall.arguments must serialize");
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
            Message::Tool { tool_call_id, content, is_error } => {
                // OpenAI's tool-role message has no literal `is_error`
                // field. The convention is to prefix the content with
                // a marker so the model sees the error semantically.
                // Audit finding `tars-provider-src-backends-openai-7`:
                // failed tool execution was being presented as success.
                let mut content_blocks = Self::translate_content(content);
                if *is_error {
                    if let Value::Array(arr) = &mut content_blocks {
                        arr.insert(
                            0,
                            json!({"type": "text", "text": "[tool execution failed]\n"}),
                        );
                    }
                }
                json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content_blocks,
                })
            }
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

        // Audit `tars-provider-src-backends-openai-11`: validate user
        // messages have non-empty content. OpenAI rejects
        // `{"role":"user","content":[]}` with a 400 — better to fail
        // fast with a typed error than waste a round trip.
        for m in &req.messages {
            if let Message::User { content } = m {
                if content.is_empty() {
                    return Err(ProviderError::InvalidRequest(
                        "Message::User content must not be empty".into(),
                    ));
                }
            }
        }

        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(sys) = &req.system {
            messages.push(json!({
                "role": "system",
                "content": [{"type": "text", "text": sys}],
            }));
        }
        for m in &req.messages {
            // Audit `tars-provider-src-backends-openai-9`: if the caller
            // supplied both `req.system` and an inline `Message::System`,
            // we used to emit two system blocks. The pattern here is
            // "system field wins" — drop inline System messages when
            // `req.system` is set.
            if req.system.is_some() && matches!(m, Message::System { .. }) {
                continue;
            }
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
            // Audit `tars-provider-src-backends-openai-13`: the comment
            // claimed we discard "defensively" because finish_reason
            // already finalizes tool calls. But finish_reason is
            // optional in some streams (proxies, partial server
            // implementations). Drain into ToolCallEnd events first so
            // accumulated tool args aren't silently dropped.
            let mut emitted = Vec::new();
            drain_buffer_into(buf, &mut emitted)?;
            // If a finish_reason chunk stashed a stop_reason and the
            // usage chunk never arrived (compatible servers that don't
            // honor `stream_options.include_usage`), emit a final
            // Finished now so the consumer always sees a terminal event.
            if let Some(stop) = buf.take_pending_stop() {
                emitted.push(ChatEvent::Finished {
                    stop_reason: stop,
                    usage: Usage::default(),
                });
            }
            return Ok(emitted);
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
                // Audit `tars-provider-src-backends-openai-{7,22}`: use
                // the stop_reason captured by an earlier finish_reason
                // chunk (typical OpenAI ordering: finish_reason in
                // chunk N, usage alone in chunk N+1). Default EndTurn
                // only when no prior chunk gave us anything.
                let stop_reason = buf.take_pending_stop().unwrap_or(StopReason::EndTurn);
                out.push(ChatEvent::Finished {
                    stop_reason,
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

        // Audit `tars-provider-src-backends-openai-15`: the previous
        // implementation had an empty `if` block pretending to emit
        // Started. Now we use ToolCallBuffer.take_started() to fire
        // exactly once per stream, on the first chunk carrying a model.
        if !model.is_empty() && buf.take_started() {
            out.push(ChatEvent::started(model.clone()));
        }

        for choice in choices {
            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    out.push(ChatEvent::Delta { text: content.to_string() });
                }
            }

            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for (iter_pos, tc) in tcs.iter().enumerate() {
                    // OpenAI's spec sends `index` for parallel tool
                    // calls. Audit `tars-provider-src-backends-openai-17`:
                    // unconditionally defaulting missing index to 0
                    // collapses parallel calls into one. Fall back to
                    // the iteration position so distinct tcs in the
                    // same delta stay distinct even if the spec slips.
                    let index = tc
                        .get("index")
                        .and_then(|i| i.as_u64())
                        .map(|i| i as usize)
                        .unwrap_or(iter_pos);
                    let id = tc.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();

                    // OpenAI sends id+name exactly once per tool_calls
                    // entry (the start chunk); subsequent chunks for
                    // the same `index` carry only `function.arguments`.
                    // So an `id`-bearing chunk = start; otherwise it's
                    // an args delta.
                    if !id.is_empty() {
                        // Audit `tars-provider-src-backends-openai-10`:
                        // accepting an empty `name` here propagated
                        // downstream as a tool call we couldn't dispatch.
                        // Treat it as a parse error per the OpenAI spec.
                        if name.is_empty() {
                            return Err(ProviderError::Parse(format!(
                                "openai tool_call delta has id `{id}` but missing function.name"
                            )));
                        }
                        out.push(ChatEvent::ToolCallStart {
                            index,
                            id: id.clone(),
                            name: name.clone(),
                        });
                        buf.on_start(index, id, name);
                    } else if !name.is_empty() {
                        // Audit `tars-provider-src-backends-openai-17`:
                        // previously synthesized an id when the provider
                        // omitted one. The spec mandates a stable id per
                        // tool call; inventing one breaks correlation
                        // with downstream consumers and masks the actual
                        // wire-format violation.
                        return Err(ProviderError::Parse(format!(
                            "openai tool_call delta has function.name `{name}` but missing id"
                        )));
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
                drain_buffer_into(buf, &mut out)?;

                let stop = match reason_str {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::MaxTokens,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "content_filter" => StopReason::ContentFilter,
                    _ => StopReason::Other,
                };
                // If usage rides along in this chunk, emit Finished now.
                // Otherwise stash the stop_reason and wait for the
                // separate usage-only chunk that
                // `stream_options.include_usage=true` generates.
                if let Some(usage_obj) = v.get("usage").and_then(|u| u.as_object()) {
                    out.push(ChatEvent::Finished {
                        stop_reason: stop,
                        usage: parse_openai_usage(usage_obj),
                    });
                } else {
                    buf.record_pending_stop(stop);
                }
            }
        }

        Ok(out)
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> ProviderError {
        // Try to parse `error.message` if present.
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.get("message")).cloned())
            .and_then(|m| m.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| truncate(body, 300));

        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderError::Auth(message),
            StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
                retry_after: crate::http_base::parse_retry_after(headers),
            },
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

    fn extras(&self) -> &HttpProviderExtras {
        &self.extras
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
///
/// Audit `tars-provider-src-backends-openai-29`: previously swallowed
/// finalize errors with `if let Ok(...)`, leaving consumers in an
/// inconsistent state when args were malformed. Now propagates.
/// Indices that were never started simply don't show up in the
/// inflight map and yield a benign `not started` error we filter out.
fn drain_buffer_into(buf: &mut ToolCallBuffer, out: &mut Vec<ChatEvent>) -> Result<(), ProviderError> {
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
    fn tool_message_marks_failure_when_is_error_set() {
        // Audit `tars-provider-src-backends-openai-7`: failed tool
        // executions used to be silently presented as successful.
        let m = Message::Tool {
            tool_call_id: "tu_1".into(),
            content: vec![tars_types::ContentBlock::text("permission denied")],
            is_error: true,
        };
        let v = OpenAiAdapter::translate_message(&m);
        assert_eq!(v["role"], "tool");
        // Marker prefix must appear so the model sees it as an error.
        assert_eq!(v["content"][0]["text"], "[tool execution failed]\n");
        assert_eq!(v["content"][1]["text"], "permission denied");
    }

    #[test]
    fn tool_message_no_marker_on_success() {
        let m = Message::Tool {
            tool_call_id: "tu_1".into(),
            content: vec![tars_types::ContentBlock::text("42")],
            is_error: false,
        };
        let v = OpenAiAdapter::translate_message(&m);
        // Just the original content, no error prefix.
        assert_eq!(v["content"][0]["text"], "42");
        assert!(v["content"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn translate_request_dedups_system_when_req_system_set() {
        // Audit `tars-provider-src-backends-openai-9`: caller set
        // both req.system AND an inline System message → 2 system
        // blocks were emitted. Now the inline System is skipped.
        let a = OpenAiAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            extras: HttpProviderExtras::default(),
        };
        let req = ChatRequest {
            model: tars_types::ModelHint::Explicit("gpt-4o".into()),
            system: Some("explicit system".into()),
            messages: vec![
                Message::System {
                    content: vec![tars_types::ContentBlock::text("inline system")],
                },
                Message::user_text("hi"),
            ],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: Default::default(),
        };
        let body = a.translate_request(&req).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let system_count = messages
            .iter()
            .filter(|m| m["role"] == "system")
            .count();
        assert_eq!(system_count, 1, "should dedupe to one system message");
        assert_eq!(messages[0]["content"][0]["text"], "explicit system");
    }

    fn empty_headers() -> reqwest::header::HeaderMap {
        reqwest::header::HeaderMap::new()
    }

    #[test]
    fn classify_401_is_auth() {
        let a = OpenAiAdapter { base_url: DEFAULT_BASE_URL.into(), extras: HttpProviderExtras::default() };
        let err = a.classify_error(
            StatusCode::UNAUTHORIZED,
            &empty_headers(),
            "{\"error\":{\"message\":\"bad\"}}",
        );
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let a = OpenAiAdapter { base_url: DEFAULT_BASE_URL.into(), extras: HttpProviderExtras::default() };
        let err = a.classify_error(StatusCode::TOO_MANY_REQUESTS, &empty_headers(), "");
        assert!(matches!(err, ProviderError::RateLimited { retry_after: None }));
    }

    #[test]
    fn classify_429_with_retry_after_seconds_populates_field() {
        let a = OpenAiAdapter { base_url: DEFAULT_BASE_URL.into(), extras: HttpProviderExtras::default() };
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "42".parse().unwrap());
        let err = a.classify_error(StatusCode::TOO_MANY_REQUESTS, &headers, "");
        match err {
            ProviderError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(42)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_400_context_length_is_typed() {
        let a = OpenAiAdapter { base_url: DEFAULT_BASE_URL.into(), extras: HttpProviderExtras::default() };
        let body = r#"{"error":{"message":"context_length_exceeded: too many tokens"}}"#;
        let err = a.classify_error(StatusCode::BAD_REQUEST, &empty_headers(), body);
        assert!(matches!(err, ProviderError::ContextTooLong { .. }));
    }
}
