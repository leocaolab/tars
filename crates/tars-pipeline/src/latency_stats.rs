//! Per-provider rolling latency stats — the minimal B-8 metrics slice
//! that feeds [`crate::routing::LatencyPolicy`].
//!
//! Scope is deliberately small: a bounded rolling window of recent
//! dispatch latencies per provider, plus percentile/mean readout. This
//! is NOT the full `tars-melt` metrics/OTel stack (Prometheus exporter,
//! cardinality validator, trace sampling) — it's a self-contained
//! in-process structure, shaped like the per-provider state
//! `CircuitBreaker` already keeps, that a routing policy can consult at
//! selection time.
//!
//! Fed by [`crate::routing::RoutingService`] (constructed via
//! `with_latency_stats`), which records each successful provider
//! dispatch's latency. Read by `LatencyPolicy`, which reorders
//! candidates by the chosen metric.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use tars_types::ProviderId;

/// Which summary statistic a policy minimizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LatencyMetric {
    /// Median — typical-case latency.
    P50,
    /// 95th percentile — tail latency. Default: routing usually cares
    /// about the slow tail, not the median.
    #[default]
    P95,
    /// Arithmetic mean.
    Mean,
}

/// Summary of a provider's recent latencies (milliseconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatencyStats {
    /// Number of samples in the window.
    pub count: usize,
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
}

impl LatencyStats {
    /// The value for a given metric — what a policy sorts on.
    pub fn metric(&self, m: LatencyMetric) -> u64 {
        match m {
            LatencyMetric::P50 => self.p50_ms,
            LatencyMetric::P95 => self.p95_ms,
            LatencyMetric::Mean => self.mean_ms,
        }
    }
}

/// Bounded rolling per-provider latency window. Cheap concurrent
/// observe/read behind a single `Mutex` — routing isn't a hot enough
/// path to justify sharded/lock-free storage (matches CircuitBreaker's
/// per-provider `Mutex` choice).
pub struct LatencyStatsRegistry {
    /// Max samples kept per provider. Oldest evicted past this.
    window: usize,
    inner: Mutex<HashMap<ProviderId, VecDeque<u64>>>,
}

impl LatencyStatsRegistry {
    /// `window` = how many recent samples to keep per provider (clamped
    /// to ≥1). A larger window smooths noise but reacts slower to a
    /// provider's latency shift.
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Record one dispatch latency for `id`. Evicts the oldest sample
    /// once the window is full.
    pub fn observe(&self, id: &ProviderId, latency_ms: u64) {
        // A poisoned lock means a prior holder panicked mid-update; the
        // data is still consistent (we only push/pop), so recover rather
        // than propagate a panic into the routing path.
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let win = map.entry(id.clone()).or_default();
        win.push_back(latency_ms);
        while win.len() > self.window {
            win.pop_front();
        }
    }

    /// Snapshot `id`'s current stats, or `None` if it has no samples
    /// yet. Computed on read (the window is small).
    pub fn snapshot(&self, id: &ProviderId) -> Option<LatencyStats> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let win = map.get(id)?;
        if win.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = win.iter().copied().collect();
        sorted.sort_unstable();
        let count = sorted.len();
        let sum: u128 = sorted.iter().map(|&x| x as u128).sum();
        let mean_ms = (sum / count as u128) as u64;
        Some(LatencyStats {
            count,
            mean_ms,
            p50_ms: percentile(&sorted, 50),
            p95_ms: percentile(&sorted, 95),
        })
    }
}

/// Nearest-rank-ish percentile over an ascending-sorted slice.
/// `p` is 0..=100. Index floors `(n-1) * p / 100` — exact at the ends,
/// good enough for a routing signal in the small-window regime.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    let idx = ((n - 1) as u64 * p / 100) as usize;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ProviderId {
        ProviderId::new(s)
    }

    #[test]
    fn unknown_provider_has_no_snapshot() {
        let reg = LatencyStatsRegistry::new(10);
        assert!(reg.snapshot(&id("nope")).is_none());
    }

    #[test]
    fn single_sample_stats() {
        let reg = LatencyStatsRegistry::new(10);
        reg.observe(&id("a"), 42);
        let s = reg.snapshot(&id("a")).unwrap();
        assert_eq!(s.count, 1);
        assert_eq!(s.mean_ms, 42);
        assert_eq!(s.p50_ms, 42);
        assert_eq!(s.p95_ms, 42);
    }

    #[test]
    fn mean_and_percentiles_over_window() {
        let reg = LatencyStatsRegistry::new(100);
        for v in [10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            reg.observe(&id("a"), v);
        }
        let s = reg.snapshot(&id("a")).unwrap();
        assert_eq!(s.count, 10);
        assert_eq!(s.mean_ms, 55); // 550 / 10
        // sorted indices: p50 → idx (9*50/100)=4 → 50; p95 → idx (9*95/100)=8 → 90
        assert_eq!(s.p50_ms, 50);
        assert_eq!(s.p95_ms, 90);
    }

    #[test]
    fn window_evicts_oldest() {
        let reg = LatencyStatsRegistry::new(3);
        for v in [1000u64, 2000, 3000, 4, 5] {
            reg.observe(&id("a"), v);
        }
        // Only the last 3 (4, 5 and 3000) remain.
        let s = reg.snapshot(&id("a")).unwrap();
        assert_eq!(s.count, 3);
        assert_eq!(s.mean_ms, (3000 + 4 + 5) / 3);
    }

    #[test]
    fn metric_selector_reads_the_right_field() {
        let s = LatencyStats {
            count: 3,
            mean_ms: 10,
            p50_ms: 20,
            p95_ms: 30,
        };
        assert_eq!(s.metric(LatencyMetric::Mean), 10);
        assert_eq!(s.metric(LatencyMetric::P50), 20);
        assert_eq!(s.metric(LatencyMetric::P95), 30);
    }

    #[test]
    fn window_of_zero_clamps_to_one() {
        let reg = LatencyStatsRegistry::new(0);
        reg.observe(&id("a"), 7);
        reg.observe(&id("a"), 9);
        let s = reg.snapshot(&id("a")).unwrap();
        assert_eq!(s.count, 1); // clamped window keeps only the newest
        assert_eq!(s.mean_ms, 9);
    }
}
