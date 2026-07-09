//! [`LlmService`] — the concrete driver that runs a request through a
//! handler-chain of [`Middleware`] and, at the bottom, streams from a
//! single bound [`tars_provider::LlmProvider`].
//!
//! `LlmService` is intentionally narrow: it binds one `provider + model`
//! and a fixed, ordered stack of middleware `layers`. A call walks the
//! layers outer→inner via [`Next`]; the innermost step is the terminal
//! `provider.stream(req, model, ctx)`. Provider-level concerns (`id`,
//! `capabilities`, `count_tokens`, `cost`) belong to the *Provider* and
//! don't leak into pipeline composition.
//!
//! Provider *selection* (routing / ensemble / provider-fallback) is NOT
//! a pipeline concern: a caller who wants it composes multiple
//! `LlmService`s themselves (build N services, call and merge / try in
//! order). The pipeline's single primitive is "one provider + model,
//! wrapped in a middleware chain".

use std::sync::Arc;

use tars_provider::{LlmEventStream, LlmProvider};
use tars_types::{ChatRequest, ProviderError, RequestContext};

use crate::middleware::Middleware;

/// The one public, concrete "callable LLM". Business code holds an
/// [`LlmService`] and calls [`LlmService::call`] — it is **model-blind**:
/// the concrete model is bound here at construction (`provider + model`),
/// never carried on the [`ChatRequest`].
///
/// A call runs the `layers` outer→inner as a handler-chain (see
/// [`Next`]) and finishes at the terminal `provider.stream(...)`.
/// Cheap to clone (Arcs + a small Vec of Arcs).
#[derive(Clone)]
pub struct LlmService {
    /// The single provider this service streams from at the bottom of
    /// the chain.
    provider: Arc<dyn LlmProvider>,
    /// Concrete model this service is bound to. Threaded through the
    /// middleware chain as an explicit argument (never on the request or
    /// the context) and handed to `provider.stream` at the terminal.
    model: String,
    /// Middleware layers, **outermost-first**. `layers[0]` runs first on
    /// the inbound and last on the outbound; the terminal provider call
    /// sits below `layers.last()`.
    layers: Vec<Arc<dyn Middleware>>,
    /// Outermost-first middleware layer names, for diagnostics. Derived
    /// from `layers` at construction so [`Self::layer_names`] can keep
    /// returning a borrowed slice.
    layer_names: Arc<[&'static str]>,
}

impl LlmService {
    /// The leaf service: bind a concrete `model` to a `provider` with no
    /// middleware. A call goes straight to `provider.stream(req, model, ctx)`.
    pub fn of(provider: Arc<dyn LlmProvider>, model: impl Into<String>) -> LlmService {
        LlmService::compose(provider, model, Vec::new())
    }

    /// Issue one streaming call. Public signature is model-free: the
    /// model bound into this service is threaded through the middleware
    /// chain as an explicit argument (never on the request or the ctx).
    pub async fn call(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let next = Next {
            layers: &self.layers,
            provider: &self.provider,
            model: &self.model,
        };
        next.run(req, ctx).await
    }

    /// Outermost-first list of layer names. `["telemetry", "retry"]`
    /// means a request hits Telemetry first, then Retry, then the
    /// terminal provider.
    pub fn layer_names(&self) -> &[&'static str] {
        &self.layer_names
    }

    /// Concrete model this service is bound to.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The provider bound at the bottom of this service's chain.
    pub fn provider(&self) -> &Arc<dyn LlmProvider> {
        &self.provider
    }

    /// Split into `(provider, model, layers)`. Crate-internal: lets the
    /// builder re-seat an already-built service as the inner stack under
    /// additional outer layers ([`crate::LlmService::builder_with_inner`]).
    pub(crate) fn into_parts(self) -> (Arc<dyn LlmProvider>, String, Vec<Arc<dyn Middleware>>) {
        (self.provider, self.model, self.layers)
    }

    /// Assemble from a provider + bound model + an outer→inner layer
    /// stack. Crate-internal: the [`crate::LlmServiceBuilder`] produces the
    /// layer stack and hands it here.
    pub(crate) fn compose(
        provider: Arc<dyn LlmProvider>,
        model: impl Into<String>,
        layers: Vec<Arc<dyn Middleware>>,
    ) -> LlmService {
        let layer_names: Arc<[&'static str]> = layers.iter().map(|l| l.name()).collect();
        LlmService {
            provider,
            model: model.into(),
            layers,
            layer_names,
        }
    }
}

/// Cursor over an [`LlmService`]'s remaining middleware chain. A layer's
/// [`Middleware::handle`] receives a `Next`; calling [`Next::run`]
/// advances to the next layer (or, once the layers are exhausted, to the
/// terminal `provider.stream(...)`).
///
/// `Next` is a triple of shared references (the remaining layer slice,
/// the bound provider, and the bound model), so it is `Copy`: a
/// middleware that needs to invoke the rest of the chain more than once
/// (retry) or zero times (cache hit) simply calls — or doesn't call —
/// `run` as needed.
///
/// The **model** rides on the cursor rather than the call signature: it
/// belongs to the `LlmService` (`provider + model`), and only some
/// middleware need it (cache key, per-model pricing, telemetry/record
/// labels). Those query [`Next::model`]; the rest never see it.
#[derive(Clone, Copy)]
pub struct Next<'a> {
    layers: &'a [Arc<dyn Middleware>],
    provider: &'a Arc<dyn LlmProvider>,
    model: &'a str,
}

impl<'a> Next<'a> {
    /// The concrete model the enclosing [`LlmService`] is bound to.
    /// Middleware that key on the model (cache), price per model (budget),
    /// or label it (telemetry / event emitter) read it here.
    ///
    /// Returns a `&'a str` (tied to the chain, not to the borrow of
    /// `self`), so a layer can bind `let model = next.model();` and still
    /// call `next.run(...)` afterwards.
    pub fn model(&self) -> &'a str {
        self.model
    }

    /// Advance the chain by one step. If a layer remains, run it with a
    /// `Next` over the rest; otherwise call the terminal provider with the
    /// bound model.
    pub async fn run(
        self,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        match self.layers.split_first() {
            Some((head, tail)) => {
                let next = Next {
                    layers: tail,
                    provider: self.provider,
                    model: self.model,
                };
                head.handle(req, ctx, next).await
            }
            None => stream_from_provider(self.provider, req, self.model, ctx).await,
        }
    }
}

/// The terminal of the chain: stream from the bound provider, stamping
/// the provider-level telemetry the outer layers read back.
///
/// This is the innermost layer. Record it AND wrap the stream to time
/// the actual provider work (HTTP open + SSE drain). When called from
/// inside `RetryMiddleware` the outer call may invoke us multiple times —
/// `provider_latency_ms` accumulates across attempts so it reflects total
/// provider wall time across the whole call.
async fn stream_from_provider(
    provider: &Arc<dyn LlmProvider>,
    req: ChatRequest,
    model: &str,
    ctx: RequestContext,
) -> Result<LlmEventStream, ProviderError> {
    // Recover from a poisoned lock rather than silently skipping: the
    // provider id stamped here is what the outer event-emitter reads back
    // to attribute the call, so dropping it on poison would corrupt
    // routing diagnostics. A poison only flags that some other holder
    // panicked; the data is still sound.
    {
        let mut t = match ctx.telemetry.lock() {
            Ok(t) => t,
            Err(poisoned) => {
                tracing::warn!(
                    "provider terminal: telemetry mutex poisoned; recovering to record provider metadata",
                );
                poisoned.into_inner()
            }
        };
        t.layers.push("provider".into());
        // Stamp the provider id so outer middleware (event emitter) can
        // record which provider actually ran.
        t.provider_id = Some(provider.id().as_ref().to_string());
    }
    let started = std::time::Instant::now();
    let telemetry = ctx.telemetry.clone();

    // `LlmProvider::stream` takes `Arc<Self>`; clone the provider Arc. The
    // model arrives as an explicit argument — the request is model-agnostic
    // content, the ctx carries no model.
    let inner = provider.clone().stream(req, model, ctx).await?;

    // Wrap the stream so we time-stamp end-of-stream into telemetry via a
    // Drop guard: if the consumer drops the stream early (client
    // disconnect, `take(n)`, timeout) a post-loop tail would never run and
    // `provider_latency_ms` would silently never be recorded. The guard
    // fires on *both* normal completion and early drop.
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

/// Records accumulated provider wall-time into the shared telemetry on
/// drop. Living in a guard (rather than code after the stream loop) means
/// the latency is captured whether the consumer drains the stream to
/// completion or drops it early — the post-loop tail of an `async_stream`
/// never runs on early drop.
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
                    "provider terminal: telemetry mutex poisoned; recovering to record provider latency",
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
    use crate::LlmService;
    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::ChatEvent;

    #[tokio::test]
    async fn empty_service_passes_through_to_provider() {
        let mock = MockProvider::new("svc_test", CannedResponse::text("ok"));
        let svc = LlmService::of(mock, "mock-1");

        let req = ChatRequest::user("ping");
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
