//! [`Middleware`] trait + [`Pipeline`] / [`LlmServiceBuilder`].
//!
//! See module-level docs on [`crate`] for the design rationale.

use std::sync::Arc;

use async_trait::async_trait;

use tars_provider::{LlmEventStream, LlmProvider};
use tars_types::{ChatRequest, ProviderError, RequestContext};

use crate::service::{LlmService, Next};

pub(crate) mod budget;
pub(crate) mod cache;
pub(crate) mod circuit_breaker;
pub(crate) mod event_emitter;
pub(crate) mod retry;
pub(crate) mod telemetry;
pub(crate) mod tenant_budget;
pub(crate) mod validation;

/// One node in the middleware handler-chain. A layer does its pre-work,
/// calls [`Next::run`] to invoke the rest of the chain (zero, one, or many
/// times), and does its post-work — the same shape as a `tower::Service`
/// wrapping its inner, but driven by an explicit `next` cursor rather than
/// a stored `inner` handle.
///
/// The concrete public service is [`LlmService`]; users implement this
/// trait and add instances via [`LlmServiceBuilder::layer`]. The **model**
/// is NOT a call argument: it belongs to the `LlmService`
/// (`provider + model`), and the layers that need it query it off the
/// chain cursor via [`Next::model`]. The ones that don't never see it.
#[async_trait]
pub trait Middleware: Send + Sync + 'static {
    /// Stable, low-cardinality label used in tracing spans / metrics.
    fn name(&self) -> &'static str;

    /// Handle one call. Do pre-work, then call `next.run(req, ctx)` to
    /// descend to the next layer (or the terminal provider), then
    /// post-work on the result / stream. Short-circuiting layers (cache
    /// hit, budget reject) may skip `next` entirely; retrying layers may
    /// call it more than once. Layers that key on the bound model read it
    /// from `next.model()`.
    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError>;
}

/// Builder-factory constructors for composing an [`LlmService`]
/// middleware chain. All constructors return the one concrete public
/// service type, [`LlmService`].
impl LlmService {
    /// Start a new builder around a Provider bound to a concrete
    /// `model`. The leaf becomes the innermost service; layers added via
    /// [`LlmServiceBuilder::layer`] wrap it from inside out, with the
    /// **first** added layer ending up outermost.
    pub fn builder(provider: Arc<dyn LlmProvider>, model: impl Into<String>) -> LlmServiceBuilder {
        LlmServiceBuilder {
            provider,
            model: model.into(),
            outer_layers: Vec::new(),
            inner_layers: Vec::new(),
        }
    }

    /// Start a builder from an already-bound [`LlmService`] as the
    /// bottom. The inner service's provider + bound model carry through,
    /// and its existing layers stay innermost; layers added via
    /// [`LlmServiceBuilder::layer`] wrap OUTSIDE them.
    pub fn builder_with_inner(inner: LlmService) -> LlmServiceBuilder {
        let (provider, model, inner_layers) = inner.into_parts();
        LlmServiceBuilder {
            provider,
            model,
            outer_layers: Vec::new(),
            inner_layers,
        }
    }

    /// Assemble the canonical TARS pipeline around `provider`. Onion
    /// (outer → inner):
    ///
    /// ```text
    /// EventEmitter? → Telemetry → Validation? → Cache → Retry → Provider
    /// ```
    ///
    /// `Validation` sits OUTSIDE `Cache` so cache stores raw provider
    /// events and validators run on every call (W4 — see Doc 15 §2 /
    /// Doc 17 §8). `EventEmitter` is outermost when configured so it
    /// sees the complete telemetry + validation_summary picture for
    /// every call.
    ///
    /// This is the Rust-native counterpart of `tars.Pipeline.from_default`
    /// in tars-py — same composition, no Python dependency.
    pub fn default_chain(
        provider: Arc<dyn LlmProvider>,
        model: impl Into<String>,
        opts: ChainOpts,
    ) -> LlmService {
        // CircuitBreaker is a per-provider wrapper (innermost, below
        // Retry): wrap the single provider here so an open breaker
        // rejects before the provider is hit. Routed `chain_over` callers
        // have no single provider to wrap — they wrap each candidate
        // before assembling the routed inner, so the field is theirs to
        // ignore.
        let provider = match &opts.circuit_breaker {
            Some(cfg) => crate::middleware::circuit_breaker::CircuitBreaker::wrap(provider, cfg.clone()),
            None => provider,
        };
        Self::chain_over(LlmService::of(provider, model), opts)
    }

    /// Assemble the canonical onion over an arbitrary bottom
    /// [`LlmService`]. `default_chain` uses this with a single
    /// provider+model leaf; a routed pipeline passes a routed
    /// `LlmService` as `inner` so the same Telemetry / Validation /
    /// Cache / Retry stack sits on top of provider *selection*. Keeps the
    /// layer order in exactly one place.
    pub fn chain_over(inner: LlmService, opts: ChainOpts) -> LlmService {
        let ChainOpts {
            cache_origin,
            validators,
            events,
            cache_registry,
            cache_factory,
            retry,
            // Consumed by `default_chain` (provider-level wrapper); a
            // routed inner service has no single provider to wrap.
            circuit_breaker: _,
            cache,
        } = opts;

        let mut builder = Self::builder_with_inner(inner);

        if let Some(EventStores { events: ev, records }) = events {
            builder = builder.layer(crate::middleware::event_emitter::EventEmitterMiddleware::new(
                ev, records,
            ));
        }
        builder = builder.layer(crate::middleware::telemetry::TelemetryMiddleware::new());

        if !validators.is_empty() {
            builder = builder.layer(crate::middleware::validation::ValidationMiddleware::new(validators));
        }

        if cache {
            let cache_registry = cache_registry
                .unwrap_or_else(|| tars_cache::MemoryCacheRegistry::default_arc() as _);
            let cache_factory =
                cache_factory.unwrap_or_else(|| tars_cache::CacheKeyFactory::new(1));
            builder = builder.layer(crate::middleware::cache::CacheLookupMiddleware::new(
                cache_registry,
                cache_factory,
                cache_origin,
            ));
        }

        let retry_cfg = retry.unwrap_or_default();
        builder = builder.layer(crate::middleware::retry::RetryMiddleware::new(retry_cfg));

        builder.build()
    }
}

/// Event store pair for [`ChainOpts::events`].
pub struct EventStores {
    pub events: Arc<dyn tars_melt::event::PipelineEventLog>,
    pub records: Arc<dyn tars_melt::event::LlmRecordStore>,
}

/// Options for [`LlmService::default_chain`]. Constructed with
/// [`ChainOpts::new`] (just a cache origin); each field overridable
/// before passing to `default_chain`.
///
/// Marked `non_exhaustive` so adding fields is a non-breaking change.
#[non_exhaustive]
pub struct ChainOpts {
    /// Cache namespace id. Distinguishes cache buckets across providers
    /// / tenants / config versions. Required: `dyn LlmProvider` doesn't
    /// expose its id, and the cache layer needs one.
    pub cache_origin: tars_types::ProviderId,

    /// Output validators (Filter / Reject / Annotate). Run outside
    /// Cache, on every call. Empty Vec = no ValidationMiddleware layer
    /// at all (saves the stream-drain cost on cache hits).
    pub validators: Vec<Arc<dyn crate::middleware::validation::OutputValidator>>,

    /// EventEmitter stores. `None` = no event emission — pipeline still
    /// works, but `tars events list` / trajectory tooling won't see
    /// these calls. Set this in production paths.
    pub events: Option<EventStores>,

    /// Cache registry override. `None` = `MemoryCacheRegistry`
    /// (process-local L1 only). Set a shared registry for service
    /// deployments that need cross-process cache.
    pub cache_registry: Option<Arc<dyn tars_cache::CacheRegistry>>,

    /// Cache key factory override. `None` = `CacheKeyFactory::new(1)`.
    /// Bump the version when prompt-affecting config changes shape so
    /// cached entries from the old shape miss instead of misfiring.
    pub cache_factory: Option<tars_cache::CacheKeyFactory>,

    /// Retry policy override. `None` = `RetryConfig::default()`
    /// (3 attempts, exp backoff, 30s cap).
    pub retry: Option<crate::middleware::retry::RetryConfig>,

    /// Per-provider circuit breaker. `None` (default) = no breaker.
    /// When set, [`LlmService::default_chain`] wraps the provider with
    /// [`crate::CircuitBreaker`] (innermost, below Retry): after
    /// `failure_threshold` consecutive open-time failures it opens and
    /// rejects calls for `cooldown` with
    /// [`tars_types::ProviderError::CircuitOpen`] (a `Retriable` class,
    /// so Retry / Routing react). The breaker state lives on the wrapper,
    /// so every call routed through the same built pipeline SHARES it —
    /// concurrent callers fast-fail the moment any of them trips it,
    /// which is the point for fan-out workloads (hundreds of concurrent
    /// calls against one provider shouldn't each burn a full retry loop
    /// when it's down). Ignored by [`LlmService::chain_over`] — a routed
    /// inner has no single provider to wrap; routed callers wrap each
    /// candidate provider individually before building the inner.
    pub circuit_breaker: Option<crate::middleware::circuit_breaker::CircuitBreakerConfig>,

    /// Include the `CacheLookup` layer. `true` (default) matches the
    /// canonical chain. Set `false` to skip caching entirely — useful
    /// for callers that need every call to hit the provider (latency
    /// measurement, non-deterministic sampling, deterministic tests
    /// where a same-prompt cache hit would shadow the provider).
    pub cache: bool,
}

impl ChainOpts {
    /// Minimal opts: only the cache origin specified, everything else
    /// at defaults (no validators, no event store, in-mem cache,
    /// default retry).
    pub fn new(cache_origin: tars_types::ProviderId) -> Self {
        Self {
            cache_origin,
            validators: Vec::new(),
            events: None,
            cache_registry: None,
            cache_factory: None,
            retry: None,
            circuit_breaker: None,
            cache: true,
        }
    }
}

/// Builder. Layers added via [`Self::layer`] are recorded outer→inner in
/// add order; `build()` produces an [`LlmService`] whose chain runs the
/// first-added layer outermost (the order users read top-to-bottom in
/// code), with any pre-seeded `inner_layers` (from
/// [`LlmService::builder_with_inner`]) kept innermost.
pub struct LlmServiceBuilder {
    provider: Arc<dyn LlmProvider>,
    model: String,
    /// Added via `.layer`, outer→inner in add order.
    outer_layers: Vec<Arc<dyn Middleware>>,
    /// Pre-existing layers from `builder_with_inner`, kept innermost.
    inner_layers: Vec<Arc<dyn Middleware>>,
}

impl LlmServiceBuilder {
    /// Add a layer. The first call adds the **outermost** layer; the
    /// last call adds the layer closest to the provider.
    pub fn layer<M: Middleware>(mut self, mw: M) -> Self {
        self.outer_layers.push(Arc::new(mw));
        self
    }

    /// Return the one concrete public [`LlmService`], carrying the bound
    /// provider + model and the assembled outer→inner layer stack.
    pub fn build(self) -> LlmService {
        let mut layers = self.outer_layers;
        layers.extend(self.inner_layers);
        LlmService::compose(self.provider, self.model, layers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmService;
    use async_trait::async_trait;
    use futures::StreamExt;
    use tars_provider::LlmEventStream;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ChatRequest, ProviderError, RequestContext};

    /// Tiny middleware that just stamps an attribute on the context so
    /// we can prove ordering in tests.
    struct TagLayer {
        tag: &'static str,
    }

    #[async_trait]
    impl Middleware for TagLayer {
        fn name(&self) -> &'static str {
            self.tag
        }
        async fn handle(
            &self,
            req: ChatRequest,
            ctx: RequestContext,
            next: Next<'_>,
        ) -> Result<LlmEventStream, ProviderError> {
            // Append our tag to the attributes so tests can read order.
            {
                let mut attrs = ctx.attributes.write().expect("attributes lock poisoned");
                let entry = attrs
                    .entry("trace".into())
                    .or_insert_with(|| serde_json::Value::String(String::new()));
                if let serde_json::Value::String(s) = entry {
                    if !s.is_empty() {
                        s.push('|');
                    }
                    s.push_str(self.tag);
                }
            }
            next.run(req, ctx).await
        }
    }

    #[tokio::test]
    async fn first_added_layer_is_outermost() {
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let pipeline = LlmService::builder(mock, "test-model")
            .layer(TagLayer { tag: "outer" })
            .layer(TagLayer { tag: "middle" })
            .layer(TagLayer { tag: "inner" })
            .build();
        assert_eq!(pipeline.layer_names(), &["outer", "middle", "inner"]);

        let ctx = RequestContext::test_default();
        let mut s = Arc::new(pipeline)
            .call(
                ChatRequest::user("x"),
                ctx.clone(),
            )
            .await
            .unwrap();
        while let Some(ev) = s.next().await {
            ev.unwrap();
        }

        let trace = ctx
            .attributes
            .read()
            .unwrap()
            .get("trace")
            .cloned()
            .unwrap();
        // Inbound order = outermost-first = "outer|middle|inner".
        assert_eq!(trace, serde_json::json!("outer|middle|inner"));
    }

    #[tokio::test]
    async fn default_chain_layers_match_documented_onion() {
        // Validators present, events present → full onion.
        use crate::middleware::validation::{OutputValidator, builtin::NotEmptyValidator};
        use tars_melt::event::{
            SqliteLlmRecordStore, SqliteLlmRecordStoreConfig, SqlitePipelineEventLog,
            SqlitePipelineEventLogConfig,
        };
        use tars_types::ProviderId;

        let dir = tempfile::tempdir().unwrap();
        let events = SqlitePipelineEventLog::open(SqlitePipelineEventLogConfig::new(
            dir.path().join("ev.db"),
        ))
        .unwrap();
        let records =
            SqliteLlmRecordStore::open(SqliteLlmRecordStoreConfig::new(dir.path().join("bd.db"))).unwrap();

        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let mut opts = ChainOpts::new(ProviderId::new("p"));
        opts.validators = vec![Arc::new(NotEmptyValidator::new()) as Arc<dyn OutputValidator>];
        opts.events = Some(EventStores { events, records });
        let pipeline = LlmService::default_chain(mock, "test-model", opts);

        // Outermost → innermost.
        assert_eq!(
            pipeline.layer_names(),
            &[
                "event_emitter",
                "telemetry",
                "validation",
                "cache_lookup",
                "retry",
            ],
        );
    }

    #[tokio::test]
    async fn default_chain_skips_validation_and_event_emitter_when_unset() {
        use tars_types::ProviderId;
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let opts = ChainOpts::new(ProviderId::new("p"));
        let pipeline = LlmService::default_chain(mock, "test-model", opts);
        assert_eq!(
            pipeline.layer_names(),
            &["telemetry", "cache_lookup", "retry"]
        );
    }

    #[tokio::test]
    async fn default_chain_omits_cache_when_disabled() {
        use tars_types::ProviderId;
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let mut opts = ChainOpts::new(ProviderId::new("p"));
        opts.cache = false;
        let pipeline = LlmService::default_chain(mock, "test-model", opts);
        // cache_lookup dropped; the rest of the canonical order stands.
        assert_eq!(pipeline.layer_names(), &["telemetry", "retry"]);
    }

    #[tokio::test]
    async fn default_chain_wires_circuit_breaker_when_configured() {
        // A provider that always fails; with the breaker configured to
        // open after 2 consecutive failures, the 3rd call must reject
        // with CircuitOpen WITHOUT reaching the provider. Retry is set to
        // a single attempt and cache disabled so each `call` = one
        // provider hit and the breaker's per-call accounting is exact.
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Duration;
        use tars_provider::{LlmEventStream, LlmProvider};
        use tars_types::{Capabilities, Pricing, ProviderError, ProviderId};

        struct AlwaysErr {
            id: ProviderId,
            caps: Capabilities,
            hits: Arc<AtomicU32>,
        }
        #[async_trait]
        impl LlmProvider for AlwaysErr {
            fn id(&self) -> &ProviderId {
                &self.id
            }
            fn capabilities(&self) -> &Capabilities {
                &self.caps
            }
            async fn stream(
                self: Arc<Self>,
                _req: ChatRequest,
                _model: &str,
        _ctx: RequestContext,
            ) -> Result<LlmEventStream, ProviderError> {
                self.hits.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::ModelOverloaded)
            }
        }

        let hits = Arc::new(AtomicU32::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysErr {
            id: ProviderId::new("p"),
            caps: Capabilities::text_only_baseline(Pricing::default()),
            hits: hits.clone(),
        });

        let mut opts = ChainOpts::new(ProviderId::new("p"));
        opts.cache = false;
        opts.retry = Some(crate::middleware::retry::RetryConfig {
            max_attempts: 1,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
            respect_retry_after: false,
            max_attempts_maybe_retriable: 1,
            max_wait: Duration::MAX,
            jitter: Duration::ZERO,
        });
        opts.circuit_breaker = Some(crate::middleware::circuit_breaker::CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(30),
        });
        let pipeline = Arc::new(LlmService::default_chain(provider, "test-model", opts));

        let req = || ChatRequest::user("x");
        // Two failures trip the breaker (both reach the provider).
        for _ in 0..2 {
            let e = pipeline.clone().call(req(), RequestContext::test_default()).await;
            assert!(matches!(e, Err(ProviderError::ModelOverloaded)));
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2, "both failures hit the provider");
        // Third call: breaker is Open → reject without touching the provider.
        let e = pipeline.clone().call(req(), RequestContext::test_default()).await;
        assert!(
            matches!(e, Err(ProviderError::CircuitOpen { .. })),
            "breaker must reject the 3rd call as CircuitOpen",
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "open breaker spared the provider");
    }

    #[tokio::test]
    async fn empty_pipeline_passes_through_to_provider() {
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let pipeline = LlmService::builder(mock, "test-model").build();
        assert!(pipeline.layer_names().is_empty());

        let mut s = Arc::new(pipeline)
            .call(
                ChatRequest::user("x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let mut got = 0;
        while let Some(ev) = s.next().await {
            ev.unwrap();
            got += 1;
        }
        assert_eq!(got, 3);
    }
}
