//! Google Gemini HTTP backend.
//!
//! Wire format reference:
//! <https://ai.google.dev/gemini-api/docs/text-generation>
//!
//! Differences from OpenAI / Anthropic:
//!
//! - **Auth**: `?key=...` query param (alternative: ADC bearer for
//!   Vertex AI, not yet supported here).
//! - **Roles**: assistant is `model`, not `assistant`. System is a
//!   separate `system_instruction` (NOT a role).
//! - **Messages**: `contents` array, each with `role` + `parts`.
//! - **Tool calls**: `functionCall` part (singular, no `tool_calls` list);
//!   parallel calls = multiple parts in the same message.
//! - **Tool results**: `functionResponse` part inside a `user`-role message.
//! - **Structured output**: `responseSchema` + `responseMimeType`.
//! - **Thinking**: parts have a `thought: bool` flag; thinking config
//!   sets `thinking_config.thinking_budget`.
//! - **Safety filter null**: when blocked the response has
//!   `candidates: null` — surface as ContentFiltered, don't index `[0]`.
//! - **Streaming endpoint**: `streamGenerateContent?alt=sse&key=...`.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};
use url::Url;

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ContentBlock, ImageData, Message, Modality,
    PromptCacheKind, ProviderError, ProviderId, RequestContext, StopReason,
    StructuredOutputMode, Usage,
};

use crate::auth::{Auth, AuthResolver, ResolvedAuth};
use crate::http_base::{stream_via_adapter, HttpAdapter, HttpProviderBase, HttpProviderExtras, SseEvent};
use crate::provider::{LlmEventStream, LlmProvider};
use crate::tool_buffer::ToolCallBuffer;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const API_VERSION: &str = "v1beta";

#[derive(Clone, Debug)]
pub struct GeminiProviderBuilder {
    id: ProviderId,
    base_url: String,
    auth: Auth,
    capabilities: Option<Capabilities>,
    extras: HttpProviderExtras,
}

impl GeminiProviderBuilder {
    pub fn new(id: impl Into<ProviderId>, auth: Auth) -> Self {
        Self {
            id: id.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth,
            capabilities: None,
            extras: HttpProviderExtras::default(),
        }
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
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
    ) -> Arc<GeminiProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let adapter = Arc::new(GeminiAdapter {
            base_url: self.base_url,
            extras: self.extras,
        });
        Arc::new(GeminiProvider {
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
        max_context_tokens: 1_048_576, // Gemini 1.5+ class
        max_output_tokens: 8_192,
        supports_tool_use: true,
        supports_parallel_tool_calls: true,
        supports_structured_output: StructuredOutputMode::StrictSchema,
        supports_vision: true,
        supports_thinking: true,
        supports_cancel: true,
        prompt_cache: PromptCacheKind::ExplicitObject, // cachedContents API
        streaming: true,
        modalities_in: modalities.clone(),
        modalities_out: HashSet::from([Modality::Text]),
        pricing: tars_types::Pricing::default(),
    }
}

pub struct GeminiProvider {
    id: ProviderId,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<GeminiAdapter>,
    capabilities: Capabilities,
}

#[async_trait]
impl LlmProvider for GeminiProvider {
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
        // Gemini puts the key in the query string. We pre-build the URL
        // with the key folded in, and don't set any auth headers.
        // Adapter handles model-name→URL with the key already present.
        let resolved = match auth {
            ResolvedAuth::ApiKey(k) => {
                if k.is_empty() {
                    return Err(ProviderError::Auth(
                        "Gemini API key is empty".into(),
                    ));
                }
                ResolvedAuthWithKey::Key(k)
            }
            ResolvedAuth::Bearer(_) => {
                return Err(ProviderError::Auth(
                    "Gemini bearer auth (Vertex AI ADC) is not yet supported; use an API key".into(),
                ));
            }
            ResolvedAuth::None => {
                return Err(ProviderError::Auth(
                    "Gemini provider requires an API key".into(),
                ));
            }
        };
        let adapter_with_key = Arc::new(GeminiAdapterWithKey {
            inner: self.adapter.clone(),
            key: match resolved {
                ResolvedAuthWithKey::Key(k) => k,
            },
        });
        // Cast through the trait — `stream_via_adapter` takes any HttpAdapter.
        // We resolve auth to None at the layer below since the key is
        // already in the URL.
        stream_via_adapter(self.http.clone(), adapter_with_key, ResolvedAuth::None, req, ctx)
            .await
    }
}

enum ResolvedAuthWithKey {
    Key(String),
}

/// Pure adapter without the API key (for testability).
pub struct GeminiAdapter {
    base_url: String,
    extras: HttpProviderExtras,
}

/// Composed adapter that knows the API key — produced per request.
struct GeminiAdapterWithKey {
    inner: Arc<GeminiAdapter>,
    key: String,
}

#[async_trait]
impl HttpAdapter for GeminiAdapterWithKey {
    fn build_url(&self, model: &str) -> Result<Url, ProviderError> {
        // streamGenerateContent + alt=sse for SSE framing.
        let trimmed = self.inner.base_url.trim_end_matches('/');
        Url::parse(&format!(
            "{trimmed}/{API_VERSION}/models/{model}:streamGenerateContent?alt=sse&key={}",
            urlencoding(&self.key)
        ))
        .map_err(|e| {
            ProviderError::Internal(format!(
                "bad gemini url for model '{model}' (base_url='{trimmed}'): {e}"
            ))
        })
    }

    fn build_headers(&self, _auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest) -> Result<Value, ProviderError> {
        self.inner.translate_request(req)
    }

    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut ToolCallBuffer,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        self.inner.parse_event(raw, buf)
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> ProviderError {
        self.inner.classify_error(status, headers, body)
    }

    fn extras(&self) -> &HttpProviderExtras {
        &self.inner.extras
    }
}

impl GeminiAdapter {
    fn translate_part(b: &ContentBlock) -> Value {
        match b {
            ContentBlock::Text { text } => json!({"text": text}),
            ContentBlock::Image { mime, data } => match data {
                ImageData::Base64(b) => json!({
                    "inline_data": {
                        "mime_type": mime,
                        "data": b,
                    }
                }),
                ImageData::Url(u) => json!({
                    "file_data": {
                        "mime_type": mime,
                        "file_uri": u,
                    }
                }),
            },
        }
    }

    fn translate_message(m: &Message) -> Result<Value, ProviderError> {
        match m {
            Message::User { content } => {
                let parts: Vec<Value> = content.iter().map(Self::translate_part).collect();
                Ok(json!({"role": "user", "parts": parts}))
            }
            Message::Assistant { content, tool_calls } => {
                let mut parts: Vec<Value> = content.iter().map(Self::translate_part).collect();
                for tc in tool_calls {
                    parts.push(json!({
                        "functionCall": {
                            "name": tc.name,
                            "args": tc.arguments,
                        }
                    }));
                }
                Ok(json!({"role": "model", "parts": parts}))
            }
            Message::Tool { tool_call_id, content, is_error } => {
                // Gemini's functionResponse needs the function's *name*, but
                // tars's canonical Message::Tool only carries the
                // `tool_call_id`. Convention: callers targeting Gemini must
                // encode the function name as `<name>@<id>` so this layer
                // can recover it. Reject with a clear, actionable error
                // when the convention is missing — Gemini's own 400 is
                // cryptic ("functionResponse.name is required").
                let name = match tool_call_id.split_once('@') {
                    Some((n, _)) if !n.is_empty() => n,
                    _ => {
                        return Err(ProviderError::InvalidRequest(format!(
                            "Gemini tool result requires `<name>@<id>` tool_call_id; got `{tool_call_id}`"
                        )));
                    }
                };
                let text = content
                    .first()
                    .and_then(|b| b.as_text())
                    .unwrap_or("")
                    .to_string();
                // Encode failure into the response object so the model
                // doesn't mistake a failed call for a successful one.
                // Gemini doesn't have a dedicated `is_error` flag for
                // functionResponse, so we surface it inside `response`.
                let response = if *is_error {
                    json!({"error": text})
                } else {
                    json!({"output": text})
                };
                Ok(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": response,
                        }
                    }]
                }))
            }
            Message::System { content } => {
                let parts: Vec<Value> = content.iter().map(Self::translate_part).collect();
                Ok(json!({"role": "user", "parts": parts}))
            }
        }
    }

    fn translate_tools(tools: &[tars_types::ToolSpec]) -> Value {
        let declarations: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema.schema,
                })
            })
            .collect();
        json!([{"functionDeclarations": declarations}])
    }
}

#[async_trait]
impl HttpAdapter for GeminiAdapter {
    fn build_url(&self, model: &str) -> Result<Url, ProviderError> {
        // Used only directly in tests; production path goes via
        // `GeminiAdapterWithKey`.
        let trimmed = self.base_url.trim_end_matches('/');
        Url::parse(&format!(
            "{trimmed}/{API_VERSION}/models/{model}:streamGenerateContent?alt=sse"
        ))
        .map_err(|e| {
            ProviderError::Internal(format!(
                "bad gemini url for model '{model}' (base_url='{trimmed}'): {e}"
            ))
        })
    }

    fn build_headers(&self, _auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest) -> Result<Value, ProviderError> {
        let _ = req.model.explicit().ok_or_else(|| {
            ProviderError::InvalidRequest(format!(
                "model must be explicit, got: {:?}",
                req.model
            ))
        })?;

        if req.messages.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "messages cannot be empty".into(),
            ));
        }

        let contents: Vec<Value> = req
            .messages
            .iter()
            .map(Self::translate_message)
            .collect::<Result<_, _>>()?;

        let mut body = json!({
            "contents": contents,
        });

        let mut config = json!({});
        if let Some(max) = req.max_output_tokens {
            config["maxOutputTokens"] = json!(max);
        }
        if let Some(t) = req.temperature {
            config["temperature"] = json!(t);
        }
        if !req.stop_sequences.is_empty() {
            config["stopSequences"] = json!(req.stop_sequences);
        }
        if let Some(seed) = req.seed {
            config["seed"] = json!(seed);
        }

        if let Some(schema) = &req.structured_output {
            config["responseMimeType"] = json!("application/json");
            config["responseSchema"] = schema.schema.clone();
        }

        // Thinking config (Gemini 2.5+ family).
        match req.thinking {
            tars_types::ThinkingMode::Off => {
                config["thinkingConfig"] = json!({"thinkingBudget": 0});
            }
            tars_types::ThinkingMode::Auto => {
                config["thinkingConfig"] = json!({"thinkingBudget": -1});
            }
            tars_types::ThinkingMode::Budget(b) => {
                config["thinkingConfig"] = json!({"thinkingBudget": b});
            }
        }

        if config.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            body["generationConfig"] = config;
        }

        if let Some(sys) = &req.system {
            // Gemini's `systemInstruction` is a Content but role is
            // implicit ("system") — passing role: "user" here is
            // misleading and contradicts the file-level header doc.
            body["systemInstruction"] = json!({
                "parts": [{"text": sys}],
            });
        }

        if !req.tools.is_empty() {
            body["tools"] = Self::translate_tools(&req.tools);
            // tool_choice → toolConfig.functionCallingConfig.mode
            let mode = match &req.tool_choice {
                tars_types::ToolChoice::Auto => json!({"mode": "AUTO"}),
                tars_types::ToolChoice::None => json!({"mode": "NONE"}),
                tars_types::ToolChoice::Required => json!({"mode": "ANY"}),
                tars_types::ToolChoice::Specific(name) => {
                    if name.is_empty() {
                        return Err(ProviderError::InvalidRequest(
                            "ToolChoice::Specific name is empty".into(),
                        ));
                    }
                    if !req.tools.iter().any(|t| &t.name == name) {
                        return Err(ProviderError::InvalidRequest(format!(
                            "ToolChoice::Specific(`{name}`) not present in tools list"
                        )));
                    }
                    json!({
                        "mode": "ANY",
                        "allowed_function_names": [name],
                    })
                }
            };
            body["toolConfig"] = json!({"functionCallingConfig": mode});
        }

        // Cache directive → cachedContent reference.
        for d in &req.cache_directives {
            if let tars_types::CacheDirective::UseExplicit { handle } = d {
                body["cachedContent"] = json!(handle.external_id);
                break;
            }
        }

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
        let v: Value = serde_json::from_str(&raw.data).map_err(|e| {
            ProviderError::Parse(format!("gemini sse: {e} (raw: {})", truncate(&raw.data, 200)))
        })?;

        let mut out = Vec::new();
        let model_version = v
            .get("modelVersion")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();

        // Started event — emit on first chunk that has modelVersion.
        // We don't have cross-chunk state in the adapter, but `Started`
        // is idempotent enough for our purposes (downstream builder
        // overwrites). To avoid duplicates we track via a sentinel in
        // the buffer's discard state — buffer is fresh per-stream, so
        // we use the absence of a tag to gate.
        // For simplicity emit Started for every chunk where it's the
        // first content of the chunk; consumers (ChatResponseBuilder)
        // overwrite cleanly.

        // Safety filter: candidates is None / missing.
        let candidates = match v.get("candidates").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => {
                // promptFeedback indicates the prompt itself was blocked.
                if let Some(fb) = v.get("promptFeedback") {
                    let category = fb
                        .get("blockReason")
                        .and_then(|s| s.as_str())
                        .unwrap_or("safety")
                        .to_string();
                    return Err(ProviderError::ContentFiltered { category });
                }
                // Otherwise nothing to do.
                return Ok(out);
            }
        };

        if !model_version.is_empty() && out.is_empty() {
            out.push(ChatEvent::started(model_version));
        }

        // ChatEvent contract: exactly one Finished event per response.
        // Gemini may return >1 candidate when n>1 sampling is requested,
        // but our request never asks for that — and emitting one
        // Finished per candidate would break downstream consumers.
        // Process only candidates[0]; ignore the rest.
        if let Some(cand) = candidates.first() {
            // Track whether this candidate contained any tool call so we
            // can override Gemini's generic `STOP` finishReason → ToolUse
            // when appropriate. Gemini doesn't have a dedicated
            // tool-use stop reason the way OpenAI/Anthropic do; without
            // this normalization the cross-provider conformance suite
            // sees ToolUse on those two but EndTurn on Gemini for the
            // same logical "model wants to call a function" outcome.
            let mut had_function_call = false;

            // Parts inside content.
            let parts = cand
                .pointer("/content/parts")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            // Tool-call index is a counter over functionCall parts only —
            // not the part position in the array. Mixing text parts and
            // functionCall parts would otherwise produce non-sequential
            // tool indices that downstream tool-buffer logic expects to
            // be 0..N.
            let mut fc_idx: usize = 0;
            for part in parts.into_iter() {
                let is_thought = part
                    .get("thought")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false);
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        if is_thought {
                            out.push(ChatEvent::ThinkingDelta {
                                text: text.to_string(),
                            });
                        } else {
                            out.push(ChatEvent::Delta { text: text.to_string() });
                        }
                    }
                }
                if let Some(fc) = part.get("functionCall") {
                    had_function_call = true;
                    let name = fc
                        .get("name")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = fc.get("args").cloned().unwrap_or(Value::Object(Default::default()));
                    // Embed the function name in the id so that downstream
                    // Tool messages can recover it via the `<name>@<id>`
                    // convention required by `translate_message`.
                    let call_id = format!("{name}@gemini-call-{fc_idx}");
                    let idx = fc_idx;
                    fc_idx += 1;
                    out.push(ChatEvent::ToolCallStart {
                        index: idx,
                        id: call_id.clone(),
                        name: name.clone(),
                    });
                    // Gemini gives args as a parsed object — feed it as a
                    // single delta containing the JSON, then finalize.
                    let args_str = args.to_string();
                    out.push(ChatEvent::ToolCallArgsDelta {
                        index: idx,
                        args_delta: args_str.clone(),
                    });
                    buf.on_start(idx, call_id.clone(), name);
                    buf.on_delta(idx, &args_str);
                    let (id, _name, parsed) = buf.finalize(idx)?;
                    out.push(ChatEvent::ToolCallEnd {
                        index: idx,
                        id,
                        parsed_args: parsed,
                    });
                }
            }

            if let Some(reason_str) = cand.get("finishReason").and_then(|r| r.as_str()) {
                let usage = v
                    .get("usageMetadata")
                    .and_then(|u| u.as_object())
                    .map(parse_usage)
                    .unwrap_or_default();
                let mut stop = map_stop_reason(reason_str);
                if had_function_call && matches!(stop, StopReason::EndTurn) {
                    // Cross-provider normalization (Doc 01 §8): when the
                    // model emitted a tool call, the upstream signal is
                    // "caller, please run this and continue" — same as
                    // OpenAI's `tool_calls` and Anthropic's `tool_use`.
                    // Surface that instead of the wire-level STOP.
                    stop = StopReason::ToolUse;
                }
                out.push(ChatEvent::Finished {
                    stop_reason: stop,
                    usage,
                });
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
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let message = v
            .pointer("/error/message")
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
                if lower.contains("token") && lower.contains("limit") {
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
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" => StopReason::ContentFilter,
        "FINISH_REASON_UNSPECIFIED" | "OTHER" => StopReason::Other,
        _ => StopReason::Other,
    }
}

fn parse_usage(u: &serde_json::Map<String, Value>) -> Usage {
    let prompt = u.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0);
    let candidates = u
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = u
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let thoughts = u
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Usage {
        input_tokens: prompt,
        output_tokens: candidates,
        cached_input_tokens: cached,
        cache_creation_tokens: 0,
        thinking_tokens: thoughts,
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

/// Minimal URL-encode. We control the input (resolved API key), so a
/// correct-by-construction subset suffices.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> GeminiAdapter {
        GeminiAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            extras: HttpProviderExtras::default(),
        }
    }

    #[test]
    fn translates_assistant_to_model_role() {
        let m = Message::assistant_text("hello");
        let v = GeminiAdapter::translate_message(&m).unwrap();
        assert_eq!(v["role"], "model");
        assert_eq!(v["parts"][0]["text"], "hello");
    }

    #[test]
    fn translates_user_to_user_role() {
        let m = Message::user_text("hi");
        let v = GeminiAdapter::translate_message(&m).unwrap();
        assert_eq!(v["role"], "user");
    }

    #[test]
    fn tool_message_without_name_in_id_is_invalid_request() {
        let m = Message::Tool {
            tool_call_id: "abc-123".into(),
            content: vec![tars_types::ContentBlock::text("ok")],
            is_error: false,
        };
        let err = GeminiAdapter::translate_message(&m).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn tool_message_extracts_name_from_id_and_encodes_error() {
        let m = Message::Tool {
            tool_call_id: "search@call-1".into(),
            content: vec![tars_types::ContentBlock::text("boom")],
            is_error: true,
        };
        let v = GeminiAdapter::translate_message(&m).unwrap();
        assert_eq!(v["parts"][0]["functionResponse"]["name"], "search");
        assert_eq!(
            v["parts"][0]["functionResponse"]["response"]["error"],
            "boom"
        );
    }

    #[test]
    fn empty_messages_rejected_early() {
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        );
        req.messages.clear();
        let err = adapter().translate_request(&req).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn tool_choice_specific_unknown_name_rejected() {
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        );
        req.tools = vec![tars_types::ToolSpec {
            name: "real_tool".into(),
            description: "".into(),
            input_schema: tars_types::JsonSchema::strict("X", serde_json::json!({"type":"object"})),
        }];
        req.tool_choice = tars_types::ToolChoice::Specific("ghost".into());
        let err = adapter().translate_request(&req).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn system_instruction_has_no_role_field() {
        let req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        )
        .with_system("be brief");
        let body = adapter().translate_request(&req).unwrap();
        assert!(body["systemInstruction"].get("role").is_none());
    }

    #[test]
    fn parse_event_uses_only_first_candidate() {
        let mut buf = ToolCallBuffer::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"candidates":[
                {"content":{"parts":[{"text":"a"}]},"finishReason":"STOP"},
                {"content":{"parts":[{"text":"b"}]},"finishReason":"STOP"}
            ]}"#.into(),
        };
        let events = adapter().parse_event(&raw, &mut buf).unwrap();
        let finished = events.iter().filter(|e| e.is_terminal()).count();
        assert_eq!(finished, 1, "exactly one Finished event expected");
        let has_b = events.iter().any(|e| matches!(e, ChatEvent::Delta { text } if text == "b"));
        assert!(!has_b, "second candidate must be ignored");
    }

    #[test]
    fn parse_event_tool_call_id_embeds_name() {
        let mut buf = ToolCallBuffer::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"candidates":[
                {"content":{"parts":[
                    {"text":"thinking..."},
                    {"functionCall":{"name":"search","args":{"q":"x"}}}
                ]},"finishReason":"STOP"}
            ]}"#.into(),
        };
        let events = adapter().parse_event(&raw, &mut buf).unwrap();
        let start = events.iter().find_map(|e| match e {
            ChatEvent::ToolCallStart { index, id, name } => {
                Some((*index, id.clone(), name.clone()))
            }
            _ => None,
        });
        let (idx, id, name) = start.expect("ToolCallStart present");
        assert_eq!(idx, 0, "function-call counter resets across mixed parts");
        assert_eq!(name, "search");
        assert!(id.starts_with("search@"), "id must embed name: {id}");
        let stop = events.iter().find_map(|e| match e {
            ChatEvent::Finished { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        });
        assert_eq!(stop, Some(StopReason::ToolUse));
    }

    #[test]
    fn system_promotes_to_system_instruction() {
        let req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        )
        .with_system("be brief");
        let body = adapter().translate_request(&req).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert!(body["contents"].is_array());
    }

    #[test]
    fn structured_output_sets_response_schema() {
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "json please",
        );
        req.structured_output = Some(tars_types::JsonSchema::strict(
            "Resp",
            serde_json::json!({"type":"object"}),
        ));
        let body = adapter().translate_request(&req).unwrap();
        assert_eq!(body["generationConfig"]["responseMimeType"], "application/json");
        assert!(body["generationConfig"]["responseSchema"].is_object());
    }

    #[test]
    fn thinking_off_sets_zero_budget() {
        let req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        );
        let body = adapter().translate_request(&req).unwrap();
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0);
    }

    #[test]
    fn safety_block_returns_content_filtered() {
        let mut buf = ToolCallBuffer::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#.into(),
        };
        let err = adapter().parse_event(&raw, &mut buf).unwrap_err();
        assert!(matches!(
            err,
            ProviderError::ContentFiltered { ref category } if category == "SAFETY"
        ));
    }

    #[test]
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason("STOP"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("MAX_TOKENS"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("SAFETY"), StopReason::ContentFilter);
        assert_eq!(map_stop_reason("RECITATION"), StopReason::ContentFilter);
        assert_eq!(map_stop_reason("WHATEVER"), StopReason::Other);
    }

    #[test]
    fn url_encode_handles_special_chars() {
        assert_eq!(urlencoding("abc-123_X"), "abc-123_X");
        assert_eq!(urlencoding("a b"), "a%20b");
        assert_eq!(urlencoding("a/b"), "a%2Fb");
    }

    #[test]
    fn classify_400_token_limit_is_context_too_long() {
        let a = adapter();
        let body = r#"{"error":{"message":"input token limit exceeded"}}"#;
        let err = a.classify_error(
            StatusCode::BAD_REQUEST,
            &reqwest::header::HeaderMap::new(),
            body,
        );
        assert!(matches!(err, ProviderError::ContextTooLong { .. }));
    }
}
