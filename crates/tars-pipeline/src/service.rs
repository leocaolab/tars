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
        // Telemetry: this is the innermost layer. Record it AND wrap
        // the stream to time the actual provider work (HTTP open +
        // SSE drain). When called from inside RetryMiddleware, the
        // outer call may invoke us multiple times — `provider_latency_ms`
        // accumulates across attempts so it reflects total provider
        // wall time across the whole call.
        // Recover from a poisoned lock rather than silently skipping:
        // the provider id stamped here is what the outer event-emitter
        // reads back to attribute the call, so dropping it on poison
        // would corrupt routing diagnostics. A poison only flags that
        // some other holder panicked; the data is still sound.
        {
            let mut t = match ctx.telemetry.lock() {
                Ok(t) => t,
                Err(poisoned) => {
                    tracing::warn!(
                        "provider_service: telemetry mutex poisoned; recovering to record provider metadata",
                    );
                    poisoned.into_inner()
                }
            };
            t.layers.push("provider".into());
            // Stamp the provider id so outer middleware (event emitter)
            // can record which provider actually ran post-routing.
            t.provider_id = Some(self.provider.id().as_ref().to_string());
        }
        let started = std::time::Instant::now();
        let telemetry = ctx.telemetry.clone();

        // `LlmProvider::stream` also takes `Arc<Self>`; clone the
        // inner Arc rather than dereffing through `&self`.
        let inner = self.provider.clone().stream(req, ctx).await?;

        // Wrap the stream so we time-stamp end-of-stream into telemetry.
        // Latency is recorded by a Drop guard rather than code after the
        // `while` loop: if the consumer drops the stream early (client
        // disconnect, `take(n)`, timeout) the post-loop tail would never
        // run and `provider_latency_ms` would silently never be recorded
        // for that attempt. The guard fires on *both* normal completion
        // and early drop, so the latency reflects actual provider wall
        // time regardless of how consumption ended.
        let observed = async_stream::stream! {
            use futures::StreamExt;
            let _latency_guard = ProviderLatencyGuard { started, telemetry };
            let mut s = inner;
            while let Some(ev) = s.next().await {
                yield ev;
            }
        };
        Ok(Box::pin(observed))
    }
}

/// Records accumulated provider wall-time into the shared telemetry on
/// drop. Living in a guard (rather than code after the stream loop)
/// means the latency is captured whether the consumer drains the stream
/// to completion or drops it early — the post-loop tail of an
/// `async_stream` never runs on early drop.
struct ProviderLatencyGuard {
    started: std::time::Instant,
    telemetry: tars_types::SharedTelemetry,
}

impl Drop for ProviderLatencyGuard {
    fn drop(&mut self) {
        // `as_millis()` is u128; saturate rather than silently wrapping
        // for a pathological duration.
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Recover from a poisoned lock (warn, don't silently drop) so
        // retry-loop latency totals aren't lost when an unrelated holder
        // panicked.
        let mut t = match self.telemetry.lock() {
            Ok(t) => t,
            Err(poisoned) => {
                tracing::warn!(
                    "provider_service: telemetry mutex poisoned; recovering to record provider latency",
                );
                poisoned.into_inner()
            }
        };
        let prev = t.provider_latency_ms.unwrap_or(0);
        t.provider_latency_ms = Some(prev.saturating_add(elapsed));
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
            let ev = ev.expect("stream yielded a provider error");
            if matches!(ev, ChatEvent::Finished { .. }) {
                got_finished = true;
            }
        }
        assert!(got_finished);
    }
}
