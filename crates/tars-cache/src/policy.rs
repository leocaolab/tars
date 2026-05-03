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
    /// L1 on, L2/L3 off (until their backends exist). Doc 03 §2.2's
    /// recommended default also has L2 on; we'll flip that bit when
    /// the L2 backend is real.
    fn default() -> Self {
        Self {
            l1: true,
            l2: false,
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
    fn default_enables_l1_only() {
        let p = CachePolicy::default();
        assert!(p.l1);
        assert!(!p.l2);
        assert!(!p.l3);
        assert!(p.any_enabled());
    }

    #[test]
    fn off_disables_everything() {
        assert!(!CachePolicy::off().any_enabled());
    }
}
