//! Per-request cache policy. Threaded through `RequestContext.attributes`
//! by upstream callers (Agent layer / explicit override).
//!
//! M1 only honours `l1`. The `l2`/`l3` fields are placeholders that
//! match Doc 03 §2.2's full policy shape so the public API doesn't
//! change when the other levels land.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Per-level on/off + optional TTL override.
///
/// Note on the shape: a cleaner sum-as-product encoding would be
/// `Option<LevelPolicy>` per tier so a disabled level can't carry a
/// meaningless TTL. We keep the flat `(bool, Option<Duration>)` shape
/// because `CachePolicy` is serialized into `RequestContext.attributes`
/// (JSON) and is read by multiple consumers across the workspace;
/// changing the wire format would force every reader to migrate at
/// the same time. The `arc scan --judge` finding for this struct
/// (l1_ttl meaningful only when l1=true) is mitigated by the
/// `*_ttl_effective` accessors below, which return `None` when the
/// corresponding level is off — readers that go through them can't
/// observe the contradiction.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CachePolicy {
    pub l1: bool,
    /// **Not yet honoured** — landed when `tars-storage` ships L2.
    pub l2: bool,
    /// **Not yet honoured** — landed when `ExplicitCacheProvider` (D-1) ships.
    pub l3: bool,
    /// TTL override for L1. **Ignored** when `l1 == false`; prefer
    /// [`Self::l1_ttl_effective`] to read this value, which surfaces
    /// the meaningless-when-disabled state as `None`.
    pub l1_ttl: Option<Duration>,
    /// TTL override for L2. **Ignored** when `l2 == false`; prefer
    /// [`Self::l2_ttl_effective`].
    pub l2_ttl: Option<Duration>,
    /// TTL override for L3. **Ignored** when `l3 == false`; prefer
    /// [`Self::l3_ttl_effective`].
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

    /// L1 TTL override, observing the "ignored when L1 is off" rule —
    /// returns `None` if `l1=false`, regardless of `l1_ttl`. Use this
    /// in preference to reading [`Self::l1_ttl`] directly so the
    /// disabled-level case can't leak through.
    pub fn l1_ttl_effective(&self) -> Option<Duration> {
        if self.l1 { self.l1_ttl } else { None }
    }

    /// See [`Self::l1_ttl_effective`].
    pub fn l2_ttl_effective(&self) -> Option<Duration> {
        if self.l2 { self.l2_ttl } else { None }
    }

    /// See [`Self::l1_ttl_effective`].
    pub fn l3_ttl_effective(&self) -> Option<Duration> {
        if self.l3 { self.l3_ttl } else { None }
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
