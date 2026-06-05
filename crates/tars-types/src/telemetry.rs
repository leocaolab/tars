//! Per-request telemetry accumulator threaded through the middleware
//! stack via [`crate::RequestContext`].
//!
//! Each middleware writes its observation into a shared
//! `Arc<Mutex<TelemetryAccumulator>>` carried on the context. After
//! the stream completes the caller reads the accumulator out and
//! packages it into [`crate::ChatResponse`] (or the language-binding
//! equivalent like tars-py's `Response.telemetry`).
//!
//! Why a single typed accumulator rather than ad-hoc string-keyed
//! attributes:
//!
//! - **Type safety** — `cache_hit: bool` is checked at compile time;
//!   `attributes["cache_hit"] -> serde_json::Value` is checked at the
//!   reader. With many middleware writers we'd accumulate string typos
//!   and value-shape divergence over time.
//! - **Discoverability** — `cargo doc` shows the full surface in one
//!   place. New middleware authors don't have to grep for who sets
//!   what key.
//! - **Forward-compatible** — adding a field is one additive struct
//!   change, with serde-default for old readers.
//!
//! What goes in here: **operationally interesting per-call data the
//! caller might log or display**. NOT: every tracing event (those go
//! to tracing-subscriber as before).

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Snapshot of one request's path through the middleware stack.
///
/// All fields default to "nothing observed yet" so the accumulator is
/// safely usable by call paths that don't go through every middleware
/// (tests, custom builder configurations, etc.).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TelemetryAccumulator {
    /// True iff CacheLookupMiddleware found a hit in any tier (L1 in-mem
    /// or L2 disk). The full Usage already tracks `cached_input_tokens`
    /// when the *provider* did prompt-cache hit, but that's a different
    /// fact — this field is "did *we* (tars's middleware cache) avoid a
    /// provider call entirely?".
    pub cache_hit: bool,

    /// Number of retry attempts (NOT including the initial attempt).
    /// `retry_count == 0` means the first try succeeded.
    pub retry_count: u32,

    /// One entry per failed attempt that was retried. Each carries the
    /// error kind that caused the retry plus the wait the retry policy
    /// chose. Last entry is the most recent failure; if the call
    /// ultimately succeeded, all entries here are by definition prior
    /// (the success itself is implicit).
    pub retry_attempts: Vec<RetryAttempt>,

    /// Wall time spent inside the innermost provider's `.call()`
    /// (HTTP round-trip + SSE stream drain). `None` means the
    /// provider's wrapper didn't record it (e.g. mock provider).
    /// **Sum across attempts** — if the call retried 3 times, this
    /// is the total provider time across all 3 calls.
    pub provider_latency_ms: Option<u64>,

    /// Wall time end-to-end through the whole pipeline, including all
    /// middleware overhead, retries, and stream drain. Always set by
    /// the outermost (TelemetryMiddleware) when present.
    pub pipeline_total_ms: Option<u64>,

    /// Names of layers that participated in this call, outermost-first.
    /// E.g. `["telemetry", "cache_lookup", "retry", "provider"]`.
    /// Useful for debugging "why didn't middleware X fire?".
    pub layers: Vec<String>,

    /// `ProviderId` of the provider that actually ran the call,
    /// resolved post-routing. `None` until the innermost
    /// `ProviderService` writes it. Read by `EventEmitterMiddleware`
    /// when building `LlmCallFinished.provider_id`.
    #[serde(default)]
    pub provider_id: Option<String>,
}

impl TelemetryAccumulator {
    /// True iff `retry_count` agrees with `retry_attempts.len()`.
    ///
    /// `retry_count` and the `retry_attempts` log are two views of the
    /// same fact and can drift if a middleware bumps one without
    /// pushing the other. Prefer [`retries`](Self::retries) as the
    /// single source of truth when reading; call this in tests / debug
    /// asserts to catch a writer that forgot to keep them in sync.
    pub fn retries_consistent(&self) -> bool {
        self.retry_count as usize == self.retry_attempts.len()
    }

    /// Canonical retry count, derived from the attempt log so it can't
    /// disagree with `retry_attempts`. Readers should prefer this over
    /// the raw `retry_count` field.
    pub fn retries(&self) -> usize {
        self.retry_attempts.len()
    }
}

/// One retry attempt summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetryAttempt {
    /// Discriminator of the underlying [`crate::error::ProviderError`]
    /// that caused this retry. Serialised in snake_case
    /// (`"rate_limited"`, `"network"`, `"model_overloaded"`, …) via
    /// the `serde(rename_all = "snake_case")` annotation on
    /// [`crate::error::ProviderErrorKind`], so the persisted JSON
    /// wire format is unchanged from when this was `String`. The
    /// Rust-side typing now catches `"rate_limmited"` typos at
    /// compile time instead of producing a silent
    /// no-fallback-trigger-matches at runtime.
    pub error_kind: crate::error::ProviderErrorKind,
    /// Backoff this retry actually slept before the next attempt.
    /// Combines policy backoff + any provider-supplied `Retry-After`
    /// (whichever was honored by the retry middleware).
    pub retry_after_ms: Option<u64>,
}

/// Convenience: a fresh `Arc<Mutex<...>>` ready to drop onto a context.
///
/// Wrapped in a Mutex (not RwLock) because writes are the common case —
/// every middleware writes once per call, only the final reader does a
/// read. Mutex is faster for "many writers, one reader" patterns at
/// this scale.
pub type SharedTelemetry = Arc<Mutex<TelemetryAccumulator>>;

/// Construct a fresh telemetry handle. Convenient for callers that
/// build a `RequestContext` from scratch.
///
/// **Poisoning contract.** The inner `Mutex` is poisoned if a thread
/// panics while holding the lock. Telemetry is *advisory* metadata, not
/// load-bearing state — a poisoned accumulator must never abort an
/// otherwise-successful request. Callers therefore MUST NOT
/// `.lock().unwrap()`; recover the guard with [`lock_telemetry`], which
/// treats a `PoisonError` as "use the data anyway" (the worst case is a
/// partially-written observation, which is acceptable for telemetry).
pub fn new_shared_telemetry() -> SharedTelemetry {
    Arc::new(Mutex::new(TelemetryAccumulator::default()))
}

/// Lock a [`SharedTelemetry`], recovering from poisoning.
///
/// Telemetry is advisory, so a panic elsewhere that poisoned the mutex
/// should not propagate into every later telemetry write. This returns
/// the guard regardless, unwrapping the `PoisonError` to its inner
/// guard. Use this everywhere instead of `.lock().unwrap()`.
pub fn lock_telemetry(t: &SharedTelemetry) -> std::sync::MutexGuard<'_, TelemetryAccumulator> {
    t.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
