//! Per-provider circuit breaker. Doc 02 §4.7 + Doc 14 M2 §8.1.
//!
//! ## Where it fits
//!
//! In the canonical pipeline (Doc 02 §2):
//! ```text
//! Routing > CircuitBreaker > Retry > Provider
//! ```
//!
//! Each provider has its own breaker. When a provider fails repeatedly
//! the breaker opens; subsequent calls reject immediately with
//! [`ProviderError::CircuitOpen`]. Because that error class is
//! `Retriable`, an upstream [`crate::RoutingService`] sees it as
//! "this candidate isn't healthy right now" and falls through to the
//! next candidate without paying the failure latency.
//!
//! After a cooldown the breaker enters HalfOpen and lets the next call
//! through as a probe. Probe success → closed. Probe failure → open
//! again with a fresh cooldown.
//!
//! ## Failure-rate vs consecutive-failures
//!
//! Doc 14 §8.1 calls for a "basic failure-rate" breaker. The simpler
//! version of that — and the one we ship here — is **consecutive
//! failures**: open after N back-to-back errors, reset the counter on
//! any success. No sliding window, no per-bucket bookkeeping. Easy to
//! reason about and the right shape for the typical "provider went
//! down for 30s" failure mode. We can grow to a real time-windowed
//! rate later if a deployment surfaces a use case (intermittent
//! 30%-failure-rate providers, etc.).
//!
//! ## Mid-stream failures
//!
//! The breaker only sees the **open** result of `provider.stream(...)`.
//! Mid-stream errors (network drop after the first byte) don't currently
//! count toward the failure tally. Doc 01 §3.2 already discusses why
//! mid-stream retry is out of scope; the same reasoning applies to
//! mid-stream breaker accounting. Open-time failures are the dominant
//! failure mode for HTTP backends.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use tars_provider::{LlmEventStream, LlmProvider};
use tars_types::{
    Capabilities, ChatRequest, ChatResponse, CostUsd, ProviderError, ProviderId, RequestContext,
    Usage,
};

#[derive(Clone, Debug)]
pub struct CircuitBreakerConfig {
    /// Open after this many consecutive open-time failures.
    pub failure_threshold: u32,
    /// How long an Open breaker stays Open before transitioning to
    /// HalfOpen. Doc 02 §4.7 suggests 30s as the standard tradeoff
    /// between "give the provider time to recover" and "don't keep a
    /// healthy provider locked out".
    pub cooldown: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BreakerStateKind {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug)]
struct BreakerState {
    kind: BreakerStateKind,
    /// Counter for `Closed`: consecutive failures observed since the
    /// last success. Reset to 0 on any success.
    consecutive_failures: u32,
    /// For `Open`: when the cooldown expires. Other states ignore.
    open_until: Option<Instant>,
    /// For `HalfOpen`: whether a probe is already in flight (prevents
    /// concurrent probes from all reaching the inner provider when N
    /// callers race the cooldown expiry).
    probe_in_flight: bool,
}

impl BreakerState {
    fn closed() -> Self {
        Self {
            kind: BreakerStateKind::Closed,
            consecutive_failures: 0,
            open_until: None,
            probe_in_flight: false,
        }
    }

    fn open(now: Instant, cooldown: Duration) -> Self {
        Self {
            kind: BreakerStateKind::Open,
            consecutive_failures: 0,
            open_until: Some(now + cooldown),
            probe_in_flight: false,
        }
    }
}

enum Decision {
    Allow,
    Reject { until: Instant },
}

/// Wrap a single provider with circuit-breaker semantics.
///
/// State lives on the wrapper instance, so the standard idiom is
/// "instantiate once per provider at registry-build time" — wrapping
/// the same underlying provider twice gives you two independent
/// breakers, which is almost never what you want. Use
/// [`CircuitBreaker::wrap`] to get an `Arc<dyn LlmProvider>` you can
/// drop into a `ProviderRegistry` slot.
pub struct CircuitBreaker {
    inner: Arc<dyn LlmProvider>,
    /// Cached so we can return `&ProviderId` without re-borrowing.
    id: ProviderId,
    config: CircuitBreakerConfig,
    state: Mutex<BreakerState>,
}

impl CircuitBreaker {
    /// Construct + return as `Arc<dyn LlmProvider>` — slots straight
    /// into a registry / routing candidate list.
    pub fn wrap(
        provider: Arc<dyn LlmProvider>,
        config: CircuitBreakerConfig,
    ) -> Arc<dyn LlmProvider> {
        let id = provider.id().clone();
        Arc::new(Self {
            inner: provider,
            id,
            config,
            state: Mutex::new(BreakerState::closed()),
        })
    }

    fn check(&self, now: Instant) -> Decision {
        let mut state = self.state.lock().expect("breaker state poisoned");
        match state.kind {
            BreakerStateKind::Closed => Decision::Allow,
            BreakerStateKind::Open => {
                let until = state.open_until.expect("open without expiry");
                if now >= until {
                    // Cooldown expired — transition to HalfOpen and
                    // mark this caller as the probe.
                    state.kind = BreakerStateKind::HalfOpen;
                    state.probe_in_flight = true;
                    state.consecutive_failures = 0;
                    state.open_until = None;
                    Decision::Allow
                } else {
                    Decision::Reject { until }
                }
            }
            BreakerStateKind::HalfOpen => {
                if state.probe_in_flight {
                    // Another caller is already probing — keep them
                    // waiting via reject so we don't hammer a possibly-
                    // still-broken provider.
                    let next_check = now + Duration::from_millis(100);
                    Decision::Reject { until: next_check }
                } else {
                    state.probe_in_flight = true;
                    Decision::Allow
                }
            }
        }
    }

    fn record_success(&self) {
        let mut state = self.state.lock().expect("breaker state poisoned");
        match state.kind {
            BreakerStateKind::Closed => {
                state.consecutive_failures = 0;
            }
            BreakerStateKind::HalfOpen => {
                tracing::info!(
                    provider_id = %self.id,
                    "circuit_breaker: probe succeeded → Closed",
                );
                *state = BreakerState::closed();
            }
            BreakerStateKind::Open => {
                // Shouldn't happen — Open rejects before dispatch — but
                // be defensive: collapse to Closed so we don't leak a
                // stuck state if the contract is ever violated.
                tracing::warn!(
                    provider_id = %self.id,
                    "circuit_breaker: success while Open — resetting to Closed",
                );
                *state = BreakerState::closed();
            }
        }
    }

    fn record_failure(&self, now: Instant) {
        let mut state = self.state.lock().expect("breaker state poisoned");
        match state.kind {
            BreakerStateKind::Closed => {
                state.consecutive_failures =
                    state.consecutive_failures.saturating_add(1);
                if state.consecutive_failures >= self.config.failure_threshold {
                    tracing::warn!(
                        provider_id = %self.id,
                        consecutive_failures = state.consecutive_failures,
                        cooldown_secs = self.config.cooldown.as_secs(),
                        "circuit_breaker: failure threshold reached → Open",
                    );
                    *state = BreakerState::open(now, self.config.cooldown);
                }
            }
            BreakerStateKind::HalfOpen => {
                tracing::warn!(
                    provider_id = %self.id,
                    "circuit_breaker: probe failed → Open (fresh cooldown)",
                );
                *state = BreakerState::open(now, self.config.cooldown);
            }
            BreakerStateKind::Open => {
                // Same defensive note as in record_success.
                tracing::warn!(
                    provider_id = %self.id,
                    "circuit_breaker: failure while Open — extending cooldown",
                );
                *state = BreakerState::open(now, self.config.cooldown);
            }
        }
    }

    /// Snapshot for tests / introspection. Not part of the trait
    /// because no production caller needs it.
    #[cfg(test)]
    fn current_kind(&self) -> BreakerStateKind {
        self.state.lock().unwrap().kind
    }
}

#[async_trait]
impl LlmProvider for CircuitBreaker {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let now = Instant::now();
        match self.check(now) {
            Decision::Reject { until } => {
                tracing::debug!(
                    provider_id = %self.id,
                    until_ms = until.saturating_duration_since(now).as_millis() as u64,
                    "circuit_breaker: rejecting (Open)",
                );
                return Err(ProviderError::CircuitOpen { until });
            }
            Decision::Allow => {}
        }

        let inner = self.inner.clone();
        let result = inner.stream(req, ctx).await;
        match &result {
            Ok(_) => self.record_success(),
            Err(_) => self.record_failure(Instant::now()),
        }
        result
    }

    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<ChatResponse, ProviderError> {
        // Forward to the inner provider's complete() rather than going
        // through the default-impl `stream` route — preserves any
        // optimization the inner provider has (no extra round-trip
        // through the breaker check, since stream() already handled it).
        let now = Instant::now();
        match self.check(now) {
            Decision::Reject { until } => {
                return Err(ProviderError::CircuitOpen { until });
            }
            Decision::Allow => {}
        }

        let inner = self.inner.clone();
        let result = inner.complete(req, ctx).await;
        match &result {
            Ok(_) => self.record_success(),
            Err(_) => self.record_failure(Instant::now()),
        }
        result
    }

    fn cost(&self, usage: &Usage) -> CostUsd {
        self.inner.cost(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::ModelHint;

    /// Fake provider with deterministic outcome. Same shape as the
    /// `routing.rs` test helper but local — keeps the breaker tests
    /// self-contained.
    struct ScriptedProvider {
        id: ProviderId,
        outcome: ScriptedOutcome,
        calls: Arc<AtomicU32>,
        capabilities: Capabilities,
    }
    enum ScriptedOutcome {
        Ok,
        Err(fn() -> ProviderError),
    }
    #[async_trait]
    impl LlmProvider for ScriptedProvider {
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.outcome {
                ScriptedOutcome::Ok => {
                    let mock = MockProvider::new(self.id.clone(), CannedResponse::text("ok"));
                    mock.stream(req, ctx).await
                }
                ScriptedOutcome::Err(f) => Err(f()),
            }
        }
    }

    fn scripted(id: &str, outcome: ScriptedOutcome) -> (Arc<ScriptedProvider>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let p = Arc::new(ScriptedProvider {
            id: ProviderId::new(id),
            outcome,
            calls: calls.clone(),
            capabilities: Capabilities::text_only_baseline(tars_types::Pricing::default()),
        });
        (p, calls)
    }

    fn req() -> ChatRequest {
        ChatRequest::user(ModelHint::Explicit("m".into()), "ping")
    }

    fn ctx() -> RequestContext {
        RequestContext::test_default()
    }

    async fn drain(stream: LlmEventStream) {
        let mut s = stream;
        while s.next().await.is_some() {}
    }

    fn config_open_after(n: u32) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: n,
            cooldown: Duration::from_secs(30),
        }
    }

    // ── Closed → Open after threshold ────────────────────────────────
    #[tokio::test]
    async fn closed_opens_after_threshold_consecutive_failures() {
        let (inner, calls) = scripted("p", ScriptedOutcome::Err(|| ProviderError::ModelOverloaded));
        let breaker = CircuitBreaker::wrap(inner, config_open_after(3));

        for _ in 0..3 {
            let r = breaker.clone().stream(req(), ctx()).await;
            assert!(matches!(r, Err(ProviderError::ModelOverloaded)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3, "all 3 hit the inner provider");

        // 4th call should reject without hitting inner.
        let r = breaker.clone().stream(req(), ctx()).await;
        assert!(matches!(r, Err(ProviderError::CircuitOpen { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 3, "breaker rejected; inner not called");
    }

    // ── Closed counter resets on success ─────────────────────────────
    #[tokio::test]
    async fn success_resets_consecutive_failure_counter() {
        // We need a provider that fails twice, then succeeds, then
        // fails twice more. With threshold=3, the breaker should NOT
        // open (the success resets the counter).
        struct Flaky {
            id: ProviderId,
            sequence: Mutex<Vec<bool>>, // true = ok, false = err
            capabilities: Capabilities,
        }
        #[async_trait]
        impl LlmProvider for Flaky {
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
                let next = self.sequence.lock().unwrap().remove(0);
                if next {
                    let mock = MockProvider::new(self.id.clone(), CannedResponse::text("ok"));
                    mock.stream(req, ctx).await
                } else {
                    Err(ProviderError::ModelOverloaded)
                }
            }
        }
        let flaky: Arc<dyn LlmProvider> = Arc::new(Flaky {
            id: ProviderId::new("p"),
            sequence: Mutex::new(vec![false, false, true, false, false]),
            capabilities: Capabilities::text_only_baseline(tars_types::Pricing::default()),
        });
        let breaker = CircuitBreaker::wrap(flaky, config_open_after(3));

        for _ in 0..5 {
            let _ = breaker.clone().stream(req(), ctx()).await;
        }

        // The inner sequence: F F S F F → counter went 1, 2, reset, 1, 2.
        // Never hit threshold. Breaker still Closed.
        let snapshot = breaker
            .clone();
        // Cast back to concrete CircuitBreaker via Arc downcasting.
        // We can't downcast Arc<dyn LlmProvider>, so re-implement the
        // assertion path: the next call must still hit the inner
        // provider (no reject).
        let (next_inner, next_calls) = scripted("does-not-matter", ScriptedOutcome::Ok);
        // The above is just a placeholder — what we actually want is
        // to confirm the breaker hasn't opened. Easiest way: try one
        // more call against the breaker; if it rejects with
        // CircuitOpen, the breaker tripped (failure). If it returns
        // some other Result (Ok or the inner's Err), we're still
        // Closed.
        let _ = (snapshot, next_inner, next_calls); // suppress unused

        // Inner's sequence is exhausted; pushing one more would panic.
        // Replenish via a direct check on the breaker's internals…
        // ...except we can't, because the breaker is type-erased. So
        // we test this property differently: the next test does the
        // direct-introspection version.
    }

    // Direct-introspection variant (works because we keep CircuitBreaker
    // concrete at construction).
    #[tokio::test]
    async fn success_resets_counter_introspected() {
        // Failure sequence: F F → counter=2, S → counter=0, F F → counter=2.
        // Threshold=3 → never opens.
        struct Flaky {
            id: ProviderId,
            sequence: Mutex<Vec<bool>>,
            capabilities: Capabilities,
        }
        #[async_trait]
        impl LlmProvider for Flaky {
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
                let next = self.sequence.lock().unwrap().remove(0);
                if next {
                    let mock = MockProvider::new(self.id.clone(), CannedResponse::text("ok"));
                    mock.stream(req, ctx).await
                } else {
                    Err(ProviderError::ModelOverloaded)
                }
            }
        }
        let flaky: Arc<dyn LlmProvider> = Arc::new(Flaky {
            id: ProviderId::new("p"),
            sequence: Mutex::new(vec![false, false, true, false, false]),
            capabilities: Capabilities::text_only_baseline(tars_types::Pricing::default()),
        });
        // Build the CircuitBreaker directly so we can introspect.
        let id = flaky.id().clone();
        let cb = Arc::new(CircuitBreaker {
            inner: flaky,
            id,
            config: config_open_after(3),
            state: Mutex::new(BreakerState::closed()),
        });
        for _ in 0..5 {
            let r = cb.clone().stream(req(), ctx()).await;
            // Drain whatever stream came back so the canned-response's
            // singleflight doesn't choke (no real requirement).
            if let Ok(s) = r {
                drain(s).await;
            }
        }
        assert_eq!(
            cb.current_kind(),
            BreakerStateKind::Closed,
            "non-consecutive failures should not open the breaker",
        );
    }

    // ── Open → HalfOpen after cooldown ───────────────────────────────
    //
    // Uses a real (short) cooldown rather than tokio::time::pause —
    // the breaker reads `std::time::Instant::now()` directly and
    // tokio's clock pause only affects tokio's own primitives
    // (sleep / timeout / Instant). Real-time tests with sub-100ms
    // cooldowns are fast enough not to dominate the suite.
    #[tokio::test(flavor = "current_thread")]
    async fn open_transitions_to_half_open_after_cooldown() {
        let (inner, _calls) =
            scripted("p", ScriptedOutcome::Err(|| ProviderError::ModelOverloaded));
        let id = inner.id().clone();
        let cb = Arc::new(CircuitBreaker {
            inner,
            id,
            config: CircuitBreakerConfig {
                failure_threshold: 1,
                cooldown: Duration::from_millis(60),
            },
            state: Mutex::new(BreakerState::closed()),
        });

        // Trip the breaker.
        let _ = cb.clone().stream(req(), ctx()).await;
        assert_eq!(cb.current_kind(), BreakerStateKind::Open);

        // Within cooldown: still rejects.
        let r = cb.clone().stream(req(), ctx()).await;
        assert!(matches!(r, Err(ProviderError::CircuitOpen { .. })));
        assert_eq!(cb.current_kind(), BreakerStateKind::Open);

        // After cooldown: next call probes (HalfOpen) — and since the
        // inner still errors, the probe fails and we go back to Open.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let r = cb.clone().stream(req(), ctx()).await;
        assert!(
            matches!(r, Err(ProviderError::ModelOverloaded)),
            "first call after cooldown is the probe; inner still errors",
        );
        assert_eq!(cb.current_kind(), BreakerStateKind::Open);
    }

    // ── HalfOpen probe success → Closed ──────────────────────────────
    #[tokio::test(flavor = "current_thread")]
    async fn half_open_probe_success_closes_breaker() {
        // Inner: error once (trip), then succeed. We use the Flaky
        // pattern again for sequenced outcomes.
        struct Flaky {
            id: ProviderId,
            sequence: Mutex<Vec<bool>>,
            capabilities: Capabilities,
        }
        #[async_trait]
        impl LlmProvider for Flaky {
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
                let next = self.sequence.lock().unwrap().remove(0);
                if next {
                    MockProvider::new(self.id.clone(), CannedResponse::text("ok"))
                        .stream(req, ctx)
                        .await
                } else {
                    Err(ProviderError::ModelOverloaded)
                }
            }
        }
        let flaky: Arc<dyn LlmProvider> = Arc::new(Flaky {
            id: ProviderId::new("p"),
            sequence: Mutex::new(vec![false, true, true]),
            capabilities: Capabilities::text_only_baseline(tars_types::Pricing::default()),
        });
        let id = flaky.id().clone();
        let cb = Arc::new(CircuitBreaker {
            inner: flaky,
            id,
            config: CircuitBreakerConfig {
                failure_threshold: 1,
                cooldown: Duration::from_millis(60),
            },
            state: Mutex::new(BreakerState::closed()),
        });

        // Trip
        let _ = cb.clone().stream(req(), ctx()).await;
        assert_eq!(cb.current_kind(), BreakerStateKind::Open);

        // Skip cooldown (real wall clock — see note on the previous test)
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Probe (success → Closed)
        let r = cb.clone().stream(req(), ctx()).await;
        assert!(r.is_ok());
        if let Ok(s) = r {
            drain(s).await;
        }
        assert_eq!(cb.current_kind(), BreakerStateKind::Closed);

        // Subsequent calls flow normally.
        let r = cb.clone().stream(req(), ctx()).await;
        assert!(r.is_ok());
    }

    // ── Reject error class is Retriable so RoutingService falls through
    #[test]
    fn circuit_open_classifies_as_retriable() {
        use tars_types::ErrorClass;
        let until = Instant::now() + Duration::from_secs(30);
        let e = ProviderError::CircuitOpen { until };
        assert_eq!(e.class(), ErrorClass::Retriable);
        // retry_after roughly matches the cooldown remaining.
        let ra = e.retry_after().unwrap();
        assert!(ra <= Duration::from_secs(31));
        assert!(ra >= Duration::from_secs(28));
    }

    // ── id / capabilities pass-through
    #[test]
    fn wrap_preserves_id_and_capabilities() {
        let (inner, _) = scripted("inner_id", ScriptedOutcome::Ok);
        let inner_caps_marker = inner.capabilities().max_context_tokens;
        let cb = CircuitBreaker::wrap(inner, CircuitBreakerConfig::default());
        assert_eq!(cb.id().as_ref(), "inner_id");
        assert_eq!(cb.capabilities().max_context_tokens, inner_caps_marker);
    }
}
