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

    /// Add a boxed middleware. Useful when layer composition is itself
    /// driven by config (each variant produces a `Box<dyn Middleware>`).
    pub fn layer_boxed(mut self, mw: Box<dyn Middleware>) -> Self {
        self.layers_outer_to_inner.push(mw);
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
