//! `OpenAiProvider`, its builder, the `LlmProvider` impl, and the
//! `BatchSubmitter` implementation (OpenAI Batch API). The default
//! capability descriptor lives here too — it's only consumed by the
//! builder fallback, so colocating it avoids a one-item module.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use tars_types::{
    BatchItemId, BatchJobId, BatchResultItem, BatchStatus, Capabilities, ChatRequest,
    Modality, ProviderError, ProviderId, RequestContext, StructuredOutputMode,
};

use crate::auth::{Auth, AuthResolver};
use crate::batch::BatchSubmitter;
use crate::http_base::{HttpAdapter, HttpProviderBase, HttpProviderExtras, stream_via_adapter};
use crate::provider::{LlmEventStream, LlmProvider};

use super::adapter::OpenAiAdapter;
use super::mapping::{
    openai_auth_only_headers, parse_openai_batch_results, translate_openai_batch_status,
};

/// Default OpenAI base URL.
pub(super) const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

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

    builder_setter! {
        /// Override base URL — for vLLM / llama.cpp / Groq / etc.
        base_url: into String
    }

    builder_setter! {
        /// Override capability descriptor. Default is a vanilla
        /// GPT-4o-style profile; OpenAI-compatible local backends
        /// should set their own.
        capabilities: opt Capabilities
    }

    builder_setter! {
        /// Attach user-config-supplied http_headers / env_http_headers /
        /// query_params (Doc 01 §6.1).
        extras: HttpProviderExtras
    }

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<OpenAiProvider> {
        let caps = self
            .capabilities
            .unwrap_or_else(default_openai_capabilities);
        let adapter = Arc::new(OpenAiAdapter::new(self.base_url, self.extras));
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
