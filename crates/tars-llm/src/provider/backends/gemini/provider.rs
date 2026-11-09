//! Builds and runs the Gemini provider. Its batch methods all fail with `InvalidRequest`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{BatchResultItem, BatchStatus, ProviderError};
use tars_types::{BatchItemId, BatchJobId, Caller, ChatRequest, ProviderId, ProviderProfile};

use crate::provider::auth::{Auth, AuthResolver};
use crate::provider::batch::BatchSubmitter;
use crate::provider::error::{Feature, RequestRefusal};
use crate::provider::http_base::{HttpClient, HttpOverrides, stream_via_adapter};
use crate::provider::wire::WireRecorder;
use crate::provider::{LlmEventStream, LlmProvider};

use super::adapter::GeminiAdapter;
use super::map_restore::{EncodedMaps, restore_in_stream};

pub(crate) const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

#[derive(Clone, Debug)]
pub struct GeminiProviderBuilder {
    id: ProviderId,
    base_url: String,
    auth: Auth,
    capabilities: Option<ProviderProfile>,
    extras: HttpOverrides,
    output_limits: tars_config::OutputLimitRule,
    wire: Option<WireRecorder>,
}

impl GeminiProviderBuilder {
    pub fn new(id: ProviderId, auth: Auth) -> Self {
        Self {
            id,
            base_url: DEFAULT_BASE_URL.to_string(),
            auth,
            capabilities: None,
            extras: HttpOverrides::default(),
            output_limits: tars_config::OutputLimitRule::catalog("gemini"),
            wire: None,
        }
    }

    builder_setter!(base_url: into String);
    builder_setter!(capabilities: opt ProviderProfile);
    builder_setter!(extras: HttpOverrides);
    builder_setter!(output_limits: tars_config::OutputLimitRule);
    builder_setter! {
        /// Sets where each attempt's raw bytes are recorded; `None` records none.
        wire: Option<WireRecorder>
    }

    pub fn build(
        self,
        http: Arc<HttpClient>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<GeminiProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let adapter = Arc::new(GeminiAdapter::new(self.base_url, self.extras));
        Arc::new(GeminiProvider {
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

fn default_capabilities() -> ProviderProfile {
    tars_config::capabilities_for("gemini", "")
}

pub struct GeminiProvider {
    id: ProviderId,
    http: Arc<HttpClient>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<GeminiAdapter>,
    capabilities: ProviderProfile,
    output_limits: tars_config::OutputLimitRule,
    wire: Option<WireRecorder>,
}

#[async_trait]
impl LlmProvider for GeminiProvider {
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
        name = "gemini.stream",
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
        check_cache_handles(&req, &ctx.tenant_id)?;
        let maps = EncodedMaps::of(&req)?;
        let auth = self.auth_resolver.resolve(&self.auth, &ctx).await?;
        let events = stream_via_adapter(
            self.http.clone(),
            self.adapter.clone(),
            auth,
            req,
            model,
            ctx,
            &self.id,
            self.wire.as_ref(),
        )
        .await?;
        Ok(restore_in_stream(events, maps))
    }

    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn BatchSubmitter>> {
        // Present, so a caller learns from the error that batches are unsupported here.
        Some(self)
    }
}

// Gemini batches are not implemented. The GenAI API runs a batch as a long-running operation
// (`batches/abc`, polled through `operations`), not a job with a status field; Vertex AI Batch
// Prediction needs service-account auth and a Cloud Storage bucket, which this API-key backend
// does not have.

fn batch_unsupported() -> ProviderError {
    ProviderError::InvalidRequest(RequestRefusal::Unsupported(Feature::Batch))
}

#[async_trait]
impl BatchSubmitter for GeminiProvider {
    async fn submit(
        &self,
        _items: Vec<(BatchItemId, ChatRequest)>,
        _model: &str,
        _ctx: &Caller,
    ) -> Result<BatchJobId, ProviderError> {
        Err(batch_unsupported())
    }

    async fn status(&self, _id: &BatchJobId, _ctx: &Caller) -> Result<BatchStatus, ProviderError> {
        Err(batch_unsupported())
    }

    async fn results(
        &self,
        _id: &BatchJobId,
        _ctx: &Caller,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        Err(batch_unsupported())
    }

    async fn cancel(&self, _id: &BatchJobId, _ctx: &Caller) -> Result<(), ProviderError> {
        Err(batch_unsupported())
    }
}

/// Checks every cached-content handle in `req` against `tenant` before its id is sent; a handle
/// of another tenant would read that tenant's cached prompt. It reads the system clock, since an
/// expired handle is refused too.
fn check_cache_handles(
    req: &ChatRequest,
    tenant: &tars_types::TenantId,
) -> Result<(), ProviderError> {
    for d in &req.cache_directives {
        if let tars_types::CacheDirective::UseExplicit { handle } = d {
            handle
                .validate_for_use(tenant)
                .map_err(|e| ProviderError::InvalidRequest(RequestRefusal::CacheHandle(e)))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod cache_handle_tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use tars_types::{CacheDirective, ProviderCacheHandle, ProviderId, TenantId};

    fn with_handle(owner: &str) -> ChatRequest {
        let mut req = ChatRequest::user("hi");
        req.cache_directives.push(CacheDirective::UseExplicit {
            handle: ProviderCacheHandle {
                provider: ProviderId::new("gemini"),
                external_id: "cachedContents/abc".into(),
                tenant_namespace: owner.parse::<TenantId>().unwrap(),
                created_at: SystemTime::now(),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
                size_estimate_bytes: None,
            },
        });
        req
    }

    #[test]
    fn a_cached_content_handle_of_another_tenant_is_refused() {
        let err = check_cache_handles(&with_handle("acme"), &TenantId::new("globex")).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)), "{err:?}");
        check_cache_handles(&with_handle("acme"), &TenantId::new("acme")).unwrap();
    }
}
