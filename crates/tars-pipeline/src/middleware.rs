//! [`Middleware`] trait + [`Pipeline`] / [`PipelineBuilder`].
//!
//! See module-level docs on [`crate`] for the design rationale.

use std::sync::Arc;

use async_trait::async_trait;

use tars_provider::{LlmEventStream, LlmProvider};
use tars_types::{ChatRequest, ProviderError, RequestContext};

use crate::service::{LlmService, ProviderService};

/// A middleware factory — given an inner [`LlmService`], produce a
/// new [`LlmService`] that wraps it. Equivalent to `tower::Layer`.
///
/// Implementors typically return a small struct that holds
/// `inner: Arc<dyn LlmService>` plus their own configuration, with
/// their own `LlmService` impl orchestrating the call. See
/// [`crate::TelemetryMiddleware`] / [`crate::RetryMiddleware`] for
/// reference impls.
pub trait Middleware: Send + Sync + 'static {
    /// Stable, low-cardinality label used in tracing spans / metrics.
    fn name(&self) -> &'static str;

    /// Wrap `inner` and return the wrapped service.
    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService>;
}

/// Built pipeline. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct Pipeline {
    inner: Arc<dyn LlmService>,
    /// Names of layers, outermost-first. Useful for diagnostic
    /// `pipeline.describe()` output and for tests asserting the
    /// configured stack.
    layer_names: Arc<[&'static str]>,
}

impl Pipeline {
    /// Start a new builder around a Provider. The Provider becomes the
    /// innermost service; layers added via [`PipelineBuilder::layer`]
    /// wrap it from inside out, with the **first** added layer ending
    /// up outermost.
    pub fn builder(provider: Arc<dyn LlmProvider>) -> PipelineBuilder {
        PipelineBuilder {
            inner: ProviderService::new(provider),
            layers_outer_to_inner: Vec::new(),
        }
    }

    /// Start a builder from an arbitrary inner service. Useful for tests
    /// that want to point the pipeline at a hand-rolled fake without
    /// going through a full `LlmProvider` impl.
    pub fn builder_with_inner(inner: Arc<dyn LlmService>) -> PipelineBuilder {
        PipelineBuilder {
            inner,
            layers_outer_to_inner: Vec::new(),
        }
    }

    /// Outermost-first list of layer names. `["telemetry", "retry"]`
    /// means a request hits Telemetry first, then Retry, then the
    /// Provider; the response flows back in reverse.
    pub fn layer_names(&self) -> &[&'static str] {
        &self.layer_names
    }

    /// Convenience: same as `Arc::new(self).call(req, ctx).await`.
    pub async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        self.inner.clone().call(req, ctx).await
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
    pub fn default_chain(provider: Arc<dyn LlmProvider>, opts: PipelineOpts) -> Self {
        let PipelineOpts {
            cache_origin,
            validators,
            events,
            cache_registry,
            cache_factory,
            retry,
        } = opts;

        let mut builder = Self::builder(provider);

        if let Some(EventStores { events: ev, bodies }) = events {
            builder = builder.layer(crate::event_emitter::EventEmitterMiddleware::new(
                ev, bodies,
            ));
        }
        builder = builder.layer(crate::telemetry::TelemetryMiddleware::new());

        if !validators.is_empty() {
            builder = builder.layer(crate::validation::ValidationMiddleware::new(validators));
        }

        let cache_registry =
            cache_registry.unwrap_or_else(|| tars_cache::MemoryCacheRegistry::default_arc() as _);
        let cache_factory = cache_factory.unwrap_or_else(|| tars_cache::CacheKeyFactory::new(1));
        builder = builder.layer(crate::cache::CacheLookupMiddleware::new(
            cache_registry,
            cache_factory,
            cache_origin,
        ));

        let retry_cfg = retry.unwrap_or_default();
        builder = builder.layer(crate::retry::RetryMiddleware::new(retry_cfg));

        builder.build()
    }
}

/// Event store pair for [`PipelineOpts::events`].
pub struct EventStores {
    pub events: Arc<dyn tars_storage::PipelineEventStore>,
    pub bodies: Arc<dyn tars_storage::BodyStore>,
}

/// Options for [`Pipeline::default_chain`]. Constructed with
/// [`PipelineOpts::new`] (just a cache origin); each field overridable
/// before passing to `default_chain`.
///
/// Marked `non_exhaustive` so adding fields is a non-breaking change.
#[non_exhaustive]
pub struct PipelineOpts {
    /// Cache namespace id. Distinguishes cache buckets across providers
    /// / tenants / config versions. Required: `dyn LlmProvider` doesn't
    /// expose its id, and the cache layer needs one.
    pub cache_origin: tars_types::ProviderId,

    /// Output validators (Filter / Reject / Annotate). Run outside
    /// Cache, on every call. Empty Vec = no ValidationMiddleware layer
    /// at all (saves the stream-drain cost on cache hits).
    pub validators: Vec<Arc<dyn crate::validation::OutputValidator>>,

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
    pub retry: Option<crate::retry::RetryConfig>,
}

impl PipelineOpts {
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
        }
    }
}

#[async_trait]
impl LlmService for Pipeline {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        self.inner.clone().call(req, ctx).await
    }
}

/// Builder. Layers are recorded outer→inner as they're added; `build()`
/// folds them onto `inner` in reverse so the first-added layer ends up
/// outermost (the order users naturally read top-to-bottom in code).
pub struct PipelineBuilder {
    inner: Arc<dyn LlmService>,
    layers_outer_to_inner: Vec<Box<dyn Middleware>>,
}

impl PipelineBuilder {
    /// Add a layer. The first call adds the **outermost** layer; the
    /// last call adds the layer closest to the provider.
    pub fn layer<M: Middleware>(mut self, mw: M) -> Self {
        self.layers_outer_to_inner.push(Box::new(mw));
        self
    }

    pub fn build(self) -> Pipeline {
        let mut svc = self.inner;
        // Wrap from innermost outward — last added → first wrapped.
        let mut names: Vec<&'static str> = Vec::with_capacity(self.layers_outer_to_inner.len());
        for mw in self.layers_outer_to_inner.iter().rev() {
            // (collected outer→inner; iterate reversed to wrap inside-out)
            svc = mw.wrap(svc);
        }
        for mw in &self.layers_outer_to_inner {
            names.push(mw.name());
        }
        Pipeline {
            inner: svc,
            layer_names: names.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::ModelHint;

    /// Tiny middleware that just stamps an attribute on the context so
    /// we can prove ordering in tests.
    struct TagLayer {
        tag: &'static str,
    }

    impl Middleware for TagLayer {
        fn name(&self) -> &'static str {
            self.tag
        }
        fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
            Arc::new(TagService {
                inner,
                tag: self.tag,
            })
        }
    }

    struct TagService {
        inner: Arc<dyn LlmService>,
        tag: &'static str,
    }

    #[async_trait]
    impl LlmService for TagService {
        async fn call(
            self: Arc<Self>,
            req: ChatRequest,
            ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            // Append our tag to the attributes so tests can read order.
            {
                let mut attrs = ctx.attributes.write().unwrap();
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
            self.inner.clone().call(req, ctx).await
        }
    }

    #[tokio::test]
    async fn first_added_layer_is_outermost() {
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let pipeline = Pipeline::builder(mock)
            .layer(TagLayer { tag: "outer" })
            .layer(TagLayer { tag: "middle" })
            .layer(TagLayer { tag: "inner" })
            .build();
        assert_eq!(pipeline.layer_names(), &["outer", "middle", "inner"]);

        let ctx = RequestContext::test_default();
        let mut s = Arc::new(pipeline)
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "x"),
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
        use crate::validation::{OutputValidator, builtin::NotEmptyValidator};
        use tars_storage::{
            SqliteBodyStore, SqliteBodyStoreConfig, SqlitePipelineEventStore,
            SqlitePipelineEventStoreConfig,
        };
        use tars_types::ProviderId;

        let dir = tempfile::tempdir().unwrap();
        let events = SqlitePipelineEventStore::open(SqlitePipelineEventStoreConfig::new(
            dir.path().join("ev.db"),
        ))
        .unwrap();
        let bodies =
            SqliteBodyStore::open(SqliteBodyStoreConfig::new(dir.path().join("bd.db"))).unwrap();

        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let mut opts = PipelineOpts::new(ProviderId::new("p"));
        opts.validators = vec![Arc::new(NotEmptyValidator::new()) as Arc<dyn OutputValidator>];
        opts.events = Some(EventStores { events, bodies });
        let pipeline = Pipeline::default_chain(mock, opts);

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
        let opts = PipelineOpts::new(ProviderId::new("p"));
        let pipeline = Pipeline::default_chain(mock, opts);
        assert_eq!(
            pipeline.layer_names(),
            &["telemetry", "cache_lookup", "retry"]
        );
    }

    #[tokio::test]
    async fn empty_pipeline_passes_through_to_provider() {
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let pipeline = Pipeline::builder(mock).build();
        assert!(pipeline.layer_names().is_empty());

        let mut s = Arc::new(pipeline)
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "x"),
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
