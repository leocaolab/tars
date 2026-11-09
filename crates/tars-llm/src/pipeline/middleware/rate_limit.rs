//! Holds a call until a provider account's tokens-per-minute bucket has room for it, so a TPM
//! ceiling shows up as a bounded wait rather than as 429s. Unlike [`PerCallBudgetMiddleware`],
//! which refuses, this layer waits.
//!
//! The bucket refills on demand: each caller credits the time since the last one at `TPM / 60`
//! tokens a second, up to the burst size, then takes its reserve, letting the balance go below
//! zero, and sleeps until the refill would cover the debt. Callers are served in the order they
//! take the lock, and no task runs in the background, so the bucket works from any runtime and
//! leaves nothing behind when it is dropped. A caller that stops waiting gives its reserve back.
//!
//! Before the call it reserves bytes/4 input tokens plus the request's `max_output_tokens` (which
//! [`LlmService::call`] fills from the model's output limit), else `reserved_output_tokens`. At
//! the stream's end, in `Finished` or in an error, the reserve is settled against the usage the
//! provider reported by then: the unused part is refunded, so the limit binds on actual tokens,
//! and a call that used more than its reserve takes the overshoot from future capacity. A call
//! that fails to open, or ends before reporting any usage, keeps its whole reserve spent.
//!
//! A TPM ceiling belongs to a provider account, not to one service: N services each with their
//! own bucket would send N × TPM. Whoever builds the services holds one [`TpmBuckets`] and takes
//! each service's limiter from it, so services over one account draw from one bucket.
//!
//! [`PerCallBudgetMiddleware`]: crate::PerCallBudgetMiddleware
//! [`LlmService::call`]: crate::LlmService::call

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;

use crate::LlmEventStream;
use tars_types::{Caller, ChatRequest, Usage};

use crate::pipeline::middleware::{Middleware, OutputReserve};
use crate::pipeline::service::{Next, OpenFailure};

/// Configuration for [`TpmRateLimiter`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TpmRateLimitConfig {
    /// The sustained ceiling, in tokens per minute.
    pub tokens_per_minute: NonZeroU32,
    /// The bucket's capacity, in tokens. `None` means one second of TPM, at least one token.
    pub burst_tokens: Option<NonZeroU32>,
    /// The output tokens reserved for a request with no `max_output_tokens`.
    pub reserved_output_tokens: u32,
}

impl TpmRateLimitConfig {
    /// Returns a config with a one-second burst and the default [`OutputReserve`].
    pub fn new(tokens_per_minute: NonZeroU32) -> Self {
        Self {
            tokens_per_minute,
            burst_tokens: None,
            reserved_output_tokens: OutputReserve::default().output_tokens.get(),
        }
    }

    fn capacity(&self) -> u32 {
        self.burst_tokens.map_or_else(
            || (self.tokens_per_minute.get() / 60).max(1),
            NonZeroU32::get,
        )
    }

    /// Returns the refill rate in tokens a second; positive, since the TPM is.
    fn rate_per_sec(&self) -> f64 {
        f64::from(self.tokens_per_minute.get()) / 60.0
    }
}

/// Returns the balance after `elapsed` seconds of refill at `rate` tokens a second, never above
/// `capacity`.
fn refilled(balance: f64, elapsed: f64, rate: f64, capacity: u32) -> f64 {
    (balance + rate * elapsed).min(f64::from(capacity))
}

/// Returns how long a caller waits after taking its reserve left `balance`: none while the
/// balance is not negative, else until the refill at `rate` covers the debt.
fn wait_for(balance: f64, rate: f64) -> Duration {
    if balance >= 0.0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(-balance / rate)
    }
}

/// The balance of a bucket when it was last touched; it may be negative while callers wait.
#[derive(Debug)]
struct Balance {
    tokens: f64,
    at: Instant,
}

/// The token pool of one provider account, shared by every [`TpmRateLimiter`] over it. It does
/// not estimate a call's cost; the limiter does.
#[derive(Debug)]
struct SharedBucket {
    cfg: TpmRateLimitConfig,
    rate_per_sec: f64,
    capacity: u32,
    balance: Mutex<Balance>,
}

impl SharedBucket {
    fn new(cfg: &TpmRateLimitConfig) -> Arc<Self> {
        let capacity = cfg.capacity();
        Arc::new(Self {
            cfg: cfg.clone(),
            rate_per_sec: cfg.rate_per_sec(),
            capacity,
            balance: Mutex::new(Balance {
                tokens: f64::from(capacity),
                at: Instant::now(),
            }),
        })
    }

    /// Returns the balance refilled up to now. A poisoned lock still holds a whole balance: each
    /// writer replaces both fields with plain values and calls nothing that panics in between.
    fn refilled_now(&self) -> MutexGuard<'_, Balance> {
        let mut b = self.balance.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let elapsed = now.duration_since(b.at).as_secs_f64();
        b.tokens = refilled(b.tokens, elapsed, self.rate_per_sec, self.capacity);
        b.at = now;
        b
    }

    /// Takes `reserve` tokens and waits until the refill covers them. Dropped before the wait
    /// ends, it gives them back.
    async fn acquire(&self, reserve: u32) {
        let wait = {
            let mut b = self.refilled_now();
            b.tokens -= f64::from(reserve);
            wait_for(b.tokens, self.rate_per_sec)
        };
        if wait.is_zero() {
            return;
        }
        let refund = Refund {
            bucket: self,
            tokens: reserve,
        };
        tokio::time::sleep(wait).await;
        std::mem::forget(refund);
    }

    /// Credits `tokens`, never past capacity.
    fn give_back(&self, tokens: f64) {
        let mut b = self.refilled_now();
        b.tokens = (b.tokens + tokens).min(f64::from(self.capacity));
    }

    /// Refunds the unused part of `reserve`, or takes the overshoot from future capacity when
    /// `actual` exceeds it.
    fn settle(&self, reserve: u32, actual: u32) {
        self.give_back(f64::from(reserve) - f64::from(actual));
    }
}

/// Gives a waiting caller's reserve back when the caller stops waiting before its turn.
struct Refund<'a> {
    bucket: &'a SharedBucket,
    tokens: u32,
}

impl Drop for Refund<'_> {
    fn drop(&mut self) {
        self.bucket.give_back(f64::from(self.tokens));
    }
}

/// The TPM buckets of the provider accounts a set of services calls, one per account key. Key by
/// whatever bounds the quota: a provider id, or `provider:tenant`.
#[derive(Default)]
pub struct TpmBuckets {
    by_account: Mutex<HashMap<String, Arc<SharedBucket>>>,
}

impl TpmBuckets {
    /// Returns an empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a limiter drawing from the bucket of `account_key`; the first call for a key makes
    /// the bucket, and each limiter keeps its own output reserve.
    ///
    /// # Errors
    ///
    /// [`SharedBucketMismatchError`] when the key's bucket already runs at another rate or
    /// burst: one account has one ceiling.
    pub fn limiter(
        &self,
        account_key: impl Into<String>,
        cfg: TpmRateLimitConfig,
    ) -> Result<TpmRateLimiter, SharedBucketMismatchError> {
        let account = account_key.into();
        // A panic while the lock is held cannot leave the map half-written: the only write is one
        // `entry` insert.
        let bucket = self
            .by_account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(account.clone())
            .or_insert_with(|| SharedBucket::new(&cfg))
            .clone();
        let same_ceiling = bucket.cfg.tokens_per_minute == cfg.tokens_per_minute
            && bucket.capacity == cfg.capacity();
        if !same_ceiling {
            return Err(SharedBucketMismatchError {
                account,
                existing_tpm: bucket.cfg.tokens_per_minute.get(),
                existing_burst: bucket.capacity,
                asked_tpm: cfg.tokens_per_minute.get(),
                asked_burst: cfg.capacity(),
            });
        }
        Ok(TpmRateLimiter {
            bucket,
            reserved_output: cfg.reserved_output_tokens,
        })
    }
}

/// A shared bucket was asked for with a rate or burst other than the one it was made with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the TPM bucket of account {account:?} runs at {existing_tpm} TPM with a {existing_burst}-token \
     burst; it was asked for {asked_tpm} TPM with a {asked_burst}-token burst"
)]
pub struct SharedBucketMismatchError {
    pub account: String,
    pub existing_tpm: u32,
    pub existing_burst: u32,
    pub asked_tpm: u32,
    pub asked_burst: u32,
}

/// Waits for room in a TPM bucket before each call. Add it outermost, so the wait comes before
/// any other work:
/// `LlmService::builder_with_inner(chain).layer(buckets.limiter("acct", cfg)?).build()`.
pub struct TpmRateLimiter {
    bucket: Arc<SharedBucket>,
    reserved_output: u32,
}

impl TpmRateLimiter {
    /// Returns a limiter with a bucket of its own.
    pub fn new(cfg: TpmRateLimitConfig) -> Self {
        Self {
            bucket: SharedBucket::new(&cfg),
            reserved_output: cfg.reserved_output_tokens,
        }
    }

    /// Returns the tokens reserved before the call, clamped to capacity: a larger reserve could
    /// never be covered, since the bucket never fills past capacity.
    fn reserve_tokens(&self, req: &ChatRequest) -> u32 {
        let input = estimate_input_tokens(req);
        let output = req.max_output_tokens.unwrap_or(self.reserved_output);
        input.saturating_add(output).min(self.bucket.capacity)
    }
}

#[async_trait]
impl Middleware for TpmRateLimiter {
    fn name(&self) -> &'static str {
        "tpm_rate_limit"
    }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: Caller,
        next: Next<'_>,
    ) -> Result<LlmEventStream, OpenFailure> {
        let reserve = self.reserve_tokens(&req);
        self.bucket.acquire(reserve).await;

        let stream = next.run(req, ctx).await?;

        // Settled inline in the consumer's drain, so the common path spawns no task.
        let bucket = self.bucket.clone();
        let mut spent = None;
        let mut settled = false;
        let wrapped = stream.inspect(move |ev| {
            if settled {
                return;
            }
            let ended = match ev {
                Ok(event) => {
                    spent = event.spent_after(spent);
                    event.is_terminal()
                }
                Err(_) => true,
            };
            if !ended {
                return;
            }
            settled = true;
            if let Some(usage) = spent {
                bucket.settle(reserve, tokens_counted(&usage));
            }
        });
        Ok(Box::pin(wrapped))
    }
}

/// Returns the tokens a call takes from the bucket, saturating at `u32::MAX`.
fn tokens_counted(usage: &Usage) -> u32 {
    // [arc:intentional-handle] reason: a count past u32::MAX is not a real call's tokens; it is
    // clamped so the overshoot is still taken from future capacity.
    u32::try_from(usage.input_tokens.saturating_add(usage.output_tokens)).unwrap_or(u32::MAX)
}

/// Returns the estimated tokens of `system` and every text block, rounded down.
fn estimate_input_tokens(req: &ChatRequest) -> u32 {
    u32::try_from(req.text_bytes() / tars_types::BYTES_PER_TOKEN).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::ChatEvent;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    #[test]
    fn capacity_defaults_to_one_second_of_tpm() {
        let cfg = TpmRateLimitConfig::new(nz(6_000_000)); // 100k tok/s
        assert_eq!(cfg.capacity(), 100_000);
        let cfg = TpmRateLimitConfig {
            tokens_per_minute: nz(6_000_000),
            burst_tokens: Some(nz(500)),
            reserved_output_tokens: 4_096,
        };
        assert_eq!(cfg.capacity(), 500);
    }

    #[test]
    fn reserve_is_clamped_to_capacity() {
        let rl = TpmRateLimiter::new(TpmRateLimitConfig {
            tokens_per_minute: nz(60_000),
            burst_tokens: Some(nz(1_000)),
            reserved_output_tokens: 100_000, // absurd reserve
        });
        let req = ChatRequest::user("hello world");
        assert_eq!(rl.reserve_tokens(&req), 1_000);
    }

    #[test]
    fn input_estimate_counts_system_and_messages() {
        // 40 chars system + 40 chars user = 80 chars → 20 tokens.
        let req = ChatRequest::user("a".repeat(40)).with_system("b".repeat(40));
        assert_eq!(estimate_input_tokens(&req), 20);
    }

    #[test]
    fn shared_key_reuses_one_bucket_new_makes_its_own() {
        let cfg = || TpmRateLimitConfig::new(nz(6_000_000));
        let buckets = TpmBuckets::new();
        let a = buckets.limiter("acct-test-A", cfg()).unwrap();
        let b = buckets.limiter("acct-test-A", cfg()).unwrap();
        let c = buckets.limiter("acct-test-B", cfg()).unwrap();
        let d = TpmRateLimiter::new(cfg());
        let e = TpmRateLimiter::new(cfg());
        assert!(
            Arc::ptr_eq(&a.bucket, &b.bucket),
            "same key must share one bucket"
        );
        assert!(
            !Arc::ptr_eq(&a.bucket, &c.bucket),
            "different key → different bucket"
        );
        assert!(
            !Arc::ptr_eq(&d.bucket, &e.bucket),
            "new() always builds a private bucket"
        );
    }

    #[tokio::test]
    async fn services_over_one_account_key_spend_one_quota() {
        use crate::LlmService;
        use crate::provider::backends::mock::{CannedResponse, MockProvider};
        use futures::StreamExt;
        use tars_types::ProviderId;

        // One token a minute, so nothing refills while the test runs.
        let cfg = || TpmRateLimitConfig {
            tokens_per_minute: nz(1),
            burst_tokens: Some(nz(1_000)),
            reserved_output_tokens: 4_096,
        };
        let buckets = TpmBuckets::new();
        let first = buckets.limiter("acct", cfg()).unwrap();
        let second = buckets.limiter("acct", cfg()).unwrap();
        let other = buckets.limiter("other-acct", cfg()).unwrap();
        let second_bucket = second.bucket.clone();
        let other_bucket = other.bucket.clone();
        let service = |limiter: TpmRateLimiter| {
            // 400 bytes of reply are reported as 100 output tokens.
            let mock =
                MockProvider::new(ProviderId::new("p"), CannedResponse::text("x".repeat(400)));
            Arc::new(LlmService::builder(mock, "m").layer(limiter).build())
        };
        let first = service(first);
        let _second = service(second);
        let _other = service(other);

        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(500);
        let mut stream = first
            .call(req, Caller::test_default())
            .await
            .result
            .expect("stream should open");
        while stream.next().await.is_some() {}

        assert_eq!(
            second_bucket.balance.lock().unwrap().tokens as usize,
            1_000 - 100
        );
        assert_eq!(other_bucket.balance.lock().unwrap().tokens as usize, 1_000);
    }

    /// Returns the bucket's free tokens after one call, read to its end, whose provider streams
    /// `events` and then, having no `Finished`, ends in `Unfinished`.
    async fn free_after_unfinished_call(events: Vec<ChatEvent>) -> usize {
        use crate::LlmService;
        use crate::provider::backends::mock::{CannedResponse, MockProvider};
        use futures::StreamExt;
        use tars_types::ProviderId;

        // One token a minute, so the refiller adds nothing while the test runs.
        let limiter = TpmRateLimiter::new(TpmRateLimitConfig {
            tokens_per_minute: nz(1),
            burst_tokens: Some(nz(1_000)),
            reserved_output_tokens: 4_096,
        });
        let bucket = limiter.bucket.clone();
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::Sequence(events));
        let svc = LlmService::builder(mock, "m").layer(limiter).build();
        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(500);

        let mut stream = Arc::new(svc)
            .call(req, Caller::test_default())
            .await
            .result
            .expect("stream should open");
        while let Some(ev) = stream.next().await {
            if ev.is_err() {
                break;
            }
        }
        bucket.balance.lock().unwrap().tokens as usize
    }

    #[tokio::test]
    async fn a_call_ending_in_an_error_is_settled_against_the_usage_it_reported() {
        let free = free_after_unfinished_call(vec![
            ChatEvent::started("m"),
            ChatEvent::UsageProgress {
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Usage::default()
                },
            },
        ])
        .await;
        assert_eq!(free, 1_000 - 150);
    }

    #[tokio::test]
    async fn a_call_ending_in_an_error_before_any_usage_keeps_its_reserve_spent() {
        let free = free_after_unfinished_call(vec![ChatEvent::started("m")]).await;
        assert_eq!(free, 1_000 - 500);
    }

    #[test]
    fn the_refill_stops_at_capacity() {
        assert_eq!(refilled(990.0, 1.0, 5.0, 1_000), 995.0);
        assert_eq!(refilled(990.0, 1.0, 50.0, 1_000), 1_000.0);
    }

    #[test]
    fn a_caller_waits_only_while_the_balance_is_negative() {
        assert_eq!(wait_for(0.0, 100.0), Duration::ZERO);
        assert_eq!(wait_for(-50.0, 100.0), Duration::from_millis(500));
    }

    fn one_call(svc: LlmService) -> impl std::future::Future<Output = ()> {
        use futures::StreamExt;
        async move {
            let mut s = svc
                .call(ChatRequest::user("x"), Caller::test_default())
                .await
                .result
                .unwrap();
            while s.next().await.is_some() {}
        }
    }

    fn mock_service(limiter: TpmRateLimiter) -> LlmService {
        use crate::provider::backends::mock::{CannedResponse, MockProvider};
        use tars_types::ProviderId;
        LlmService::builder(
            MockProvider::new(ProviderId::new("p"), CannedResponse::Sequence(vec![])),
            "m",
        )
        .layer(limiter)
        .build()
    }

    use crate::LlmService;

    #[test]
    fn a_shared_bucket_refills_after_the_runtime_of_its_first_caller_ends() {
        // 100 tokens a second; each call reserves the whole 50-token burst.
        let cfg = || TpmRateLimitConfig {
            tokens_per_minute: nz(6_000),
            burst_tokens: Some(nz(50)),
            reserved_output_tokens: 50,
        };
        let buckets = TpmBuckets::new();
        let svc = || mock_service(buckets.limiter("refill-across-runtimes", cfg()).unwrap());

        let first = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        first.block_on(one_call(svc()));
        drop(first);

        let second = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let waited = second.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), one_call(svc())).await
        });
        assert!(waited.is_ok(), "50 tokens refill in 0.5 s");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_limiter_leaves_no_task_running() {
        let before = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        for _ in 0..20 {
            one_call(mock_service(TpmRateLimiter::new(TpmRateLimitConfig::new(
                nz(600_000),
            ))))
            .await;
        }
        let after = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        assert_eq!(after, before);
    }

    #[test]
    fn a_shared_bucket_asked_for_another_ceiling_is_refused() {
        let cfg = |tpm| TpmRateLimitConfig::new(nz(tpm));
        let buckets = TpmBuckets::new();
        buckets.limiter("mismatch-acct", cfg(60_000)).unwrap();
        assert!(buckets.limiter("mismatch-acct", cfg(60_000)).is_ok());
        let err = buckets
            .limiter("mismatch-acct", cfg(120_000))
            .err()
            .unwrap();
        assert_eq!(err.existing_tpm, 60_000);
        assert_eq!(err.asked_tpm, 120_000);
    }

    #[tokio::test]
    async fn a_caller_that_stops_waiting_gives_its_reserve_back() {
        // One token a minute, so nothing refills while the test runs.
        let bucket = SharedBucket::new(&TpmRateLimitConfig {
            tokens_per_minute: nz(1),
            burst_tokens: Some(nz(100)),
            reserved_output_tokens: 0,
        });
        bucket.acquire(100).await;
        let gave_up = tokio::time::timeout(Duration::from_millis(20), bucket.acquire(40)).await;
        assert!(gave_up.is_err());
        let left = bucket.balance.lock().unwrap().tokens;
        assert!(
            (0.0..1.0).contains(&left),
            "the 40 tokens came back: {left}"
        );
    }
}
