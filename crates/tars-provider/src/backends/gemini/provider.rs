//! Provider lifecycle for the Gemini backend: builder, `LlmProvider`
//! impl (delegates the streaming path to the HTTP base), and the
//! batch-submitter surface (currently a typed "unsupported" stub).
//! The protocol translation lives in [`super::adapter`].

use std::sync::Arc;

use async_trait::async_trait;

use tars_types::{
    BatchItemId, BatchJobId, BatchResultItem, BatchStatus, Capabilities, ChatRequest, Modality,
    PromptCacheKind, ProviderError, ProviderId, RequestContext, StructuredOutputMode,
};

use crate::auth::{Auth, AuthResolver, ResolvedAuth};
use crate::batch::BatchSubmitter;
use crate::http_base::{HttpProviderBase, HttpProviderExtras, stream_via_adapter};
use crate::provider::{LlmEventStream, LlmProvider};

use super::adapter::{GeminiAdapter, GeminiAdapterWithKey, ResolvedAuthWithKey};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

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

    builder_setter!(base_url: into String);
    builder_setter!(capabilities: opt Capabilities);
    builder_setter!(extras: HttpProviderExtras);

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
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
                    return Err(ProviderError::Auth("Gemini API key is empty".into()));
                }
                ResolvedAuthWithKey::Key(k)
            }
            ResolvedAuth::Bearer(_) => {
                return Err(ProviderError::Auth(
                    "Gemini bearer auth (Vertex AI ADC) is not yet supported; use an API key"
                        .into(),
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
        stream_via_adapter(
            self.http.clone(),
            adapter_with_key,
            ResolvedAuth::None,
            req,
            ctx,
        )
        .await
    }

    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn BatchSubmitter>> {
        // We expose the surface so callers can `provider.as_batch_submitter()`
        // and pattern-match uniformly with Anthropic / OpenAI; the impl
        // itself returns `InvalidRequest` to signal "configured but
        // unsupported." See the BatchSubmitter impl below.
        Some(self)
    }
}

// ─── BatchSubmitter — Gemini (NOT YET SUPPORTED) ────────────────────
//
// Gemini's batch API path is **fundamentally different** from
// Anthropic / OpenAI:
//
// - The public GenAI API (`generativelanguage.googleapis.com`) batch
//   endpoint uses Long-Running Operations (LRO) — resource names like
//   `batches/abc`, polling via `operations` resource — not a direct
//   status field like Anthropic / OpenAI.
//
// - Vertex AI Batch Prediction is a separate product on
//   `aiplatform.googleapis.com` that requires service-account auth +
//   a Google Cloud Storage bucket for input/output files. This tars
//   backend uses API-key auth against the GenAI API path and **does
//   not support Vertex AI**.
//
// Rather than ship a wrong-shape stub or fake a half-working impl,
// each method returns a typed `InvalidRequest` with a stable message
// so callers can pattern-match on the surface AND know batch is not
// usable on this provider yet.
//
// Tracking: `docs/roadmap.md §5 Phase 4`. Re-opening this requires
// pinning the GenAI batch API spec (its shape has shifted as the API
// has matured) — kept deferred until a contributor has time to do
// that work end-to-end.

const GEMINI_BATCH_NOT_SUPPORTED: &str =
    "Gemini batch is not yet implemented in this tars backend. \
     Tracked at docs/roadmap.md §5 Phase 4. \
     (Vertex AI Batch Prediction requires a different auth path and is out of scope.)";

#[async_trait]
impl BatchSubmitter for GeminiProvider {
    async fn submit(
        &self,
        _items: Vec<(BatchItemId, ChatRequest)>,
        _ctx: &RequestContext,
    ) -> Result<BatchJobId, ProviderError> {
        Err(ProviderError::InvalidRequest(GEMINI_BATCH_NOT_SUPPORTED.into()))
    }

    async fn status(
        &self,
        _id: &BatchJobId,
        _ctx: &RequestContext,
    ) -> Result<BatchStatus, ProviderError> {
        Err(ProviderError::InvalidRequest(GEMINI_BATCH_NOT_SUPPORTED.into()))
    }

    async fn results(
        &self,
        _id: &BatchJobId,
        _ctx: &RequestContext,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        Err(ProviderError::InvalidRequest(GEMINI_BATCH_NOT_SUPPORTED.into()))
    }

    async fn cancel(
        &self,
        _id: &BatchJobId,
        _ctx: &RequestContext,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::InvalidRequest(GEMINI_BATCH_NOT_SUPPORTED.into()))
    }
}
