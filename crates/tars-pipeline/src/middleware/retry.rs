//! Retry middleware — exponential backoff at *open* time only.
//!
//! Mid-stream retries are **out of scope** for the M1 implementation
//! and likely forever: once the provider has emitted any [`ChatEvent`]
//! the consumer has already started observing the response (text
//! deltas, tool-call starts) and replaying from scratch would either
//! double-emit those events or require provider-side sequence numbers
//! we don't have. Mid-stream errors propagate untouched to the caller.
//!
//! Retry decisions are driven entirely by
//! [`tars_types::ProviderError::class`]:
//!
//! - [`ErrorClass::Permanent`] — never retry (auth, invalid request,
//!   content filter, budget, context overflow).
//! - [`ErrorClass::Retriable`] — retry up to `max_attempts` (rate
//!   limit, model overloaded, network).
//! - [`ErrorClass::MaybeRetriable`] — retry once. Anything more is
//!   risky for parse / internal / subprocess failures.
//!
//! Backoff is exponential; if the error carries a `retry_after`
//! (Anthropic / OpenAI Retry-After header) we honour that instead of
//! computing our own — the provider knows its own load shape better
//! than we do.
//!
//! Optional **jitter** ([`RetryConfig::jitter`], off by default) adds a
//! small pseudo-random offset to each *computed* backoff so that many
//! independently-started clients sharing one rate-limited provider don't
//! retry in lockstep (a thundering herd that re-collides every round).
//! It is NOT applied to an explicit `Retry-After` — there the provider
//! is dictating the cadence and a herd it told to wait `N`s should wait
//! `N`s. Single-client callers leave it at zero.
//!
//! `max_wait` caps how long we'll honour a `Retry-After`. Past the
//! cap, we bubble the error unchanged so an outer FallbackMiddleware
//! (or the caller) can decide — agents should never sleep 30 minutes
//! inside a single call. See `docs/roadmap.md §1`.

use std::time::Duration;

use async_trait::async_trait;

use tars_provider::LlmEventStream;
use tars_types::{ChatRequest, ErrorClass, ProviderError, RequestContext};

use crate::middleware::Middleware;
use crate::service::Next;

#[derive(Clone, Debug)]
pub struct RetryConfig {
    /// Total attempts including the first try. `1` disables retry.
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub multiplier: f64,
    /// If true, prefer `ProviderError::retry_after()` over our own
    /// computed backoff when the error carries one.
    pub respect_retry_after: bool,
    /// Cap for [`ErrorClass::MaybeRetriable`] — these errors smell like
    /// adapter bugs (parse failures, subprocess crashes); one extra
    /// attempt is often the right call, but more is throwing good
    /// effort after bad.
    pub max_attempts_maybe_retriable: u32,
    /// Upper bound on how long we'll wait between attempts — applies to
    /// both `Retry-After` headers and computed backoff. If the wait we'd
    /// pick exceeds this, the error bubbles up unchanged so an outer
    /// `FallbackMiddleware` (or the caller) can switch providers
    /// instead of sleeping. Default: 30 s. Set to `Duration::MAX` to
    /// disable (don't — agents shouldn't sleep for minutes).
    pub max_wait: Duration,
    /// Maximum random jitter ADDED to each *computed* backoff, to de-sync
    /// many concurrent clients that would otherwise retry in lockstep
    /// (thundering herd against a rate-limited provider). Each attempt's
    /// actual jitter is a pseudo-random value in `[0, jitter)`, derived
    /// from the wall-clock sub-millisecond fraction so independently-
    /// started workers land on different offsets without a PRNG
    /// dependency. NOT applied to an explicit `Retry-After` wait (the
    /// provider's cadence is authoritative). Default `ZERO` — disabled,
    /// so single-client behaviour and existing tests are unchanged.
    pub jitter: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(30),
            multiplier: 2.0,
            respect_retry_after: true,
            max_attempts_maybe_retriable: 2,
            max_wait: Duration::from_secs(30),
            jitter: Duration::ZERO,
        }
    }
}

/// Attach retry semantics to whatever inner service you compose it over.
/// In the canonical pipeline this sits **just above** the Provider so
/// that Routing / Circuit Breaker can react to the *final* outcome
/// after retries have exhausted.
#[derive(Clone, Debug, Default)]
pub struct RetryMiddleware {
    config: RetryConfig,
}

impl RetryMiddleware {
    pub fn new(config: RetryConfig) -> Self {
        Self { config }
    }

    /// Convenience for tests — disable backoff so polling tests stay fast.
    pub fn no_backoff(max_attempts: u32) -> Self {
        Self {
            config: RetryConfig {
                max_attempts,
                initial_backoff: Duration::ZERO,
                max_backoff: Duration::ZERO,
                multiplier: 1.0,
                respect_retry_after: false,
                max_attempts_maybe_retriable: max_attempts,
                // Tests don't exercise long Retry-After bubbling; the
                // dedicated test for that uses an explicit config.
                max_wait: Duration::MAX,
                // No jitter — polling tests advance the paused clock by
                // an exact backoff and must land deterministically.
                jitter: Duration::ZERO,
            },
        }
    }
}

#[async_trait]
impl Middleware for RetryMiddleware {
    fn name(&self) -> &'static str {
        "retry"
    }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        let cfg = &self.config;
        // Clamp initial backoff to max so a misconfigured
        // `initial_backoff > max_backoff` can't make the *first* retry
        // wait longer than every subsequent one (which `next_backoff`
        // caps at `max_backoff`) — backoff should never decrease.
        let mut backoff = cfg.initial_backoff.min(cfg.max_backoff);
        let mut attempt: u32 = 0;
        // Record this layer in the telemetry trace. Best-effort: a
        // poisoned lock (another holder panicked) must not fail the call,
        // but leave a breadcrumb rather than dropping silently.
        match ctx.telemetry.lock() {
            Ok(mut t) => t.layers.push("retry".into()),
            Err(_) => tracing::trace!(
                event = "retry.telemetry_poisoned",
                "telemetry mutex poisoned; skipping layer trace"
            ),
        }
        loop {
            attempt += 1;
            // Honour cancellation between attempts (the very first try
            // also checks — caller may have cancelled before we ran).
            if ctx.cancel.is_cancelled() {
                return Err(ProviderError::Internal("cancelled before retry".into()));
            }

            let result = next.run(req.clone(), ctx.clone()).await;
            let err = match result {
                Ok(stream) => return Ok(stream),
                Err(e) => e,
            };

            let class = err.class();
            let cap = match class {
                ErrorClass::Permanent => {
                    tracing::debug!(
                        attempt, error = %err, "retry: permanent — giving up",
                    );
                    return Err(err);
                }
                // `.max(1)` so a misconfigured `max_attempts = 0` still
                // allows the single attempt we already made, rather than
                // behaving as a hard "never even try once" cap.
                ErrorClass::Retriable => cfg.max_attempts.max(1),
                ErrorClass::MaybeRetriable => cfg.max_attempts_maybe_retriable.max(1),
            };
            if attempt >= cap {
                tracing::debug!(
                    attempt, cap, error = %err, "retry: exhausted attempts",
                );
                return Err(err);
            }

            // Track whether the provider's explicit Retry-After drove the
            // wait this round. If it did, we must NOT escalate our own
            // exponential `backoff` — the provider is dictating the
            // cadence, and growing `backoff` underneath would over-inflate
            // the delay the moment a later attempt comes back *without* a
            // Retry-After. Exponential growth only applies to our own
            // computed backoff.
            let (wait, used_retry_after) = if cfg.respect_retry_after {
                match err.retry_after() {
                    Some(ra) => (ra, true),
                    None => (with_jitter(backoff, cfg.jitter), false),
                }
            } else {
                (with_jitter(backoff, cfg.jitter), false)
            };

            // Don't sleep past the cap — bubble the error so an outer
            // FallbackMiddleware (or the caller) can switch providers
            // instead. Agents are not meant to sleep for minutes
            // inside a single call.
            if wait > cfg.max_wait {
                tracing::debug!(
                    attempt,
                    wait_ms = millis_u64(wait),
                    max_wait_ms = millis_u64(cfg.max_wait),
                    error = %err,
                    "retry: wait exceeds max_wait — bubbling error for fallback / caller",
                );
                return Err(err);
            }

            tracing::debug!(
                attempt,
                next_attempt = attempt + 1,
                wait_ms = millis_u64(wait),
                error = %err,
                "retry: backing off",
            );

            // Telemetry: record this failed attempt before we sleep.
            // `retry_count` increments AFTER the wait commits — so it
            // tracks "attempts retried", which is the natural caller
            // intuition (if the next attempt also fails terminally,
            // retry_count still reflects the count of *retries that
            // happened* before the final failure).
            match ctx.telemetry.lock() {
                Ok(mut t) => {
                    t.retry_count = t.retry_count.saturating_add(1);
                    t.retry_attempts.push(tars_types::RetryAttempt {
                        error_kind: provider_error_kind(&err),
                        retry_after_ms: Some(millis_u64(wait)),
                    });
                }
                Err(_) => tracing::trace!(
                    event = "retry.telemetry_poisoned",
                    "telemetry mutex poisoned; skipping retry attempt record"
                ),
            }

            // Cancel-aware sleep.
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => {
                    return Err(ProviderError::Internal(
                        "cancelled during retry backoff".into(),
                    ));
                }
                _ = tokio::time::sleep(wait) => {}
            }

            // Exponential backoff for the next attempt — never above max,
            // and panic-safe against a bad `multiplier` (see next_backoff).
            // Only escalate when *we* chose the wait; an explicit
            // Retry-After must not bump our computed backoff (see above).
            if !used_retry_after {
                backoff = next_backoff(backoff, cfg.multiplier, cfg.max_backoff);
            }
        }
    }
}

/// Typed kind tag matching `tars-py`'s `TarsProviderError.kind`
/// Wrapper around [`ProviderError::kind`] kept for call-site clarity
/// and so a future `provider_error_kind` divergence (if it ever needs
/// to differ from the canonical kind) doesn't have to update every
/// callsite.
fn provider_error_kind(err: &ProviderError) -> tars_types::ProviderErrorKind {
    err.kind()
}

/// `Duration::as_millis()` is `u128`; telemetry / log fields want `u64`.
/// Saturate instead of the silent wrap an `as u64` truncation would do
/// for pathological durations (e.g. a `Duration::MAX` `max_wait`).
fn millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Next exponential backoff, capped at `max`. `Duration::mul_f64`
/// panics on a negative, NaN, or overflowing product, and `RetryConfig`
/// doesn't validate `multiplier`, so guard the arithmetic here: a
/// non-finite/negative multiplier or an overflowing product collapses
/// to `max` rather than panicking inside the retry loop.
/// Add a pseudo-random `[0, jitter)` offset to a computed backoff so
/// concurrent clients de-sync. The randomness source is the wall-clock
/// sub-millisecond fraction (`subsec_micros`): dep-free (no `rand`), and
/// independently-started workers reach this point at different instants
/// so they read different offsets — which is all jitter needs (de-sync,
/// not cryptographic randomness). The jittered value feeds only this
/// attempt's sleep; the exponential `backoff` itself escalates from the
/// un-jittered base, so jitter never compounds. A zero ceiling (the
/// default) or a frozen/unavailable clock yields the base unchanged,
/// keeping `start_paused` tests deterministic.
fn with_jitter(base: Duration, jitter: Duration) -> Duration {
    let ceil_ms = u64::try_from(jitter.as_millis()).unwrap_or(u64::MAX);
    if ceil_ms == 0 {
        return base;
    }
    let offset_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_micros()) % ceil_ms)
        .unwrap_or(0);
    base + Duration::from_millis(offset_ms)
}

fn next_backoff(current: Duration, multiplier: f64, max: Duration) -> Duration {
    if !multiplier.is_finite() || multiplier < 0.0 {
        return max;
    }
    let scaled = current.as_secs_f64() * multiplier;
    if !scaled.is_finite() {
        return max;
    }
    Duration::try_from_secs_f64(scaled).unwrap_or(max).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmService;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ChatEvent, ModelHint};

    /// A fake `LlmService` whose first N calls return an error of the
    /// caller's choosing; subsequent calls delegate to a Mock provider.
    struct FailNTimes {
        remaining: AtomicU32,
        error_factory: Box<dyn Fn() -> ProviderError + Send + Sync>,
        ok_inner: Arc<dyn Service>,
        observed: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Service for FailNTimes {
        async fn call(
            self: Arc<Self>,
            req: ChatRequest,
                    model: &str,
        ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            self.observed.fetch_add(1, Ordering::SeqCst);
            // Atomically decrement only while a failure budget remains —
            // a `load`-then-`fetch_sub` would race under concurrent
            // callers and could decrement past zero.
            let decremented = self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n > 0).then(|| n - 1)
                })
                .is_ok();
            if decremented {
                return Err((self.error_factory)());
            }
            self.ok_inner.clone().call(req, model, ctx).await
        }
    }

    fn build(
        fails: u32,
        err: impl Fn() -> ProviderError + Send + Sync + 'static,
        retry: RetryMiddleware,
    ) -> (Arc<dyn Service>, Arc<AtomicU32>) {
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let inner: Arc<dyn Service> = LlmService::of(mock, "test-model").chain();
        let observed = Arc::new(AtomicU32::new(0));
        let failer: Arc<dyn Service> = Arc::new(FailNTimes {
            remaining: AtomicU32::new(fails),
            error_factory: Box::new(err),
            ok_inner: inner,
            observed: observed.clone(),
        });
        (retry.wrap(failer), observed)
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retries_then_succeeds_for_retriable_errors() {
        let (svc, observed) = build(
            2,
            || ProviderError::ModelOverloaded,
            RetryMiddleware::no_backoff(5),
        );
        let mut s = svc
            .call(
                ChatRequest::user("x"), "m",
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        while let Some(ev) = s.next().await {
            ev.unwrap();
        }
        // 2 failures + 1 success = 3 inner calls.
        assert_eq!(observed.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn jitter_zero_is_identity_and_bounded_when_set() {
        let base = Duration::from_secs(4);
        // Zero ceiling (the default) must not perturb the backoff —
        // this is what keeps `start_paused` polling tests deterministic.
        assert_eq!(with_jitter(base, Duration::ZERO), base);
        // A set ceiling adds an offset within `[0, jitter)`: never below
        // base, never `base + jitter` or beyond. Sample a few reads (the
        // wall clock advances between them) to exercise the range.
        let ceil = Duration::from_millis(400);
        for _ in 0..50 {
            let w = with_jitter(base, ceil);
            assert!(w >= base, "jitter must not shrink the wait");
            assert!(w < base + ceil, "jitter must stay under the ceiling");
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn never_retries_permanent_errors() {
        let (svc, observed) = build(
            5,
            || ProviderError::Auth("bad key".into()),
            RetryMiddleware::no_backoff(5),
        );
        let err = match svc
            .call(
                ChatRequest::user("x"), "m",
                RequestContext::test_default(),
            )
            .await
        {
            Ok(_) => panic!("expected Auth error, got success"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::Auth(_)));
        // Failed on first call; no retry.
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn caps_maybe_retriable_at_dedicated_limit() {
        // max_attempts = 5 but max_attempts_maybe_retriable = 2 → only
        // two tries on a Parse error.
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
            respect_retry_after: false,
            max_attempts_maybe_retriable: 2,
            max_wait: Duration::MAX,
            jitter: Duration::ZERO,
        };
        let (svc, observed) = build(
            10,
            || ProviderError::Parse("bad".into()),
            RetryMiddleware::new(cfg),
        );
        let result = svc
            .call(
                ChatRequest::user("x"), "m",
                RequestContext::test_default(),
            )
            .await;
        assert!(result.is_err(), "expected Parse error after retries");
        assert_eq!(observed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_during_backoff_aborts_loop() {
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::from_secs(60), // long enough to cancel
            max_backoff: Duration::from_secs(60),
            multiplier: 1.0,
            respect_retry_after: false,
            max_attempts_maybe_retriable: 5,
            // Test asserts cancel during sleep — needs the sleep to
            // actually happen, so allow long waits.
            max_wait: Duration::MAX,
            jitter: Duration::ZERO,
        };
        let (svc, observed) = build(
            10,
            || ProviderError::ModelOverloaded,
            RetryMiddleware::new(cfg),
        );
        let ctx = RequestContext::test_default();
        let cancel = ctx.cancel.clone();

        // Cancel after first failure but during backoff.
        let task = tokio::spawn(async move {
            svc.call(ChatRequest::user("x"), "m", ctx)
                .await
        });

        // Let the first attempt happen + enter backoff.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();

        let err = match task.await.unwrap() {
            Ok(_) => panic!("expected cancellation error"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::Internal(ref m) if m.contains("cancel")));
        // First attempt observed; cancel kicked in before any retry.
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bubbles_when_retry_after_exceeds_max_wait() {
        // Provider says "wait 5 minutes", but we cap at 10 s — bubble
        // the original error so an outer FallbackMiddleware can
        // switch providers instead of sleeping.
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
            respect_retry_after: true,
            max_attempts_maybe_retriable: 5,
            max_wait: Duration::from_secs(10),
            jitter: Duration::ZERO,
        };
        let (svc, observed) = build(
            10,
            || ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(300)),
            },
            RetryMiddleware::new(cfg),
        );
        let err = svc
            .call(
                ChatRequest::user("x"), "m",
                RequestContext::test_default(),
            )
            .await
            .err()
            .expect("bubble RateLimited");
        // Must surface the *original* RateLimited so FallbackMiddleware
        // (or the caller) can pattern-match on `retry_after`.
        match err {
            ProviderError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(300)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // First attempt ran; max_wait check fired before any sleep.
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn honors_retry_after_within_max_wait() {
        // Symmetric case: Retry-After fits under max_wait → normal retry.
        // (Sibling test to the bubble case so the boundary behavior is
        // pinned from both sides.)
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
            respect_retry_after: true,
            max_attempts_maybe_retriable: 5,
            max_wait: Duration::from_secs(10),
            jitter: Duration::ZERO,
        };
        let (svc, observed) = build(
            1,
            || ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
            },
            RetryMiddleware::new(cfg),
        );
        let ctx = RequestContext::test_default();
        let req = ChatRequest::user("x");
        let task = tokio::spawn(svc.call(req, "m", ctx));

        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(3)).await;
        let mut stream = task.await.unwrap().unwrap();
        while let Some(ev) = stream.next().await {
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                break;
            }
        }
        assert_eq!(observed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn honors_retry_after_when_present() {
        // Construct a RateLimited with a 2s retry_after. With paused
        // time, sleep is observable via `tokio::time::advance`.
        let cfg = RetryConfig::default();
        let (svc, observed) = build(
            1,
            || ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
            },
            RetryMiddleware::new(cfg),
        );
        let ctx = RequestContext::test_default();
        let req = ChatRequest::user("x");
        let task = tokio::spawn(svc.call(req, "m", ctx));

        // Without advancing time the sleep would block forever in
        // start_paused mode. Advance past 2s and the retry should fire.
        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(3)).await;
        let mut stream = task.await.unwrap().unwrap();
        while let Some(ev) = stream.next().await {
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                break;
            }
        }
        assert_eq!(observed.load(Ordering::SeqCst), 2);
    }
}
