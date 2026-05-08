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
//! Backoff is exponential with jitter-free multiplier; if the error
//! carries a `retry_after` (Anthropic / OpenAI Retry-After header)
//! we honour that instead of computing our own — the provider knows
//! its own load shape better than we do.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use tars_provider::LlmEventStream;
use tars_types::{ChatRequest, ErrorClass, ProviderError, RequestContext};

use crate::middleware::Middleware;
use crate::service::LlmService;

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
            },
        }
    }
}

impl Middleware for RetryMiddleware {
    fn name(&self) -> &'static str {
        "retry"
    }
    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(RetryService {
            inner,
            config: self.config.clone(),
        })
    }
}

struct RetryService {
    inner: Arc<dyn LlmService>,
    config: RetryConfig,
}

#[async_trait]
impl LlmService for RetryService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let cfg = &self.config;
        let mut backoff = cfg.initial_backoff;
        let mut attempt: u32 = 0;
        // Record this layer in the telemetry trace.
        if let Ok(mut t) = ctx.telemetry.lock() {
            t.layers.push("retry".into());
        }
        loop {
            attempt += 1;
            // Honour cancellation between attempts (the very first try
            // also checks — caller may have cancelled before we ran).
            if ctx.cancel.is_cancelled() {
                return Err(ProviderError::Internal("cancelled before retry".into()));
            }

            let result = self.inner.clone().call(req.clone(), ctx.clone()).await;
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
                ErrorClass::Retriable => cfg.max_attempts,
                ErrorClass::MaybeRetriable => cfg.max_attempts_maybe_retriable,
            };
            if attempt >= cap {
                tracing::debug!(
                    attempt, cap, error = %err, "retry: exhausted attempts",
                );
                return Err(err);
            }

            let wait = if cfg.respect_retry_after {
                err.retry_after().unwrap_or(backoff)
            } else {
                backoff
            };
            tracing::debug!(
                attempt,
                next_attempt = attempt + 1,
                wait_ms = wait.as_millis() as u64,
                error = %err,
                "retry: backing off",
            );

            // Telemetry: record this failed attempt before we sleep.
            // `retry_count` increments AFTER the wait commits — so it
            // tracks "attempts retried", which is the natural caller
            // intuition (if the next attempt also fails terminally,
            // retry_count still reflects the count of *retries that
            // happened* before the final failure).
            if let Ok(mut t) = ctx.telemetry.lock() {
                t.retry_count = t.retry_count.saturating_add(1);
                t.retry_attempts.push(tars_types::RetryAttempt {
                    error_kind: provider_error_kind(&err).into(),
                    retry_after_ms: Some(wait.as_millis() as u64),
                });
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

            // Exponential backoff for the next attempt — never above max.
            backoff = (backoff.mul_f64(cfg.multiplier)).min(cfg.max_backoff);
        }
    }
}

/// Snake-case kind tag matching `tars-py`'s `TarsProviderError.kind`
/// Wrapper around [`ProviderError::kind`] kept for call-site clarity
/// and so a future `provider_error_kind` divergence (if it ever needs
/// to differ from the canonical kind string) doesn't have to update
/// every callsite.
fn provider_error_kind(err: &ProviderError) -> &'static str {
    err.kind()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        ok_inner: Arc<dyn LlmService>,
        observed: Arc<AtomicU32>,
    }

    #[async_trait]
    impl LlmService for FailNTimes {
        async fn call(
            self: Arc<Self>,
            req: ChatRequest,
            ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            self.observed.fetch_add(1, Ordering::SeqCst);
            if self.remaining.load(Ordering::SeqCst) > 0 {
                self.remaining.fetch_sub(1, Ordering::SeqCst);
                return Err((self.error_factory)());
            }
            self.ok_inner.clone().call(req, ctx).await
        }
    }

    fn build(
        fails: u32,
        err: impl Fn() -> ProviderError + Send + Sync + 'static,
        retry: RetryMiddleware,
    ) -> (Arc<dyn LlmService>, Arc<AtomicU32>) {
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let inner: Arc<dyn LlmService> = crate::ProviderService::new(mock);
        let observed = Arc::new(AtomicU32::new(0));
        let failer: Arc<dyn LlmService> = Arc::new(FailNTimes {
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
                ChatRequest::user(ModelHint::Explicit("m".into()), "x"),
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn never_retries_permanent_errors() {
        let (svc, observed) = build(
            5,
            || ProviderError::Auth("bad key".into()),
            RetryMiddleware::no_backoff(5),
        );
        let err = match svc
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "x"),
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
        };
        let (svc, observed) = build(
            10,
            || ProviderError::Parse("bad".into()),
            RetryMiddleware::new(cfg),
        );
        let result = svc
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "x"),
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
            svc.call(ChatRequest::user(ModelHint::Explicit("m".into()), "x"), ctx)
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
        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "x");
        let task = tokio::spawn(svc.call(req, ctx));

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
