//! OpenAI HTTP backend.
//!
//! Also serves OpenAI-compatible endpoints (vLLM, llama.cpp server,
//! Groq, Together, DeepSeek) by overriding `base_url`.
//!
//! Mirrors the Python `OpenAIClient` (the equivalent Python OpenAI client)
//! semantics for `max_tokens` vs `max_completion_tokens` routing and
//! usage tracking, but is async + streaming.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use url::Url;

use tars_types::{
    BatchItemId, BatchJobId, BatchResultItem, BatchStatus, Capabilities, ChatEvent, ChatRequest,
    ChatResponse, ChatResponseBuilder, ContentBlock, Message, Modality, ProviderError,
    ProviderId, RequestContext, StopReason, StructuredOutputMode, Usage,
};

use crate::auth::{Auth, AuthResolver, ResolvedAuth};
use crate::batch::BatchSubmitter;
use crate::http_base::{
    HttpAdapter, HttpProviderBase, HttpProviderExtras, SseEvent, stream_via_adapter,
};
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
    /// query_params (Doc 01 §6.1).
    pub fn extras(mut self, extras: HttpProviderExtras) -> Self {
        self.extras = extras;
        self
    }

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<OpenAiProvider> {
        let caps = self
            .capabilities
            .unwrap_or_else(default_openai_capabilities);
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

pub fn default_openai_capabilities() -> Capabilities {
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

    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn BatchSubmitter>> {
        Some(self)
    }
}

// ─── BatchSubmitter — OpenAI Batch API ──────────────────────────────
//
// Reference: <https://platform.openai.com/docs/api-reference/batch>
//
// Two-step submission (different from Anthropic's one-step):
//   1) POST /files  (multipart, purpose=batch) → file_id
//   2) POST /batches { input_file_id, endpoint, completion_window } → job
//
// Results come back as a separate output file (output_file_id on the
// batch object); fetch via GET /files/{id}/content. Errors during the
// batch surface in an error_file_id similarly.
//
// Per-line JSONL shape (input):
//   {"custom_id": "...", "method":"POST", "url":"/v1/chat/completions",
//    "body": <chat completion request body>}
//
// Per-line JSONL shape (output):
//   {"custom_id": "...", "response": {"status_code":200,"body":{...}}, "error": null}
//   or {"custom_id": "...", "response": null, "error":{...}}

#[async_trait]
impl BatchSubmitter for OpenAiProvider {
    async fn submit(
        &self,
        items: Vec<(BatchItemId, ChatRequest)>,
    ) -> Result<BatchJobId, ProviderError> {
        if items.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "batch submit: items list must not be empty".into(),
            ));
        }

        // 1) Build the JSONL input file content.
        let mut jsonl = String::with_capacity(items.len() * 256);
        for (item_id, req) in &items {
            let body = self.adapter.translate_request(req)?;
            let line = serde_json::to_string(&json!({
                "custom_id": item_id.as_str(),
                "method": "POST",
                "url": "/v1/chat/completions",
                "body": body,
            }))
            .map_err(|e| ProviderError::Internal(format!("batch input serialize: {e}")))?;
            jsonl.push_str(&line);
            jsonl.push('\n');
        }

        // 2) Upload the JSONL as a "batch" purpose file via multipart.
        let auth = self
            .auth_resolver
            .resolve(&self.auth, &RequestContext::test_default())
            .await?;
        let auth_only = openai_auth_only_headers(&auth)?;

        let file_part = reqwest::multipart::Part::bytes(jsonl.into_bytes())
            .file_name("batch.jsonl")
            .mime_str("application/jsonl")
            .map_err(|e| ProviderError::Internal(format!("multipart part: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("purpose", "batch")
            .part("file", file_part);

        let upload_url = self.adapter.files_url("")?;
        let upload_resp = self
            .http
            .client
            .post(upload_url)
            .headers(auth_only.clone())
            .multipart(form)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !upload_resp.status().is_success() {
            let status = upload_resp.status();
            let h = upload_resp.headers().clone();
            let text = upload_resp.text().await.unwrap_or_default();
            return Err(self.adapter.classify_error(status, &h, &text));
        }
        let file_v: Value = upload_resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("file upload: response not JSON: {e}")))?;
        let input_file_id = file_v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ProviderError::Parse("file upload: response missing `id`".into())
            })?
            .to_string();

        // 3) Create the batch referencing that file.
        let create_url = self.adapter.batches_url("")?;
        let headers = self.adapter.build_headers(&auth)?; // JSON content-type
        let body = json!({
            "input_file_id": input_file_id,
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h",
        });
        let resp = self
            .http
            .client
            .post(create_url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.adapter.classify_error(status, &h, &text));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("batch create: response not JSON: {e}")))?;
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Parse("batch create: response missing `id`".into()))?;
        Ok(BatchJobId::new(id))
    }

    async fn status(&self, id: &BatchJobId) -> Result<BatchStatus, ProviderError> {
        let v = self.fetch_batch_object(id).await?;
        translate_openai_batch_status(&v)
    }

    async fn results(
        &self,
        id: &BatchJobId,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        let v = self.fetch_batch_object(id).await?;
        let status = translate_openai_batch_status(&v)?;
        if !status.is_terminal() {
            return Err(ProviderError::InvalidRequest(format!(
                "batch results: job {id} is not yet terminal (status: {status:?})"
            )));
        }
        // For Completed: read output_file_id and download it. For
        // Failed/Expired/Cancelled there's typically no output file
        // (errors live in error_file_id which we currently surface as
        // an Err on each item-less response). Return empty for now;
        // callers should branch on status() before results().
        let output_file_id = v.get("output_file_id").and_then(|s| s.as_str());
        let Some(output_file_id) = output_file_id else {
            return Ok(Vec::new());
        };

        let auth = self
            .auth_resolver
            .resolve(&self.auth, &RequestContext::test_default())
            .await?;
        let headers = openai_auth_only_headers(&auth)?;
        let url = self
            .adapter
            .files_url(&format!("/{output_file_id}/content"))?;
        let resp = self
            .http
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.adapter.classify_error(status, &h, &text));
        }
        let text = resp.text().await.map_err(ProviderError::from)?;
        parse_openai_batch_results(&text)
    }

    async fn cancel(&self, id: &BatchJobId) -> Result<(), ProviderError> {
        let auth = self
            .auth_resolver
            .resolve(&self.auth, &RequestContext::test_default())
            .await?;
        let headers = self.adapter.build_headers(&auth)?;
        let url = self
            .adapter
            .batches_url(&format!("/{}/cancel", id.as_str()))?;
        let resp = self
            .http
            .client
            .post(url)
            .headers(headers)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.adapter.classify_error(status, &h, &text));
        }
        Ok(())
    }
}

impl OpenAiProvider {
    /// Shared GET that pulls the batch object JSON for status / results.
    async fn fetch_batch_object(&self, id: &BatchJobId) -> Result<Value, ProviderError> {
        let auth = self
            .auth_resolver
            .resolve(&self.auth, &RequestContext::test_default())
            .await?;
        let headers = self.adapter.build_headers(&auth)?;
        let url = self.adapter.batches_url(&format!("/{}", id.as_str()))?;
        let resp = self
            .http
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.adapter.classify_error(status, &h, &text));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| ProviderError::Parse(format!("batch fetch: response not JSON: {e}")))
    }
}

/// Authorization header only (no `Content-Type: application/json`) —
/// reqwest sets the right multipart `Content-Type` automatically; we
/// must not preset JSON or the boundary string gets clobbered.
fn openai_auth_only_headers(auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
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
fn translate_openai_batch_status(v: &Value) -> Result<BatchStatus, ProviderError> {
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
            // OpenAI surfaces the top-level reason via the `errors` array.
            // We collapse to one-message Failed.
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
fn parse_openai_batch_results(text: &str) -> Result<Vec<BatchResultItem>, ProviderError> {
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

/// The wire-format adapter — pure functions, no state.
pub struct OpenAiAdapter {
    base_url: String,
    extras: HttpProviderExtras,
}

impl OpenAiAdapter {
    /// Decide which "max tokens" parameter the model accepts.
    fn max_tokens_field(model: &str) -> &'static str {
        if NEW_TOKENS_PARAM_PREFIXES
            .iter()
            .any(|p| model.starts_with(p))
        {
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
            Message::Assistant {
                content,
                tool_calls,
            } => {
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
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
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

impl OpenAiAdapter {
    /// Build a URL under `{base_url}/files{suffix}` (file upload + download).
    pub(crate) fn files_url(&self, suffix: &str) -> Result<Url, ProviderError> {
        let trimmed = self.base_url.trim_end_matches('/');
        Url::parse(&format!("{trimmed}/files{suffix}"))
            .map_err(|e| ProviderError::Internal(format!("bad openai files url: {e}")))
    }

    /// Build a URL under `{base_url}/batches{suffix}`. `""` is the
    /// collection (POST create), `/{id}` is one job, `/{id}/cancel`
    /// is the cancel sub-resource.
    pub(crate) fn batches_url(&self, suffix: &str) -> Result<Url, ProviderError> {
        let trimmed = self.base_url.trim_end_matches('/');
        Url::parse(&format!("{trimmed}/batches{suffix}"))
            .map_err(|e| ProviderError::Internal(format!("bad openai batches url: {e}")))
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

        // Per-request thinking control via the OpenAI-compat
        // `chat_template_kwargs` field. mlx_lm.server / vLLM / Qwen3
        // chat templates accept `enable_thinking: bool` here; OpenAI
        // proper ignores unknown body fields, so this is harmless to
        // send on the standard endpoint. Only emit when the caller
        // explicitly set the override — `None` means "no preference,
        // let the server's chat-template default decide."
        if let Some(enable) = req.enable_chat_template_thinking {
            body["chat_template_kwargs"] = json!({"enable_thinking": enable});
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
            ProviderError::Parse(format!(
                "openai sse json: {e} (raw: {})",
                truncate(&raw.data, 200)
            ))
        })?;

        let mut out = Vec::new();

        // Usage chunk (final, when stream_options.include_usage=true).
        // Per spec it has `choices: []`, so we check usage independently.
        if let Some(usage) = v.get("usage").and_then(|u| u.as_object()) {
            // Defer emission until we also know the model + stop_reason
            // (handled in the choices block). But if there are no
            // choices in this chunk, emit Finished with what we have.
            let choices_empty = v
                .get("choices")
                .and_then(|c| c.as_array())
                .is_none_or(|a| a.is_empty());
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

        let model = v
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

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
            // Reasoning channel — emitted as a separate field by o1 /
            // DeepSeek-R1 / Qwen3-thinking / many LM Studio models.
            // Surface as ThinkingDelta so consumers can route it to
            // a separate display channel and our Usage's
            // thinking_tokens accumulator picks it up. Ordering: most
            // models emit reasoning BEFORE content per chunk, so emit
            // ThinkingDelta first when both are present.
            //
            // Two field names in the wild: `reasoning_content`
            // (OpenAI spec / DeepSeek-R1 / many LM Studio models) and
            // `reasoning` (mlx_lm.server / some Qwen-thinking variants).
            // Accept both; prefer the spec name when present.
            let reasoning = delta
                .get("reasoning_content")
                .and_then(|r| r.as_str())
                .or_else(|| delta.get("reasoning").and_then(|r| r.as_str()));
            if let Some(text) = reasoning
                && !text.is_empty()
            {
                out.push(ChatEvent::ThinkingDelta {
                    text: text.to_string(),
                });
            }
            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    out.push(ChatEvent::Delta {
                        text: content.to_string(),
                    });
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
                        // Fall back to the array position if `index` is
                        // missing OR doesn't fit `usize` (32-bit targets):
                        // a silent `as usize` truncation could collapse
                        // two distinct parallel tool calls into one slot.
                        .and_then(|i| usize::try_from(i).ok())
                        .unwrap_or(iter_pos);
                    let id = tc
                        .get("id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
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
                    ProviderError::ContextTooLong {
                        limit: 0,
                        requested: 0,
                    }
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
fn drain_buffer_into(
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
        assert_eq!(
            OpenAiAdapter::max_tokens_field("o1-preview"),
            "max_completion_tokens"
        );
        assert_eq!(
            OpenAiAdapter::max_tokens_field("gpt-5-something"),
            "max_completion_tokens"
        );
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
            enable_chat_template_thinking: None,
        };
        let body = a.translate_request(&req).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let system_count = messages.iter().filter(|m| m["role"] == "system").count();
        assert_eq!(system_count, 1, "should dedupe to one system message");
        assert_eq!(messages[0]["content"][0]["text"], "explicit system");
    }

    fn empty_headers() -> reqwest::header::HeaderMap {
        reqwest::header::HeaderMap::new()
    }

    #[test]
    fn classify_401_is_auth() {
        let a = OpenAiAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            extras: HttpProviderExtras::default(),
        };
        let err = a.classify_error(
            StatusCode::UNAUTHORIZED,
            &empty_headers(),
            "{\"error\":{\"message\":\"bad\"}}",
        );
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let a = OpenAiAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            extras: HttpProviderExtras::default(),
        };
        let err = a.classify_error(StatusCode::TOO_MANY_REQUESTS, &empty_headers(), "");
        assert!(matches!(
            err,
            ProviderError::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn classify_429_with_retry_after_seconds_populates_field() {
        let a = OpenAiAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            extras: HttpProviderExtras::default(),
        };
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
        let a = OpenAiAdapter {
            base_url: DEFAULT_BASE_URL.into(),
            extras: HttpProviderExtras::default(),
        };
        let body = r#"{"error":{"message":"context_length_exceeded: too many tokens"}}"#;
        let err = a.classify_error(StatusCode::BAD_REQUEST, &empty_headers(), body);
        assert!(matches!(err, ProviderError::ContextTooLong { .. }));
    }
}
