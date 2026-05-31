//! Provider lifecycle for the Anthropic backend: builder, `LlmProvider`
//! impl (delegates the streaming path to the HTTP base), default
//! capabilities, and the Anthropic Message Batches API `BatchSubmitter`
//! impl. Protocol translation lives in [`super::adapter`]; pure JSON
//! converters in [`super::mapping`].

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use tars_types::{
    BatchItemId, BatchJobId, BatchResultItem, BatchStatus, Capabilities, ChatRequest,
    Modality, PromptCacheKind, ProviderError, ProviderId, RequestContext, StructuredOutputMode,
};

use crate::auth::{Auth, AuthResolver};
use crate::batch::BatchSubmitter;
use crate::http_base::{HttpAdapter, HttpProviderBase, HttpProviderExtras, stream_via_adapter};
use crate::provider::{LlmEventStream, LlmProvider};

use super::adapter::AnthropicAdapter;
use super::mapping::{parse_batch_results, translate_batch_status};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";

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

    builder_setter!(base_url: into String);
    builder_setter!(api_version: into String);
    builder_setter!(capabilities: opt Capabilities);
    builder_setter!(extras: HttpProviderExtras);

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<AnthropicProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let adapter = Arc::new(AnthropicAdapter::new(
            self.base_url,
            self.api_version,
            self.extras,
        ));
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

    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn BatchSubmitter>> {
        Some(self)
    }
}

// ─── BatchSubmitter — Anthropic Message Batches API ────────────────
//
// Reference: <https://docs.anthropic.com/en/api/creating-message-batches>
//
// One-step submission (no separate file upload): the request body
// inlines all items under `requests[]`. Vendor SLAs say up to 24 h,
// usually faster. Pricing is ~50% of sync. Per-item failures surface
// in `results()` while the overall job stays `Completed`.

#[async_trait]
impl BatchSubmitter for AnthropicProvider {
    async fn submit(
        &self,
        items: Vec<(BatchItemId, ChatRequest)>,
        ctx: &RequestContext,
    ) -> Result<BatchJobId, ProviderError> {
        if items.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "batch submit: items list must not be empty".into(),
            ));
        }

        // Reuse the streaming adapter's translate_request to build each
        // line's `params` — same body shape the synchronous endpoint
        // would have accepted.
        let mut requests = Vec::with_capacity(items.len());
        for (item_id, req) in items {
            let params = self.adapter.translate_request(&req)?;
            requests.push(json!({
                "custom_id": item_id.as_str(),
                "params": params,
            }));
        }
        let body = json!({ "requests": requests });

        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let headers = self.adapter.build_headers(&auth)?;
        let url = self.adapter.batch_url("")?;

        let resp = self
            .http
            .client
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(ProviderError::from)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let text = resp.text().await.unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    %status,
                    "anthropic batch: failed to read error response body; \
                     classifying by HTTP status only",
                );
                String::new()
            });
            return Err(self.adapter.classify_error(status, &h, &text));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("batch submit: response not JSON: {e}")))?;
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Parse("batch submit: response missing `id`".into()))?;
        Ok(BatchJobId::new(id))
    }

    async fn status(
        &self,
        id: &BatchJobId,
        ctx: &RequestContext,
    ) -> Result<BatchStatus, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let headers = self.adapter.build_headers(&auth)?;
        let url = self.adapter.batch_url(&format!("/{}", id.as_str()))?;

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
            let text = resp.text().await.unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    %status,
                    "anthropic batch: failed to read error response body; \
                     classifying by HTTP status only",
                );
                String::new()
            });
            return Err(self.adapter.classify_error(status, &h, &text));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("batch status: response not JSON: {e}")))?;
        translate_batch_status(&v)
    }

    async fn results(
        &self,
        id: &BatchJobId,
        ctx: &RequestContext,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        // Anthropic's results endpoint 404s on non-terminal jobs; we
        // pre-check here so the error path is uniform across vendors
        // (see trait doc — `results()` on non-terminal is a caller bug,
        // not a backend error).
        let st = self.status(id, ctx).await?;
        if !st.is_terminal() {
            return Err(ProviderError::InvalidRequest(format!(
                "batch results: job {id} is not yet terminal (status: {st:?})"
            )));
        }

        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let headers = self.adapter.build_headers(&auth)?;
        let url = self
            .adapter
            .batch_url(&format!("/{}/results", id.as_str()))?;

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
            let text = resp.text().await.unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    %status,
                    "anthropic batch: failed to read error response body; \
                     classifying by HTTP status only",
                );
                String::new()
            });
            return Err(self.adapter.classify_error(status, &h, &text));
        }

        let text = resp.text().await.map_err(ProviderError::from)?;
        parse_batch_results(&text)
    }

    async fn cancel(&self, id: &BatchJobId, ctx: &RequestContext) -> Result<(), ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let headers = self.adapter.build_headers(&auth)?;
        let url = self
            .adapter
            .batch_url(&format!("/{}/cancel", id.as_str()))?;

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
            let text = resp.text().await.unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    %status,
                    "anthropic batch: failed to read error response body; \
                     classifying by HTTP status only",
                );
                String::new()
            });
            return Err(self.adapter.classify_error(status, &h, &text));
        }
        Ok(())
    }
}
