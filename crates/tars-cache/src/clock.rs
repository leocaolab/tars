//! Injectable wall-clock for TTL-expiry decisions.
//!
//! The SQLite L2 cache decides whether a row is expired by comparing
//! `expires_at_ms` against "now". Reaching for `SystemTime::now()` inline
//! makes that comparison depend on the real clock — untestable without
//! sleeping. We instead *receive* the clock at construction: production
//! wires [`SystemClock`], tests wire a fake they can advance.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Source of "now" in Unix-epoch milliseconds. `Send + Sync` so it can
/// live behind an `Arc<dyn Clock>` shared across the cache's async tasks.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// The real wall clock — the only [`Clock`] used in production.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0)
    }
}

/// Convenience: the default production clock behind an `Arc<dyn Clock>`.
pub(crate) fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}
