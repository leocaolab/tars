//! TPM (tokens-per-minute) rate-limit middleware — proactive provider-rate
//! backpressure. The sibling of [`super::budget`]: budget REJECTS a call whose
//! cost exceeds a cap; this one PARKS a call until the token budget refills, so a
//! provider's TPM ceiling shows up as backpressure (a bounded wait), never as
//! dropped work or a wall of 429s.
//!
//! ## The bucket (why a Semaphore, not a Mutex)
//!
//! Tokens ARE semaphore permits. `acquire_many(cost).await` parks the caller in
//! tokio's fair FIFO queue — no shared `Mutex<Bucket>` that every one of 10k
//! callers spins on (that turns the limiter itself into the bottleneck, the same
//! trap as a single-connection store). A single background **refiller** task adds
//! `TPM/60` permits per second, capped at the burst size; nothing else writes the
//! bucket. The refiller is spawned once, lazily, on the first call (so
//! construction needs no runtime).
//!
//! ## Cost = estimate → reconcile
//!
//! Pre-call the true token count is unknown, so we RESERVE an upper bound —
//! `chars/4` over `system` + message text (the house heuristic, budget.rs §15
//! "no tokenizers on the hot path"), plus the reserved output (`req.max_output`,
//! else the provider cap). After the stream finishes we know the REAL
//! [`Usage`]; the unused reserve is refunded to the bucket, so the limit binds on
//! *actual* tokens, not the worst-case reserve. A call that outruns its estimate
//! (rare) debits the difference from future capacity.
//!
//! ## One bucket per provider ACCOUNT (not per LlmService)
//!
//! A TPM ceiling belongs to a provider account, not to a service instance. If
//! every [`LlmService`] built its own bucket, N services over the same account
//! would each get the full TPM → N×TPM → the real quota blows. So
//! [`TpmRateLimiter::shared`] keys the bucket by an account string in a process-
//! global registry: all limiters for `"anthropic:acct-A"` share ONE bucket, and
//! their combined rate is the account TPM. [`TpmRateLimiter::new`] keeps a private
//! bucket for the standalone / single-service / test case.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Semaphore;

use tars_provider::LlmEventStream;
use tars_types::{ChatEvent, ChatRequest, ProviderError, ProviderProfile, RequestContext};

use crate::middleware::Middleware;
use crate::service::Next;

/// How often the refiller tops the bucket up. A bucket sustains the full TPM only
/// if `capacity >= rate × REFILL_TICK` (one tick's refill must fit); the 1-second
/// default burst clears that by 100×. Finer tick = smaller minimum burst, more
/// wakeups. 10ms → a bucket as small as `rate/100` still sustains.
const REFILL_TICK: Duration = Duration::from_millis(10);

/// Configuration for [`TpmRateLimiter`].
#[derive(Clone, Debug)]
pub struct TpmRateLimitConfig {
    /// The sustained ceiling, in tokens per minute.
    pub tokens_per_minute: u32,
    /// Burst bucket capacity, in tokens. `None` → one second of TPM
    /// (`tokens_per_minute / 60`), the natural "allow a one-second spike" default.
    pub burst_tokens: Option<u32>,
    /// Reserved output tokens when neither the request nor the provider pins a
    /// bound — the worst-case output hold. Refunded down to actual after the call.
    pub reserved_output_tokens: u32,
}

impl TpmRateLimitConfig {
    /// Minimal config: a TPM ceiling, a 1-second burst, and a 4k output reserve.
    pub fn new(tokens_per_minute: u32) -> Self {
        Self {
            tokens_per_minute,
            burst_tokens: None,
            reserved_output_tokens: 4_096,
        }
    }

    /// The bucket capacity this config resolves to (explicit burst, else 1s of TPM,
    /// never below one so a zero-TPM misconfig still makes progress on refund).
    fn capacity(&self) -> u32 {
        self.burst_tokens
            .unwrap_or_else(|| (self.tokens_per_minute / 60).max(1))
    }
}

/// The shared token pool — ONE per provider account. Multiple [`TpmRateLimiter`]
/// instances over the same account share an `Arc<SharedBucket>` so the combined
/// rate is the account TPM, not N×TPM. Holds the semaphore (the tokens) + the
/// single refiller; the per-call cost estimate lives on the limiter, not here.
struct SharedBucket {
    sem: Arc<Semaphore>,
    /// Refill rate, tokens per second (`TPM / 60`).
    rate_per_sec: f64,
    capacity: u32,
    /// Spawns the single refiller exactly once, on the first call.
    refiller: Once,
}

impl SharedBucket {
    fn new(cfg: &TpmRateLimitConfig) -> Arc<Self> {
        let capacity = cfg.capacity();
        let rate_per_sec = cfg.tokens_per_minute as f64 / 60.0;
        // A bucket smaller than one tick's refill can't sustain the configured
        // TPM — the refiller would clamp at `capacity` and the effective ceiling
        // silently drops to `capacity / tick`. Surface it rather than throttle
        // quietly (the 1-second default burst never trips this).
        let min_burst = rate_per_sec * REFILL_TICK.as_secs_f64();
        if (capacity as f64) < min_burst {
            tracing::warn!(
                tpm = cfg.tokens_per_minute,
                burst_tokens = capacity,
                min_burst = min_burst as u32,
                "TpmRateLimiter: burst is below one refill tick; effective rate will be \
                 capped at burst/tick ({} tok/s), BELOW the configured TPM",
                (capacity as f64 / REFILL_TICK.as_secs_f64()) as u64,
            );
        }
        Arc::new(Self {
            sem: Arc::new(Semaphore::new(capacity as usize)),
            rate_per_sec,
            capacity,
            refiller: Once::new(),
        })
    }

    /// Spawn the refiller once. Runs in the caller's runtime (first `handle` is
    /// always inside one), so construction stays runtime-free.
    fn ensure_refiller(&self) {
        if self.refiller.is_completed() {
            return;
        }
        self.refiller.call_once(|| {
            let sem = self.sem.clone();
            let rate = self.rate_per_sec;
            let cap = self.capacity as usize;
            tokio::spawn(async move {
                // Credit REAL elapsed time, not the nominal tick — so sleep
                // overshoot / scheduler stalls don't bias the delivered rate
                // below TPM (a stalled tick delivers its backlog on the next).
                let mut last = Instant::now();
                let mut carry = 0.0f64; // sub-token remainder, kept across ticks
                loop {
                    tokio::time::sleep(REFILL_TICK).await;
                    let now = Instant::now();
                    let dt = now.duration_since(last).as_secs_f64();
                    last = now;
                    carry += rate * dt;
                    let want = carry as usize; // floor; fraction stays in carry
                    carry -= want as f64;
                    let room = cap.saturating_sub(sem.available_permits());
                    let add = want.min(room);
                    if add > 0 {
                        sem.add_permits(add);
                    }
                }
            });
        });
    }

    /// Backpressure: park until `reserve` tokens are available, then spend them
    /// (`forget` — the refiller, not Drop, returns tokens over time).
    async fn acquire(&self, reserve: u32) {
        self.ensure_refiller();
        self.sem
            .acquire_many(reserve)
            .await
            .expect("rate-limit semaphore never closed")
            .forget();
    }

    /// Reconcile the reserve against the ACTUAL usage. Refund the unused reserve
    /// (never past the cap → keeps `available ∈ [0, capacity]`); if the call
    /// outran its estimate (rare), debit the overshoot from future capacity.
    fn settle(&self, reserve: u32, actual: u32) {
        if reserve >= actual {
            let unused = reserve - actual;
            let room = (self.capacity as usize).saturating_sub(self.sem.available_permits());
            let give = (unused as usize).min(room);
            if give > 0 {
                self.sem.add_permits(give);
            }
        } else {
            let over = actual - reserve;
            let sem = self.sem.clone();
            tokio::spawn(async move {
                if let Ok(p) = sem.acquire_many_owned(over).await {
                    p.forget();
                }
            });
        }
    }
}

/// The process-global bucket registry, keyed by provider-account string.
fn registry() -> &'static Mutex<HashMap<String, Arc<SharedBucket>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<SharedBucket>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A proactive TPM limiter as a middleware layer. Add it OUTERMOST (before
/// telemetry / retry / cache) so the backpressure wait happens before any work:
/// `LlmService::builder_with_inner(chain).layer(TpmRateLimiter::shared("acct", cfg)).build()`.
pub struct TpmRateLimiter {
    bucket: Arc<SharedBucket>,
    reserved_output: u32,
}

impl TpmRateLimiter {
    /// A limiter with its OWN private bucket. Use for a single service, or tests.
    /// For a real provider account shared by several services, use [`Self::shared`]
    /// so they don't each get the full TPM.
    pub fn new(cfg: TpmRateLimitConfig) -> Self {
        Self {
            bucket: SharedBucket::new(&cfg),
            reserved_output: cfg.reserved_output_tokens,
        }
    }

    /// A limiter that shares ONE bucket per `account_key` across the process. The
    /// FIRST call for a key defines the bucket (`cfg`); later calls with the same
    /// key reuse it and ignore their `cfg`'s rate/burst (they still carry their own
    /// output reserve). Key by whatever bounds the quota — provider id, or
    /// `provider:tenant` when accounts are per-tenant.
    pub fn shared(account_key: impl Into<String>, cfg: TpmRateLimitConfig) -> Self {
        let reserved_output = cfg.reserved_output_tokens;
        let bucket = registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(account_key.into())
            .or_insert_with(|| SharedBucket::new(&cfg))
            .clone();
        Self {
            bucket,
            reserved_output,
        }
    }

    /// Build (private bucket) from a provider's capability snapshot — the output
    /// reserve comes from the provider's `max_output_tokens` (worst case), like the
    /// budget middleware. For a shared account bucket, use [`Self::shared`].
    pub fn from_capabilities(tokens_per_minute: u32, caps: &ProviderProfile) -> Self {
        Self::new(TpmRateLimitConfig {
            tokens_per_minute,
            burst_tokens: None,
            reserved_output_tokens: caps.max_output_tokens.unwrap_or(4_096),
        })
    }

    /// The reserve (upper bound) charged before the call: input estimate + the
    /// output reserve, clamped to the bucket so one call can't exceed total
    /// capacity (which would park it forever — the refiller caps at `capacity`).
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
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        let reserve = self.reserve_tokens(&req);
        self.bucket.acquire(reserve).await;

        let stream = next.run(req, ctx).await?;

        // Reconcile on the terminal Finished event: refund the unused reserve so
        // the limit binds on ACTUAL tokens. Done once, inline in the drain the
        // caller already performs — no extra task on the happy path. The closure
        // captures the shared bucket (`'static`), so it outlives `&self`.
        let bucket = self.bucket.clone();
        let mut reconciled = false;
        let wrapped = stream.inspect(move |ev| {
            if reconciled {
                return;
            }
            if let Ok(ChatEvent::Finished { usage, .. }) = ev {
                let actual = u32::try_from(usage.input_tokens.saturating_add(usage.output_tokens))
                    .unwrap_or(u32::MAX);
                bucket.settle(reserve, actual);
                reconciled = true;
            }
        });
        Ok(Box::pin(wrapped))
    }
}

/// Input-token estimate: `chars / 4` over `system` + every text content block, the
/// same heuristic the budget middleware documents (no tokenizer on the hot path).
fn estimate_input_tokens(req: &ChatRequest) -> u32 {
    let mut chars = req.system.as_deref().map_or(0, str::len);
    for m in &req.messages {
        for b in m.content() {
            if let Some(t) = b.as_text() {
                chars += t.len();
            }
        }
    }
    u32::try_from(chars / 4).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_defaults_to_one_second_of_tpm() {
        let cfg = TpmRateLimitConfig::new(6_000_000); // 100k tok/s
        assert_eq!(cfg.capacity(), 100_000);
        let cfg = TpmRateLimitConfig {
            tokens_per_minute: 6_000_000,
            burst_tokens: Some(500),
            reserved_output_tokens: 4_096,
        };
        assert_eq!(cfg.capacity(), 500);
    }

    #[test]
    fn reserve_is_clamped_to_capacity() {
        let rl = TpmRateLimiter::new(TpmRateLimitConfig {
            tokens_per_minute: 60_000,
            burst_tokens: Some(1_000),
            reserved_output_tokens: 100_000, // absurd reserve
        });
        // A giant reserve can never exceed the bucket, so acquire can't deadlock.
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
        let cfg = || TpmRateLimitConfig::new(6_000_000);
        // Same account key → same Arc<SharedBucket> (combined rate = account TPM,
        // NOT N×TPM). Different key → different bucket. `new` → always private.
        let a = TpmRateLimiter::shared("acct-test-A", cfg());
        let b = TpmRateLimiter::shared("acct-test-A", cfg());
        let c = TpmRateLimiter::shared("acct-test-B", cfg());
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
}
