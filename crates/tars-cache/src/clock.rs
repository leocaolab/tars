//! The SQLite L2 cache decides whether a row is expired by comparing
//! `expires_at_ms` against "now". Reaching for `SystemTime::now()` inline
//! makes that comparison depend on the real clock — untestable without
//! sleeping. We instead *receive* the clock at construction: production
//! wires [`SystemClock`], tests wire a fake they can advance.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

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

pub(crate) fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}
