//! Retries a call that fails to open, with exponential backoff.
//!
//! An error after the stream has opened is passed through untouched: the consumer has already
//! seen events, and replaying from scratch would emit them twice.
//!
//! Whether to retry follows [`ProviderError::class`](crate::ProviderError::class):
//! [`ErrorClass::Permanent`] never, [`ErrorClass::Transient`] up to
//! [`RetryConfig::max_attempts`]. How long an error says the provider stays unavailable (a
//! `Retry-After`, a provider out until a time) replaces the computed backoff. A wait longer than [`RetryConfig::max_wait`] returns the error unchanged,
//! so the caller can try another provider instead of sleeping inside one call; so does a wait that
//! ends past the caller's deadline.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::LlmError;
use crate::LlmEventStream;
use futures::StreamExt;
use tars_types::{CallTelemetry, Caller, ChatEvent, ChatRequest, ErrorClass, RetryAttempt};
use tars_utils::display_chain;

use crate::pipeline::middleware::Middleware;
use crate::pipeline::service::{Next, OpenFailure};

/// Configures [`RetryMiddleware`]. The default makes 3 attempts, starts at 200 ms, doubles, and
/// waits at most 30 s. It reads from the `[llm.retry]` config section, each duration in
/// milliseconds (`initial_backoff_ms`); a key left out keeps its default.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    /// Total attempts including the first; `1` disables retry and `0` acts as `1`.
    pub max_attempts: u32,
    #[serde(rename = "initial_backoff_ms", with = "crate::duration_serde::millis")]
    pub initial_backoff: Duration,
    #[serde(rename = "max_backoff_ms", with = "crate::duration_serde::millis")]
    pub max_backoff: Duration,
    pub multiplier: f64,
    /// Whether an error's `unavailable_for()` replaces the computed backoff.
    pub respect_retry_after: bool,
    /// The longest wait between attempts, for a `Retry-After` and a computed backoff alike. A
    /// longer wait returns the error unchanged; `Duration::MAX` removes the bound, and is the
    /// one value the config file cannot write.
    #[serde(rename = "max_wait_ms", with = "crate::duration_serde::millis")]
    pub max_wait: Duration,
    /// The ceiling of a pseudo-random offset added to each computed backoff, so many clients of
    /// one rate-limited provider do not retry in lockstep. It is not added to a `Retry-After`,
    /// whose cadence is the provider's. `ZERO` turns it off.
    #[serde(rename = "jitter_ms", with = "crate::duration_serde::millis")]
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
            max_wait: Duration::from_secs(30),
            jitter: Duration::ZERO,
        }
    }
}

/// Retries the rest of the chain per its [`RetryConfig`].
#[derive(Clone, Debug, Default)]
pub struct RetryMiddleware {
    config: RetryConfig,
}

impl RetryMiddleware {
    pub fn new(config: RetryConfig) -> Self {
        Self { config }
    }

    /// Returns a retry with no backoff, no `Retry-After` and no `max_wait`, for tests.
    pub fn no_backoff(max_attempts: u32) -> Self {
        Self {
            config: RetryConfig {
                max_attempts,
                initial_backoff: Duration::ZERO,
                max_backoff: Duration::ZERO,
                multiplier: 1.0,
                respect_retry_after: false,
                max_wait: Duration::MAX,
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
        ctx: Caller,
        next: Next<'_>,
    ) -> Result<LlmEventStream, OpenFailure> {
        let cfg = &self.config;
        // Clamped so `initial_backoff > max_backoff` cannot make the first wait the longest.
        let mut backoff = cfg.initial_backoff.min(cfg.max_backoff);
        let mut attempt: u32 = 0;
        // The retries started so far and what the failed attempts measured below this layer.
        let mut measured = CallTelemetry::default();
        loop {
            attempt += 1;
            if ctx.cancel.is_cancelled() {
                return Err(failed(LlmError::Cancelled, measured));
            }

            let OpenFailure {
                error: err,
                measured: below,
            } = match next.run(req.clone(), ctx.clone()).await {
                Ok(stream) => return Ok(with_measured(measured, stream)),
                Err(failure) => failure,
            };
            measured = measured.merge(*below);

            let Some(cap) = attempt_cap(err.class(), cfg) else {
                tracing::debug!(attempt, error = %display_chain(&err), "retry: not retried — giving up");
                return Err(failed(err, measured));
            };
            if attempt >= cap {
                tracing::debug!(
                    attempt, cap, error = %display_chain(&err), "retry: exhausted attempts",
                );
                return Err(failed(err, measured));
            }

            // A wait set by Retry-After does not grow `backoff`: the provider set that cadence, and
            // growing ours under it would inflate the wait of a later attempt that comes without one.
            let (wait, used_retry_after) = if cfg.respect_retry_after {
                match err.unavailable_for() {
                    Some(ra) => (ra, true),
                    None => (with_jitter(backoff, cfg.jitter), false),
                }
            } else {
                (with_jitter(backoff, cfg.jitter), false)
            };

            if wait > cfg.max_wait {
                tracing::debug!(
                    attempt,
                    wait_ms = millis_u64(wait),
                    max_wait_ms = millis_u64(cfg.max_wait),
                    error = %display_chain(&err),
                    "retry: wait exceeds max_wait — bubbling error for fallback / caller",
                );
                return Err(failed(err, measured));
            }

            if !ends_before(std::time::Instant::now(), wait, ctx.deadline) {
                tracing::debug!(
                    attempt,
                    wait_ms = millis_u64(wait),
                    error = %display_chain(&err),
                    "retry: wait ends past the caller's deadline — bubbling error",
                );
                return Err(failed(err, measured));
            }

            tracing::debug!(
                attempt,
                next_attempt = attempt + 1,
                wait_ms = millis_u64(wait),
                error = %display_chain(&err),
                "retry: backing off",
            );

            // Counted once the wait is decided, so `retry_count` is the number of retries started;
            // one cancelled during its wait is counted.
            measured = measured.merge(CallTelemetry {
                retry_count: 1,
                retry_attempts: vec![RetryAttempt {
                    error_kind: err.kind(),
                    retry_after_ms: Some(millis_u64(wait)),
                }],
                ..CallTelemetry::default()
            });

            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => {
                    return Err(failed(LlmError::Cancelled, measured));
                }
                _ = tokio::time::sleep(wait) => {}
            }

            if !used_retry_after {
                backoff = next_backoff(backoff, cfg.multiplier, cfg.max_backoff);
            }
        }
    }
}

/// Returns the failure this layer hands out: the error, with every retry it started and what the
/// failed attempts measured.
fn failed(error: LlmError, measured: CallTelemetry) -> OpenFailure {
    OpenFailure::new(error, measured)
}

/// Returns `stream` led by what the failed attempts before it measured; unchanged when the first
/// attempt opened.
fn with_measured(measured: CallTelemetry, stream: LlmEventStream) -> LlmEventStream {
    if measured == CallTelemetry::default() {
        return stream;
    }
    let lead = futures::stream::once(async move {
        Ok(ChatEvent::Measured {
            telemetry: measured,
        })
    });
    Box::pin(lead.chain(stream))
}

/// Returns how many attempts in all an error allows, or `None` when it is returned at once.
fn attempt_cap(class: ErrorClass, cfg: &RetryConfig) -> Option<u32> {
    match class {
        ErrorClass::Permanent => None,
        ErrorClass::Transient => Some(cfg.max_attempts.max(1)),
    }
}

/// Returns whether a wait of `wait` from `now` ends before `deadline`; with no deadline it does.
/// A wait that ends past it would only spend the caller's time on an attempt that cannot run.
fn ends_before(
    now: std::time::Instant,
    wait: Duration,
    deadline: Option<std::time::Instant>,
) -> bool {
    match deadline {
        None => true,
        Some(d) => now.checked_add(wait).is_some_and(|end| end < d),
    }
}

/// Returns whole milliseconds, saturating at `u64::MAX` (a `Duration::MAX` `max_wait`).
fn millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Adds an offset in `[0, jitter)` whole milliseconds to `base`. The offset is the wall clock's
/// microseconds modulo the ceiling: clients that reach this at different instants get different
/// offsets, which is all de-syncing needs. A zero ceiling returns `base` unchanged. A sum past
/// `Duration::MAX` saturates there; the caller's `max_wait` check then sees a wait as long as any.
fn with_jitter(base: Duration, jitter: Duration) -> Duration {
    let ceil_ms = u64::try_from(jitter.as_millis()).unwrap_or(u64::MAX);
    if ceil_ms == 0 {
        return base;
    }
    let offset_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_micros()) % ceil_ms)
        .unwrap_or(0);
    base.saturating_add(Duration::from_millis(offset_ms))
}

/// Returns the next backoff, capped at `max`. `RetryConfig` does not validate `multiplier`, so a
/// negative or non-finite one, or an overflowing product, gives `max` rather than a panic in
/// `Duration` arithmetic.
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
    use crate::ProviderError;
    use crate::provider::error::{CredentialError, ParseProblem, ProviderRefusal};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use crate::LlmProvider;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use futures::StreamExt;
    use tars_types::LlmErrorKind;
    use tars_types::{ChatEvent, Pricing, ProviderId, ProviderProfile};

    #[test]
    fn jitter_on_the_longest_backoff_saturates_instead_of_overflowing() {
        assert_eq!(
            with_jitter(Duration::MAX, Duration::from_secs(1)),
            Duration::MAX
        );
        assert_eq!(
            with_jitter(Duration::from_secs(1), Duration::ZERO),
            Duration::from_secs(1)
        );
    }

    /// A fake `LlmProvider` whose first N calls return an error of the caller's choosing; the
    /// calls after delegate to a mock provider.
    struct FailNTimes {
        id: ProviderId,
        caps: ProviderProfile,
        remaining: AtomicU32,
        error_factory: Box<dyn Fn() -> ProviderError + Send + Sync>,
        ok_inner: Arc<dyn LlmProvider>,
        observed: Arc<AtomicU32>,
    }

    #[async_trait]
    impl LlmProvider for FailNTimes {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn capabilities(&self) -> &ProviderProfile {
            &self.caps
        }
        async fn stream(
            self: Arc<Self>,
            req: ChatRequest,
            model: &str,
            ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            self.observed.fetch_add(1, Ordering::SeqCst);
            // Atomically decrement only while a failure budget remains —
            // a `load`-then-`fetch_sub` would race under concurrent
            // callers and could decrement past zero.
            let decremented = self
                .remaining
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n > 0).then(|| n - 1)
                })
                .is_ok();
            if decremented {
                return Err((self.error_factory)());
            }
            self.ok_inner.clone().stream(req, model, ctx).await
        }
    }

    fn build(
        fails: u32,
        err: impl Fn() -> ProviderError + Send + Sync + 'static,
        retry: RetryMiddleware,
    ) -> (LlmService, Arc<AtomicU32>) {
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        let observed = Arc::new(AtomicU32::new(0));
        let failer: Arc<dyn LlmProvider> = Arc::new(FailNTimes {
            id: ProviderId::new("failer"),
            caps: ProviderProfile::text_only_baseline(Pricing::default()),
            remaining: AtomicU32::new(fails),
            error_factory: Box::new(err),
            ok_inner: mock,
            observed: observed.clone(),
        });
        let svc = LlmService::builder(failer, "test-model")
            .layer(retry)
            .build();
        (svc, observed)
    }

    #[test]
    fn a_wait_fits_only_when_it_ends_before_the_deadline() {
        let now = std::time::Instant::now();
        let sec = Duration::from_secs(1);
        assert!(ends_before(now, sec, None));
        assert!(ends_before(now, sec, Some(now + 2 * sec)));
        assert!(!ends_before(now, sec, Some(now + sec)));
        assert!(!ends_before(now, Duration::MAX, Some(now + sec)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_retry_whose_wait_ends_past_the_deadline_returns_the_error_at_once() {
        let retry = RetryMiddleware::new(RetryConfig {
            initial_backoff: Duration::from_secs(10),
            max_wait: Duration::MAX,
            ..RetryConfig::default()
        });
        let (svc, observed) = build(5, || ProviderError::ModelOverloaded, retry);
        let mut ctx = Caller::test_default();
        ctx.deadline = Some(tokio::time::Instant::now().into_std() + Duration::from_secs(1));
        let started = tokio::time::Instant::now();
        let result = svc.call(ChatRequest::user("x"), ctx).await.result;
        assert!(result.is_err());
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retries_then_succeeds_for_retriable_errors() {
        let (svc, observed) = build(
            2,
            || ProviderError::ModelOverloaded,
            RetryMiddleware::no_backoff(5),
        );
        let mut s = svc
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result
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
        assert_eq!(with_jitter(base, Duration::ZERO), base);
        // The wall clock advances between reads, so 50 samples cover a spread of offsets.
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
            || {
                ProviderError::Auth(CredentialError::Refused(ProviderRefusal {
                    status: Some(401),
                    message: "bad key".into(),
                }))
            },
            RetryMiddleware::no_backoff(5),
        );
        let err = match svc
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result
        {
            Ok(_) => panic!("expected Auth error, got success"),
            Err(e) => e,
        };
        assert!(matches!(err, LlmError::Provider(ProviderError::Auth(_))));
        // Failed on first call; no retry.
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_failure_not_shown_to_go_away_is_tried_once() {
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
            respect_retry_after: false,
            max_wait: Duration::MAX,
            jitter: Duration::ZERO,
        };
        let (svc, observed) = build(
            10,
            || ProviderError::Parse {
                at: "a test response",
                line: None,
                problem: Box::new(ParseProblem::Missing("id")),
                raw: None,
            },
            RetryMiddleware::new(cfg),
        );
        let result = svc
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result;
        assert!(result.is_err(), "expected the Parse error");
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_during_backoff_aborts_loop() {
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::from_secs(60), // long enough to cancel
            max_backoff: Duration::from_secs(60),
            multiplier: 1.0,
            respect_retry_after: false,
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
        let ctx = Caller::test_default();
        let cancel = ctx.cancel.clone();

        // Cancel after first failure but during backoff.
        let task = tokio::spawn(async move { svc.call(ChatRequest::user("x"), ctx).await.result });

        // Let the first attempt happen + enter backoff.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();

        let err = match task.await.unwrap() {
            Ok(_) => panic!("expected cancellation error"),
            Err(e) => e,
        };
        assert!(matches!(err, LlmError::Cancelled));
        // First attempt observed; cancel kicked in before any retry.
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bubbles_when_retry_after_exceeds_max_wait() {
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
            respect_retry_after: true,
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
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result
            .err()
            .expect("bubble RateLimited");
        match err {
            LlmError::Provider(ProviderError::RateLimited { retry_after }) => {
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
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
            respect_retry_after: true,
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
        let ctx = Caller::test_default();
        let req = ChatRequest::user("x");
        let task = tokio::spawn(async move { svc.call(req, ctx).await.result });

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
        let ctx = Caller::test_default();
        let req = ChatRequest::user("x");
        let task = tokio::spawn(async move { svc.call(req, ctx).await.result });

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

    #[test]
    fn a_transient_error_gets_the_attempts_and_a_permanent_one_none() {
        let cfg = RetryConfig::default();
        assert_eq!(
            attempt_cap(ErrorClass::Transient, &cfg),
            Some(cfg.max_attempts)
        );
        assert_eq!(attempt_cap(ErrorClass::Permanent, &cfg), None);
    }

    fn unavailable_for(wait: Duration) -> ProviderError {
        ProviderError::Unavailable {
            until: std::time::Instant::now() + wait,
            last_failure: crate::LastFailure {
                kind: LlmErrorKind::Network,
                message: "network: connection refused".into(),
            },
        }
    }

    #[tokio::test]
    async fn a_provider_unavailable_for_longer_than_max_wait_reaches_the_caller_at_once() {
        let (svc, observed) = build(
            5,
            || unavailable_for(Duration::from_secs(60)),
            RetryMiddleware::new(RetryConfig::default()),
        );
        let r = svc
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result;
        assert!(matches!(
            r,
            Err(LlmError::Provider(ProviderError::Unavailable { .. }))
        ));
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_provider_unavailable_briefly_is_waited_for_like_any_transient_error() {
        let (svc, observed) = build(
            1,
            || unavailable_for(Duration::from_secs(1)),
            RetryMiddleware::new(RetryConfig::default()),
        );
        let mut s = svc
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result
            .expect("the second attempt succeeds");
        while s.next().await.is_some() {}
        assert_eq!(observed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_retry_section_sets_only_the_keys_it_names() {
        let cfg: RetryConfig = toml::from_str("max_attempts = 5\ninitial_backoff_ms = 50\n")
            .expect("a partial [llm.retry] reads");
        assert_eq!(cfg.max_attempts, 5);
        assert_eq!(cfg.initial_backoff, Duration::from_millis(50));
        assert_eq!(cfg.max_wait, RetryConfig::default().max_wait);
    }

    #[test]
    fn a_misspelt_retry_key_is_refused() {
        let err = toml::from_str::<RetryConfig>("max_attemps = 5\n").unwrap_err();
        assert!(err.to_string().contains("max_attemps"), "{err}");
    }

    #[test]
    fn the_default_retry_writes_and_reads_back_unchanged() {
        let written = toml::to_string(&RetryConfig::default()).expect("the default writes");
        let read: RetryConfig = toml::from_str(&written).expect("and reads back");
        assert_eq!(format!("{read:?}"), format!("{:?}", RetryConfig::default()));
    }

    #[test]
    fn a_wait_finer_than_a_millisecond_does_not_write() {
        let cfg = RetryConfig {
            jitter: Duration::from_micros(1500),
            ..RetryConfig::default()
        };
        assert!(toml::to_string(&cfg).is_err());
        let cfg = RetryConfig {
            jitter: Duration::from_micros(2000),
            ..RetryConfig::default()
        };
        assert!(toml::to_string(&cfg).is_ok());
    }
}
