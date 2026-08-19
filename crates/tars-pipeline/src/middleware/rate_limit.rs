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

use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Semaphore;

use tars_provider::LlmEventStream;
use tars_types::{Capabilities, ChatEvent, ChatRequest, ProviderError, RequestContext};

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

/// A proactive TPM limiter as a middleware layer. Add it OUTERMOST (before
/// telemetry / retry / cache) so the backpressure wait happens before any work:
/// `LlmService::builder_with_inner(chain).layer(TpmRateLimiter::new(cfg)).build()`.
pub struct TpmRateLimiter {
    sem: Arc<Semaphore>,
    /// Refill rate, tokens per second (`TPM / 60`).
    rate_per_sec: f64,
    capacity: u32,
    reserved_output: u32,
    /// Spawns the single refiller exactly once, on the first call.
    refiller: Once,
}

impl TpmRateLimiter {
    pub fn new(cfg: TpmRateLimitConfig) -> Self {
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
        Self {
            sem: Arc::new(Semaphore::new(capacity as usize)),
            rate_per_sec,
            capacity,
            reserved_output: cfg.reserved_output_tokens,
            refiller: Once::new(),
        }
    }

    /// Build from a provider's capability snapshot — the output reserve comes from
    /// the provider's `max_output_tokens` (worst case), like the budget middleware.
    pub fn from_capabilities(tokens_per_minute: u32, caps: &Capabilities) -> Self {
        Self::new(TpmRateLimitConfig {
            tokens_per_minute,
            burst_tokens: None,
            reserved_output_tokens: caps.max_output_tokens.unwrap_or(4_096),
        })
    }

    /// Spawn the refiller once. Runs in the caller's runtime (first `handle` is
    /// always inside one), so construction stays runtime-free.
    fn ensure_refiller(&self) {
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

    /// The reserve (upper bound) charged before the call: input estimate + the
    /// output reserve, clamped to the bucket so one call can't exceed total
    /// capacity (which would park it forever — the refiller caps at `capacity`).
    fn reserve_tokens(&self, req: &ChatRequest) -> u32 {
        let input = estimate_input_tokens(req);
        let output = req.max_output_tokens.unwrap_or(self.reserved_output);
        input.saturating_add(output).min(self.capacity)
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
        self.ensure_refiller();

        let reserve = self.reserve_tokens(&req);
        // Backpressure: park until the bucket holds the reserve, then spend it
        // (`forget` — the refiller, not Drop, returns tokens over time).
        self.sem
            .acquire_many(reserve)
            .await
            .expect("rate-limit semaphore never closed")
            .forget();

        let stream = next.run(req, ctx).await?;

        // Reconcile on the terminal Finished event: refund the unused reserve so
        // the limit binds on ACTUAL tokens. Done once, inline in the drain the
        // caller already performs — no extra task on the happy path.
        let sem_limiter = ReconcileHandle {
            sem: self.sem.clone(),
            capacity: self.capacity,
            reserve,
        };
        let mut reconciled = false;
        let wrapped = stream.inspect(move |ev| {
            if reconciled {
                return;
            }
            if let Ok(ChatEvent::Finished { usage, .. }) = ev {
                let actual = u32::try_from(
                    usage.input_tokens.saturating_add(usage.output_tokens),
                )
                .unwrap_or(u32::MAX);
                sem_limiter.settle(actual);
                reconciled = true;
            }
        });
        Ok(Box::pin(wrapped))
    }
}

/// The subset of limiter state the stream-reconcile closure needs. Kept separate
/// so the closure is `'static` (it outlives `&self`, riding the returned stream).
struct ReconcileHandle {
    sem: Arc<Semaphore>,
    capacity: u32,
    reserve: u32,
}

impl ReconcileHandle {
    fn settle(&self, actual: u32) {
        if self.reserve >= actual {
            let unused = self.reserve - actual;
            let room = (self.capacity as usize).saturating_sub(self.sem.available_permits());
            let give = (unused as usize).min(room);
            if give > 0 {
                self.sem.add_permits(give);
            }
        } else {
            let over = actual - self.reserve;
            let sem = self.sem.clone();
            tokio::spawn(async move {
                if let Ok(p) = sem.acquire_many_owned(over).await {
                    p.forget();
                }
            });
        }
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
}
