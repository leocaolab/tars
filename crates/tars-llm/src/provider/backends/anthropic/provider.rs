//! Builds and runs the Anthropic provider: chat over HTTP, plus the Message Batches API.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::pipeline::middleware::OutputReserve;
use crate::{BatchResultItem, BatchStatus, ProviderError};
use tars_types::{BatchItemId, BatchJobId, Caller, ChatRequest, ProviderId, ProviderProfile};

use crate::provider::auth::{Auth, AuthResolver};
use crate::provider::batch::BatchSubmitter;
use crate::provider::error::{ParseProblem, RequestRefusal};
use crate::provider::http_base::{
    ERROR_BODY_CAP_BYTES, HttpAdapter, HttpClient, HttpOverrides, insert_extra_headers,
    read_bounded_body, stream_via_adapter, truncate_utf8,
};
use crate::provider::wire::WireRecorder;
use crate::provider::{LlmEventStream, LlmProvider};

use super::adapter::AnthropicAdapter;
use super::mapping::{parse_batch_results, translate_batch_status};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub(crate) const DEFAULT_API_VERSION: &str = "2023-06-01";

#[derive(Clone, Debug)]
pub struct AnthropicProviderBuilder {
    id: ProviderId,
    base_url: String,
    api_version: String,
    auth: Auth,
    capabilities: Option<ProviderProfile>,
    extras: HttpOverrides,
    output_limits: tars_config::OutputLimitRule,
    output_reserve: OutputReserve,
    wire: Option<WireRecorder>,
}

impl AnthropicProviderBuilder {
    pub fn new(id: ProviderId, auth: Auth) -> Self {
        Self {
            id,
            base_url: DEFAULT_BASE_URL.to_string(),
            api_version: DEFAULT_API_VERSION.to_string(),
            auth,
            capabilities: None,
            extras: HttpOverrides::default(),
            output_limits: tars_config::OutputLimitRule::catalog("anthropic"),
            output_reserve: OutputReserve::default(),
            wire: None,
        }
    }

    builder_setter!(base_url: into String);
    builder_setter!(api_version: into String);
    builder_setter!(capabilities: opt ProviderProfile);
    builder_setter!(extras: HttpOverrides);
    builder_setter!(output_limits: tars_config::OutputLimitRule);
    builder_setter! {
        /// Sets the `max_tokens` sent for a request that sets no limit.
        output_reserve: OutputReserve
    }
    builder_setter! {
        /// Sets where each attempt's raw bytes are recorded; `None` records none.
        wire: Option<WireRecorder>
    }

    pub fn build(
        self,
        http: Arc<HttpClient>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<AnthropicProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let adapter = Arc::new(AnthropicAdapter::new(
            self.base_url,
            self.api_version,
            self.extras,
            self.output_reserve,
        ));
        Arc::new(AnthropicProvider {
            id: self.id,
            http,
            auth_resolver,
            auth: self.auth,
            adapter,
            capabilities: caps,
            output_limits: self.output_limits,
            wire: self.wire,
        })
    }
}

/// Returns the capabilities of the `anthropic` block of `data/provider.toml`, for a builder given
/// none.
fn default_capabilities() -> ProviderProfile {
    tars_config::capabilities_for("anthropic", "")
}

pub struct AnthropicProvider {
    id: ProviderId,
    http: Arc<HttpClient>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<AnthropicAdapter>,
    capabilities: ProviderProfile,
    output_limits: tars_config::OutputLimitRule,
    wire: Option<WireRecorder>,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }
    fn output_limit(&self, model: &str) -> tars_types::OutputLimit {
        self.output_limits.output_limit(model)
    }
    // `err(Display)` logs each failed call once, with the provider id and model.
    #[tracing::instrument(
        name = "anthropic.stream",
        skip_all,
        fields(provider = %self.id, model = %model),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, &ctx).await?;
        stream_via_adapter(
            self.http.clone(),
            self.adapter.clone(),
            auth,
            req,
            model,
            ctx,
            &self.id,
            self.wire.as_ref(),
        )
        .await
    }

    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn BatchSubmitter>> {
        Some(self)
    }
}

impl AnthropicProvider {
    /// Returns the headers of a Message Batches request: the credential resolved for `ctx`, the
    /// API version, and the config's extra headers over them.
    async fn batch_headers(&self, ctx: &Caller) -> Result<HeaderMap, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let mut headers = self.adapter.build_headers(&auth)?;
        insert_extra_headers(self.adapter.extras(), self.http.env.as_ref(), &mut headers)
            .map_err(ProviderError::ExtraHeader)?;
        Ok(headers)
    }
}

// Anthropic's Message Batches API (<https://docs.anthropic.com/en/api/creating-message-batches>)
// takes every request inline in one body. A batch may take up to 24 h; a failed item shows in
// `results()` while the batch is `Completed`.

#[async_trait]
impl BatchSubmitter for AnthropicProvider {
    async fn submit(
        &self,
        items: Vec<(BatchItemId, ChatRequest)>,
        model: &str,
        ctx: &Caller,
    ) -> Result<BatchJobId, ProviderError> {
        if items.is_empty() {
            return Err(ProviderError::InvalidRequest(RequestRefusal::EmptyBatch));
        }

        // Each item's `params` is the streaming request body, `"stream": true` included.
        let mut requests = Vec::with_capacity(items.len());
        for (item_id, req) in items {
            let params = self.adapter.translate_request(&req, model)?;
            requests.push(json!({
                "custom_id": item_id.as_str(),
                "params": params,
            }));
        }
        let body = json!({ "requests": requests });

        let headers = self.batch_headers(ctx).await?;
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
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            return Err(self.adapter.classify_error(
                status,
                &h,
                truncate_utf8(&body, ERROR_BODY_CAP_BYTES),
            ));
        }

        let unparsed = |problem| ProviderError::Parse {
            at: "anthropic batch submit response",
            line: None,
            problem: Box::new(problem),
            raw: None,
        };
        let v: Value = resp
            .json()
            .await
            .map_err(|e| unparsed(ParseProblem::JsonBody(e)))?;
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| unparsed(ParseProblem::Missing("id")))?;
        id.parse().map_err(|e| unparsed(ParseProblem::JobId(e)))
    }

    async fn status(&self, id: &BatchJobId, ctx: &Caller) -> Result<BatchStatus, ProviderError> {
        let headers = self.batch_headers(ctx).await?;
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
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            return Err(self.adapter.classify_error(
                status,
                &h,
                truncate_utf8(&body, ERROR_BODY_CAP_BYTES),
            ));
        }

        let v: Value = resp.json().await.map_err(|e| ProviderError::Parse {
            at: "anthropic batch status response",
            line: None,
            problem: Box::new(ParseProblem::JsonBody(e)),
            raw: None,
        })?;
        translate_batch_status(&v)
    }

    async fn results(
        &self,
        id: &BatchJobId,
        ctx: &Caller,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        // Anthropic answers 404 for an unfinished batch; checked first, so the error says why.
        let st = self.status(id, ctx).await?;
        if !st.is_terminal() {
            return Err(ProviderError::InvalidRequest(
                RequestRefusal::BatchNotTerminal {
                    id: id.clone(),
                    status: st,
                },
            ));
        }

        let headers = self.batch_headers(ctx).await?;
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
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            return Err(self.adapter.classify_error(
                status,
                &h,
                truncate_utf8(&body, ERROR_BODY_CAP_BYTES),
            ));
        }

        let text = resp.text().await.map_err(ProviderError::from)?;
        parse_batch_results(&text)
    }

    async fn cancel(&self, id: &BatchJobId, ctx: &Caller) -> Result<(), ProviderError> {
        let headers = self.batch_headers(ctx).await?;
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
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            return Err(self.adapter.classify_error(
                status,
                &h,
                truncate_utf8(&body, ERROR_BODY_CAP_BYTES),
            ));
        }
        Ok(())
    }
}
