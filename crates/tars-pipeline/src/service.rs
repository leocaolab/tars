//! [`LlmService`] trait — the unit each [`super::Middleware`] wraps.
//!
//! `LlmService` is intentionally narrower than [`tars_provider::LlmProvider`]:
//! it exposes only the streaming call. Provider-level concerns (`id`,
//! `capabilities`, `count_tokens`, `cost`) belong to the *Provider*
//! and shouldn't leak into pipeline composition. The
//! [`ProviderService`] adapter is the canonical bottom-of-pipeline
//! impl; routing/fallback (Doc 02 §4.6) will introduce alternatives
//! that pick a provider at call time.

use std::sync::Arc;

use async_trait::async_trait;

use tars_provider::{LlmEventStream, LlmProvider};
use tars_types::{ChatRequest, ProviderError, RequestContext};

/// One node in the middleware onion. Same return type as
/// [`tars_provider::LlmProvider::stream`] so adapters can swap in
/// trivially.
///
/// The `Arc<Self>` receiver mirrors `LlmProvider`: a returned stream
/// is `'static`, so it can't borrow from `&self`. Callers wrap services
/// in `Arc` once at construction and clone-as-needed.
#[async_trait]
pub trait LlmService: Send + Sync + 'static {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError>;
}

/// Adapter — wrap any [`LlmProvider`] as the innermost [`LlmService`].
///
/// Stateless beyond the inner Arc; cheap to clone via `Arc::clone`.
pub struct ProviderService {
    provider: Arc<dyn LlmProvider>,
}

impl ProviderService {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Arc<Self> {
        Arc::new(Self { provider })
    }
}

#[async_trait]
impl LlmService for ProviderService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // `LlmProvider::stream` also takes `Arc<Self>`; clone the
        // inner Arc rather than dereffing through `&self`.
        self.provider.clone().stream(req, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ChatEvent, ModelHint};

    #[tokio::test]
    async fn provider_service_passes_through_to_inner_provider() {
        let mock = MockProvider::new("svc_test", CannedResponse::text("ok"));
        let svc: Arc<dyn LlmService> = ProviderService::new(mock);

        let req = ChatRequest::user(ModelHint::Explicit("mock-1".into()), "ping");
        let mut stream = svc.call(req, RequestContext::test_default()).await.unwrap();

        let mut got_finished = false;
        while let Some(ev) = stream.next().await {
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                got_finished = true;
            }
        }
        assert!(got_finished);
    }
}
