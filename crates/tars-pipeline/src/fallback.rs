//! Fallback middleware — error-driven provider switching.
//!
//! Sits **outside** [`crate::RetryMiddleware`]: each fallback hop is
//! its own independent "primary attempt," so transient flakes on a
//! given provider exhaust that provider's retry budget before we
//! switch. The hop order matters and is explicit — no magic.
//!
//! Triggers are matched against [`tars_types::ProviderError::kind`]
//! strings rather than [`tars_types::ErrorClass`] because the class
//! granularity is too coarse: `BudgetExceeded` and `ContextTooLong`
//! both map to `Permanent`, but cost-driven and context-driven
//! fallbacks want different downstream providers.
//!
//! See [`docs/roadmap.md §2`](../../../../docs/roadmap.md) for the
//! design rationale and the Cando-Peter motivation.
//!
//! # Composition
//!
//! ```ignore
//! let primary_svc = RetryMiddleware::default().wrap(ProviderService::new(primary));
//! let sonnet_svc  = RetryMiddleware::default().wrap(ProviderService::new(sonnet));
//! let local_svc   = RetryMiddleware::default().wrap(ProviderService::new(local));
//!
//! let mw = FallbackMiddleware::builder()
//!     .fallback_to_service(sonnet_svc, FallbackTrigger::cost_related())
//!     .fallback_to_service(local_svc,  FallbackTrigger::availability())
//!     .build();
//!
//! let pipeline = Pipeline::builder(primary_svc)
//!     .layer(TelemetryMiddleware::new())
//!     .layer(mw)                            // Fallback OUTSIDE
//!     .build();
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use tars_provider::LlmEventStream;
use tars_types::{ChatRequest, ProviderError, RequestContext};

use crate::middleware::Middleware;
use crate::service::LlmService;

/// Which `ProviderError` kinds should trigger a fallback hop.
///
/// Keyed on the stable `kind()` strings from `tars-types` (e.g.
/// `"rate_limited"`, `"budget_exceeded"`). An empty set matches any
/// error — useful for a final "kitchen-sink" hop that catches whatever
/// the earlier hops didn't.
#[derive(Clone, Debug, Default)]
pub struct FallbackTrigger {
    kinds: HashSet<tars_types::ProviderErrorKind>,
}

impl FallbackTrigger {
    /// Trigger on the given error kinds.
    pub fn on(kinds: &[tars_types::ProviderErrorKind]) -> Self {
        Self {
            kinds: kinds.iter().copied().collect(),
        }
    }

    /// Cost / capability errors: caller wants a cheaper or larger-context model.
    /// Includes `BudgetExceeded` and `ContextTooLong`.
    pub fn cost_related() -> Self {
        use tars_types::ProviderErrorKind as K;
        Self::on(&[K::BudgetExceeded, K::ContextTooLong])
    }

    /// Availability / load errors: caller wants a different provider.
    /// Includes `RateLimited`, `ModelOverloaded`, `CircuitOpen`, `Network`.
    pub fn availability() -> Self {
        use tars_types::ProviderErrorKind as K;
        Self::on(&[
            K::RateLimited,
            K::ModelOverloaded,
            K::CircuitOpen,
            K::Network,
        ])
    }

    /// Match anything. Use sparingly — masks bugs (`invalid_request` on
    /// hop 1 would fallback then bug-mask on hop 2 with the same input).
    pub fn any() -> Self {
        Self {
            kinds: HashSet::new(),
        }
    }

    pub fn matches(&self, err: &ProviderError) -> bool {
        // Empty set = match-anything sentinel.
        self.kinds.is_empty() || self.kinds.contains(&err.kind())
    }
}

/// Builds a [`FallbackMiddleware`] with an ordered list of hops.
#[derive(Default)]
pub struct FallbackBuilder {
    hops: Vec<(Arc<dyn LlmService>, FallbackTrigger)>,
}

impl FallbackBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a hop with a fully-built inner service. Use when the hop
    /// needs custom middleware (cache, retry override, etc.).
    pub fn fallback_to_service(
        mut self,
        svc: Arc<dyn LlmService>,
        trigger: FallbackTrigger,
    ) -> Self {
        self.hops.push((svc, trigger));
        self
    }

    pub fn build(self) -> FallbackMiddleware {
        FallbackMiddleware { hops: self.hops }
    }
}

/// Middleware that switches to a configured fallback provider when the
/// inner service returns a typed error matching that hop's trigger.
///
/// Place this **outside** [`crate::RetryMiddleware`] so each hop gets a
/// full retry budget on its own provider before we switch.
#[derive(Clone)]
pub struct FallbackMiddleware {
    hops: Vec<(Arc<dyn LlmService>, FallbackTrigger)>,
}

impl FallbackMiddleware {
    pub fn builder() -> FallbackBuilder {
        FallbackBuilder::new()
    }
}

impl Middleware for FallbackMiddleware {
    fn name(&self) -> &'static str {
        "fallback"
    }

    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(FallbackService {
            primary: inner,
            hops: self.hops.clone(),
        })
    }
}

struct FallbackService {
    primary: Arc<dyn LlmService>,
    hops: Vec<(Arc<dyn LlmService>, FallbackTrigger)>,
}

#[async_trait]
impl LlmService for FallbackService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Layer trace. Best-effort: a poisoned lock (another thread
        // panicked while holding it) must not fail the call, but we leave
        // a breadcrumb rather than swallowing it silently.
        // [arc:intentional-handle]
        match ctx.telemetry.lock() {
            Ok(mut t) => t.layers.push("fallback".into()),
            Err(_) => tracing::debug!(
                event = "fallback.telemetry_poisoned",
                "telemetry mutex poisoned; skipping layer trace"
            ),
        }

        // Try the primary first.
        let mut last_err = match self.primary.clone().call(req.clone(), ctx.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(e) => e,
        };

        // Walk the hop chain in declared order. Each hop only fires if
        // its trigger matches the *current* error (which may have been
        // updated by a previous hop's failure).
        for (hop_index, (svc, trigger)) in self.hops.iter().enumerate() {
            if ctx.cancel.is_cancelled() {
                return Err(ProviderError::Internal(
                    "cancelled during fallback chain".into(),
                ));
            }
            if !trigger.matches(&last_err) {
                continue;
            }
            tracing::warn!(
                event = "fallback.triggered",
                hop = hop_index,
                from_kind = last_err.kind().as_str(),
                trace_id = %ctx.trace_id,
                tenant_id = %ctx.tenant_id,
                "fallback: primary failed, switching to next hop",
            );
            match svc.clone().call(req.clone(), ctx.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }

        // Every applicable hop failed (or no hop applied). Surface the
        // most recent error — pattern-matchers downstream see the same
        // `ProviderError` shape as without fallback.
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ChatEvent, ModelHint};

    use crate::service::ProviderService;

    /// Service that returns a canned error every call.
    struct AlwaysFails {
        err: Box<dyn Fn() -> ProviderError + Send + Sync>,
        observed: Arc<AtomicU32>,
    }

    #[async_trait]
    impl LlmService for AlwaysFails {
        async fn call(
            self: Arc<Self>,
            _req: ChatRequest,
            _ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            self.observed.fetch_add(1, Ordering::SeqCst);
            Err((self.err)())
        }
    }

    fn failing(
        err: impl Fn() -> ProviderError + Send + Sync + 'static,
    ) -> (Arc<dyn LlmService>, Arc<AtomicU32>) {
        let observed = Arc::new(AtomicU32::new(0));
        let svc: Arc<dyn LlmService> = Arc::new(AlwaysFails {
            err: Box::new(err),
            observed: observed.clone(),
        });
        (svc, observed)
    }

    fn ok_service(text: &'static str) -> (Arc<dyn LlmService>, Arc<AtomicU32>) {
        // Count via a wrapper because MockProvider doesn't expose call count.
        let observed = Arc::new(AtomicU32::new(0));
        struct CountingService {
            inner: Arc<dyn LlmService>,
            observed: Arc<AtomicU32>,
        }
        #[async_trait]
        impl LlmService for CountingService {
            async fn call(
                self: Arc<Self>,
                req: ChatRequest,
                ctx: RequestContext,
            ) -> Result<LlmEventStream, ProviderError> {
                self.observed.fetch_add(1, Ordering::SeqCst);
                self.inner.clone().call(req, ctx).await
            }
        }
        let mock = MockProvider::new("ok", CannedResponse::text(text));
        let inner = ProviderService::new(mock);
        let svc: Arc<dyn LlmService> = Arc::new(CountingService {
            inner,
            observed: observed.clone(),
        });
        (svc, observed)
    }

    #[tokio::test]
    async fn primary_success_skips_all_hops() {
        let (primary, primary_calls) = ok_service("primary");
        let (hop1, hop1_calls) = failing(|| ProviderError::BudgetExceeded);

        let mw = FallbackMiddleware::builder()
            .fallback_to_service(hop1, FallbackTrigger::any())
            .build();
        let wrapped = mw.wrap(primary);

        let mut stream = wrapped
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        while let Some(ev) = stream.next().await {
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                break;
            }
        }
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            hop1_calls.load(Ordering::SeqCst),
            0,
            "hop must not run on success"
        );
    }

    #[tokio::test]
    async fn matching_trigger_switches_to_hop() {
        let (primary, primary_calls) = failing(|| ProviderError::BudgetExceeded);
        let (hop, hop_calls) = ok_service("hop-success");

        let mw = FallbackMiddleware::builder()
            .fallback_to_service(hop, FallbackTrigger::cost_related())
            .build();
        let wrapped = mw.wrap(primary);

        let mut stream = wrapped
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .expect("hop should succeed");
        while let Some(ev) = stream.next().await {
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                break;
            }
        }
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hop_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn non_matching_trigger_bubbles_without_hop() {
        let (primary, primary_calls) = failing(|| ProviderError::Auth("bad key".into()));
        let (hop, hop_calls) = ok_service("hop-should-not-run");

        let mw = FallbackMiddleware::builder()
            // cost_related does NOT cover "auth"
            .fallback_to_service(hop, FallbackTrigger::cost_related())
            .build();
        let wrapped = mw.wrap(primary);

        let err = wrapped
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .err()
            .expect("must bubble auth error");
        assert!(matches!(err, ProviderError::Auth(_)));
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hop_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn walks_chain_until_one_matches() {
        // primary fails with BudgetExceeded → hop1 trigger=availability (no match)
        // → hop2 trigger=cost_related (match) → succeeds
        let (primary, primary_calls) = failing(|| ProviderError::BudgetExceeded);
        let (hop1, hop1_calls) = ok_service("should-not-fire");
        let (hop2, hop2_calls) = ok_service("matching-hop");

        let mw = FallbackMiddleware::builder()
            .fallback_to_service(hop1, FallbackTrigger::availability())
            .fallback_to_service(hop2, FallbackTrigger::cost_related())
            .build();
        let wrapped = mw.wrap(primary);

        let _ = wrapped
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .expect("hop2 should succeed");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            hop1_calls.load(Ordering::SeqCst),
            0,
            "non-matching trigger must skip"
        );
        assert_eq!(hop2_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_hops_fail_returns_last_error() {
        let (primary, _) = failing(|| ProviderError::BudgetExceeded);
        let (hop1, _) = failing(|| ProviderError::RateLimited { retry_after: None });
        let (hop2, _) = failing(|| ProviderError::ModelOverloaded);

        let mw = FallbackMiddleware::builder()
            .fallback_to_service(hop1, FallbackTrigger::any())
            .fallback_to_service(hop2, FallbackTrigger::any())
            .build();
        let wrapped = mw.wrap(primary);

        let err = wrapped
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .err()
            .expect("all fail");
        // Must surface the LAST hop's error (ModelOverloaded), not the primary's.
        assert!(
            matches!(err, ProviderError::ModelOverloaded),
            "expected ModelOverloaded, got {err:?}"
        );
    }

    #[tokio::test]
    async fn cancel_between_hops_aborts_chain() {
        let (primary, _) = failing(|| ProviderError::BudgetExceeded);
        let (hop, hop_calls) = ok_service("should-not-fire-after-cancel");

        let mw = FallbackMiddleware::builder()
            .fallback_to_service(hop, FallbackTrigger::any())
            .build();
        let wrapped = mw.wrap(primary);

        let ctx = RequestContext::test_default();
        let cancel = ctx.cancel.clone();
        cancel.cancel(); // pre-cancel — primary runs, then cancel is observed before hop

        let err = wrapped
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "hi"),
                ctx,
            )
            .await
            .err()
            .expect("must abort");
        assert!(
            matches!(err, ProviderError::Internal(ref m) if m.contains("cancel")),
            "expected cancellation, got {err:?}"
        );
        assert_eq!(hop_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn trigger_matchers_cover_expected_kinds() {
        // Pin the kind-string sets so a rename in tars-types ProviderError::kind()
        // breaks this test immediately — that's intentional, it's the canary.
        let e_budget = ProviderError::BudgetExceeded;
        let e_ctx = ProviderError::ContextTooLong {
            limit: 1,
            requested: 2,
        };
        let e_rate = ProviderError::RateLimited { retry_after: None };
        let e_load = ProviderError::ModelOverloaded;
        let e_auth = ProviderError::Auth("x".into());

        assert!(FallbackTrigger::cost_related().matches(&e_budget));
        assert!(FallbackTrigger::cost_related().matches(&e_ctx));
        assert!(!FallbackTrigger::cost_related().matches(&e_rate));
        assert!(!FallbackTrigger::cost_related().matches(&e_auth));

        assert!(FallbackTrigger::availability().matches(&e_rate));
        assert!(FallbackTrigger::availability().matches(&e_load));
        assert!(!FallbackTrigger::availability().matches(&e_budget));

        assert!(FallbackTrigger::any().matches(&e_auth));
        assert!(
            FallbackTrigger::on(&[tars_types::ProviderErrorKind::Auth]).matches(&e_auth)
        );

        // sanity: kind strings are stable
        assert_eq!(
            ProviderError::BudgetExceeded.kind(),
            tars_types::ProviderErrorKind::BudgetExceeded
        );
        assert_eq!(
            ProviderError::BudgetExceeded.kind().as_str(),
            "budget_exceeded"
        );
        let _ = Duration::from_secs(0); // pin import (no-op)
    }
}
