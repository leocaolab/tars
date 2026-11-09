//! Defines a chain layer ([`Middleware`]) and how layers are stacked into an [`LlmService`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::{LlmEventStream, LlmProvider};
use tars_types::{Caller, ChatRequest};

use crate::pipeline::service::{LlmService, Next, OpenFailure, RefusedCall};

pub(crate) mod budget;
pub(crate) mod event_emitter;
pub(crate) mod rate_limit;
pub(crate) mod retry;
pub(crate) mod telemetry;
pub(crate) mod tenant_budget;
pub(crate) mod validation;

/// One layer of an [`LlmService`]'s chain. A layer that needs the model reads [`Next::model`].
#[async_trait]
pub trait Middleware: Send + Sync + 'static {
    /// Returns a stable, low-cardinality label for tracing spans and metrics.
    fn name(&self) -> &'static str;

    /// Handles one call, calling `next.run` zero or more times. What the layer measures goes out
    /// with what it returns: as [`ChatEvent::Measured`](tars_types::ChatEvent::Measured) in the
    /// stream, or in the [`OpenFailure`] when no stream opens.
    async fn handle(
        &self,
        req: ChatRequest,
        ctx: Caller,
        next: Next<'_>,
    ) -> Result<LlmEventStream, OpenFailure>;

    /// Is told of a call the dispatch layer refused before any layer ran, at the moment it
    /// refused it; `handle` never sees that call. A layer that records every call (the call log,
    /// tracing) records this one here; a layer that only acts on the calls it handles does
    /// nothing, as by default.
    fn on_refusal(&self, _refusal: &RefusedCall<'_>) {}
}

impl LlmService {
    /// Starts a builder over `provider` bound to `model`; the first layer added ends up outermost.
    pub fn builder(provider: Arc<dyn LlmProvider>, model: impl Into<String>) -> LlmServiceBuilder {
        LlmServiceBuilder {
            provider,
            model: model.into(),
            outer_layers: Vec::new(),
            inner_layers: Vec::new(),
        }
    }

    /// Starts a builder over an existing service, whose layers stay innermost; layers added with
    /// [`LlmServiceBuilder::layer`] wrap outside them.
    pub fn builder_with_inner(inner: LlmService) -> LlmServiceBuilder {
        let (provider, model, inner_layers) = inner.into_parts();
        LlmServiceBuilder {
            provider,
            model,
            outer_layers: Vec::new(),
            inner_layers,
        }
    }

    /// Builds the default chain over `provider` bound to `model`, outermost first:
    ///
    /// ```text
    /// EventEmitter? → Telemetry → Validation? → Retry → Provider
    /// ```
    ///
    /// The order is each layer's own requirement: [`EventEmitterMiddleware`] says why it sits
    /// outside Telemetry and Validation.
    ///
    /// [`EventEmitterMiddleware`]: crate::EventEmitterMiddleware
    pub fn default_chain(
        provider: Arc<dyn LlmProvider>,
        model: impl Into<String>,
        opts: ChainOptions,
    ) -> LlmService {
        Self::chain_over(LlmService::of(provider, model), opts)
    }

    /// Builds the [`default_chain`](Self::default_chain) layers outside `inner`'s own layers.
    pub fn chain_over(inner: LlmService, opts: ChainOptions) -> LlmService {
        let ChainOptions {
            validators,
            events,
            bodies,
            retry,
        } = opts;

        let mut builder = Self::builder_with_inner(inner);

        if let Some(calls) = events {
            builder = builder.layer(
                crate::pipeline::middleware::event_emitter::EventEmitterMiddleware::new(calls)
                    .with_bodies(bodies),
            );
        }
        builder = builder.layer(crate::pipeline::middleware::telemetry::TelemetryMiddleware::new());

        if !validators.is_empty() {
            builder = builder.layer(
                crate::pipeline::middleware::validation::ValidationMiddleware::new(validators),
            );
        }

        let retry_cfg = retry.unwrap_or_default();
        builder = builder.layer(crate::pipeline::middleware::retry::RetryMiddleware::new(
            retry_cfg,
        ));

        builder.build()
    }
}

/// Options for [`LlmService::default_chain`]; the default has no validators and no call log.
#[derive(Default)]
#[non_exhaustive]
pub struct ChainOptions {
    /// Output validators. Empty means no validation layer.
    pub validators: Vec<Arc<dyn crate::pipeline::middleware::validation::OutputValidator>>,

    /// The call log that gets one `llm_call` row per call. `None` means no emitter layer and no
    /// log. The caller owns the sink and closes it.
    pub events:
        Option<Arc<dyn tars_melt::event::log::LogSink<tars_melt::event::call_log::LlmCallTable>>>,

    /// Whether the call log keeps request and response bodies.
    pub bodies: crate::pipeline::middleware::event_emitter::BodyRetention,

    /// The retry policy. `None` means `RetryConfig::default()`.
    pub retry: Option<crate::pipeline::middleware::retry::RetryConfig>,
}

/// Collects layers for an [`LlmService`]; the first layer added is outermost.
pub struct LlmServiceBuilder {
    provider: Arc<dyn LlmProvider>,
    model: String,
    outer_layers: Vec<Arc<dyn Middleware>>,
    /// Layers of the service given to `builder_with_inner`, kept innermost.
    inner_layers: Vec<Arc<dyn Middleware>>,
}

impl LlmServiceBuilder {
    /// Adds a layer inside the ones already added and outside the inner service's.
    pub fn layer<M: Middleware>(mut self, mw: M) -> Self {
        self.outer_layers.push(Arc::new(mw));
        self
    }

    pub fn build(self) -> LlmService {
        let mut layers = self.outer_layers;
        layers.extend(self.inner_layers);
        LlmService::compose(self.provider, self.model, layers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmError;
    use crate::LlmEventStream;
    use crate::LlmService;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use async_trait::async_trait;
    use futures::StreamExt;
    use tars_types::ProviderId;
    use tars_types::{Caller, ChatRequest};

    /// Appends its tag to the context's `trace` attribute, so a test can read the order layers
    /// ran in.
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
            ctx: Caller,
            next: Next<'_>,
        ) -> Result<LlmEventStream, OpenFailure> {
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
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        let pipeline = LlmService::builder(mock, "test-model")
            .layer(TagLayer { tag: "outer" })
            .layer(TagLayer { tag: "middle" })
            .layer(TagLayer { tag: "inner" })
            .build();
        assert_eq!(pipeline.layer_names(), &["outer", "middle", "inner"]);

        let ctx = Caller::test_default();
        let mut s = Arc::new(pipeline)
            .call(ChatRequest::user("x"), ctx.clone())
            .await
            .result
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
        assert_eq!(trace, serde_json::json!("outer|middle|inner"));
    }

    #[tokio::test]
    async fn default_chain_layers_match_documented_onion() {
        use crate::pipeline::middleware::validation::{
            OutputValidator, builtin::NotEmptyValidator,
        };
        use tars_melt::event::call_log::{CALL_LOG_SINK, LlmCallTable};
        use tars_melt::event::log::MemLog;

        let calls =
            MemLog::<LlmCallTable>::new().sink(CALL_LOG_SINK, &tokio::runtime::Handle::current());

        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        let opts = ChainOptions {
            validators: vec![Arc::new(NotEmptyValidator::new()) as Arc<dyn OutputValidator>],
            events: Some(Arc::new(calls)),
            ..Default::default()
        };
        let pipeline = LlmService::default_chain(mock, "test-model", opts);

        assert_eq!(
            pipeline.layer_names(),
            &["event_emitter", "telemetry", "validation", "retry",],
        );
    }

    #[tokio::test]
    async fn default_chain_skips_validation_and_event_emitter_when_unset() {
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        let opts = ChainOptions::default();
        let pipeline = LlmService::default_chain(mock, "test-model", opts);
        assert_eq!(pipeline.layer_names(), &["telemetry", "retry"]);
    }

    #[tokio::test]
    async fn circuit_breaker_wrapped_provider_rejects_once_open() {
        // Retry makes a single attempt, so each `call` is one provider hit.
        use crate::ProviderError;
        use crate::{LlmEventStream, LlmProvider};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Duration;
        use tars_types::{Pricing, ProviderId, ProviderProfile};

        struct AlwaysErr {
            id: ProviderId,
            caps: ProviderProfile,
            hits: Arc<AtomicU32>,
        }
        #[async_trait]
        impl LlmProvider for AlwaysErr {
            fn id(&self) -> &ProviderId {
                &self.id
            }
            fn capabilities(&self) -> &ProviderProfile {
                &self.caps
            }
            async fn stream(
                self: Arc<Self>,
                _req: ChatRequest,
                _model: &str,
                _ctx: Caller,
            ) -> Result<LlmEventStream, ProviderError> {
                self.hits.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::ModelOverloaded)
            }
        }

        let hits = Arc::new(AtomicU32::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysErr {
            id: ProviderId::new("p"),
            caps: ProviderProfile::text_only_baseline(Pricing::default()),
            hits: hits.clone(),
        });
        let provider = crate::pipeline::circuit_breaker::CircuitBreaker::wrap(
            provider,
            crate::pipeline::circuit_breaker::CircuitBreakerConfig::new(
                std::num::NonZeroU32::new(2).unwrap(),
                Duration::from_secs(30),
            )
            .unwrap(),
        );

        let opts = ChainOptions {
            retry: Some(crate::pipeline::middleware::retry::RetryConfig {
                max_attempts: 1,
                initial_backoff: Duration::ZERO,
                max_backoff: Duration::ZERO,
                multiplier: 1.0,
                respect_retry_after: false,
                max_wait: Duration::MAX,
                jitter: Duration::ZERO,
            }),
            ..Default::default()
        };
        let pipeline = Arc::new(LlmService::default_chain(provider, "test-model", opts));

        let req = || ChatRequest::user("x");
        for _ in 0..2 {
            let e = pipeline
                .clone()
                .call(req(), Caller::test_default())
                .await
                .result;
            assert!(matches!(
                e,
                Err(LlmError::Provider(ProviderError::ModelOverloaded))
            ));
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "both failures hit the provider"
        );
        let e = pipeline
            .clone()
            .call(req(), Caller::test_default())
            .await
            .result;
        assert!(
            matches!(
                e,
                Err(LlmError::Provider(ProviderError::Unavailable { .. }))
            ),
            "breaker must reject the 3rd call as Unavailable",
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "open breaker spared the provider"
        );
    }

    #[tokio::test]
    async fn empty_pipeline_passes_through_to_provider() {
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        let pipeline = LlmService::builder(mock, "test-model").build();
        assert!(pipeline.layer_names().is_empty());

        let mut s = Arc::new(pipeline)
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result
            .unwrap();
        let mut events = Vec::new();
        while let Some(ev) = s.next().await {
            events.push(ev.unwrap());
        }
        // The provider's events, with the provider time just before `Finished`.
        assert!(
            matches!(
                events.as_slice(),
                [
                    tars_types::ChatEvent::Started { .. },
                    tars_types::ChatEvent::Delta { .. },
                    tars_types::ChatEvent::Measured { .. },
                    tars_types::ChatEvent::Finished { .. },
                ]
            ),
            "{events:?}"
        );
    }
}
