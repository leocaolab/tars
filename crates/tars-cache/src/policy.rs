//! Per-request cache policy. Threaded through `RequestContext.attributes`
//! by upstream callers (Agent layer / explicit override).
//!
//! M1 only honours `l1`. The `l2`/`l3` fields are placeholders that
//! match Doc 03 §2.2's full policy shape so the public API doesn't
//! change when the other levels land.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CachePolicy {
    pub l1: bool,
    /// **Not yet honoured** — landed when `tars-storage` ships L2.
    pub l2: bool,
    /// **Not yet honoured** — landed when `ExplicitCacheProvider` (D-1) ships.
    pub l3: bool,
    pub l1_ttl: Option<Duration>,
    pub l2_ttl: Option<Duration>,
    pub l3_ttl: Option<Duration>,
}

impl Default for CachePolicy {
    /// L1 + L2 on (matches Doc 03 §2.2). L3 is opt-in per request
    /// because explicit provider-side caches cost storage rent and
    /// only pay back for long-prefix multi-turn workloads.
    ///
    /// L2 only does anything when the registry impl is L2-aware
    /// ([`crate::SqliteCacheRegistry`]). [`crate::MemoryCacheRegistry`]
    /// silently ignores the `l2` flag — same shape, narrower
    /// implementation — so callers don't need to know which backend
    /// is wired.
    fn default() -> Self {
        Self {
            l1: true,
            l2: true,
            l3: false,
            l1_ttl: None,
            l2_ttl: None,
            l3_ttl: None,
        }
    }
}

impl CachePolicy {
    /// Disable caching entirely — useful for tests / debugging /
    /// explicit `--no-cache` flag down the road.
    pub fn off() -> Self {
        Self {
            l1: false,
            l2: false,
            l3: false,
            l1_ttl: None,
            l2_ttl: None,
            l3_ttl: None,
        }
    }

    /// True iff at least one tier is enabled. The middleware short-
    /// circuits the entire cache pipeline (no key compute, no lookup,
    /// no write) when this returns false.
    pub fn any_enabled(&self) -> bool {
        self.l1 || self.l2 || self.l3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_l1_and_l2() {
        let p = CachePolicy::default();
        assert!(p.l1);
        assert!(p.l2);
        assert!(!p.l3, "L3 stays opt-in per Doc 03 §2.2");
        assert!(p.any_enabled());
    }

    #[test]
    fn off_disables_everything() {
        assert!(!CachePolicy::off().any_enabled());
    }

    #[test]
    fn any_enabled_covers_all_eight_states() {
        for bits in 0u8..8 {
            let l1 = bits & 0b001 != 0;
            let l2 = bits & 0b010 != 0;
            let l3 = bits & 0b100 != 0;
            let p = CachePolicy {
                l1,
                l2,
                l3,
                l1_ttl: None,
                l2_ttl: None,
                l3_ttl: None,
            };
            assert_eq!(p.any_enabled(), l1 || l2 || l3, "bits={bits:03b}");
        }
    }
}
