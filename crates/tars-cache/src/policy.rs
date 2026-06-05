//! Per-request cache policy. Threaded through `RequestContext.attributes`
//! by upstream callers (Agent layer / explicit override).
//!
//! M1 only honours `l1`. The `l2`/`l3` fields are placeholders that
//! match Doc 03 §2.2's full policy shape so the public API doesn't
//! change when the other levels land.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Per-level cache policy. Sum-as-product fix for the original
/// `(bool, Option<Duration>)` shape that admitted the meaningless
/// "disabled but has a TTL override" state (`arc scan --judge`
/// finding `ARC-L5-B-7`).
///
/// - `Disabled` — level off, no key compute / lookup / write.
/// - `Default` — level on, use the registry's configured TTL.
/// - `Override { ttl }` — level on, use `ttl` instead of the default.
///
/// **Wire shape** (since `ARC-L5-B-5` killed the legacy flat adapter):
/// natural serde tagged-enum JSON, with `snake_case` variant tags so
/// the on-disk form reads idiomatically:
///
/// ```text
///   "disabled"
///   "default"
///   {"override": {"ttl": {"secs": 60, "nanos": 0}}}
/// ```
///
/// The illegal `(disabled, Some(ttl))` state is now unrepresentable
/// at every layer — domain type, wire form, and the in-memory
/// `serde_json::Value` carried in `RequestContext.attributes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheLayerPolicy {
    /// Level disabled entirely — no key compute, no lookup, no write.
    Disabled,
    /// Level enabled with the registry's configured default TTL.
    #[default]
    Default,
    /// Level enabled with a per-request TTL override.
    Override { ttl: Duration },
}

impl CacheLayerPolicy {
    /// True iff this layer is on (`Default` or `Override`).
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// The per-request TTL override, if any. Returns `None` for both
    /// `Disabled` and `Default` — the registry's configured default
    /// applies in the latter case.
    pub fn ttl_override(&self) -> Option<Duration> {
        match self {
            Self::Override { ttl } => Some(*ttl),
            _ => None,
        }
    }
}

/// Cache policy across all three tiers.
///
/// **Wire shape** since `ARC-L5-B-5` dropped the bespoke flat-JSON
/// adapter (`CachePolicyWire` and its `lift`/`project` helpers — the
/// helpers carried a `(bool, Option<Duration>)` tuple that was itself
/// `ARC-L5-D-3` ROT). Now derived `Serialize`/`Deserialize` over the
/// typed enum per layer:
///
/// ```text
///   {"l1": "default", "l2": "default", "l3": "disabled"}
///   {"l1": {"override": {"ttl": {"secs": 60, "nanos": 0}}}, ...}
/// ```
///
/// The wire format is internal: `RequestContext.attributes` is an
/// in-memory `HashMap<String, serde_json::Value>` per request — not a
/// persisted store — so there is no legacy on-disk payload that needs
/// to keep round-tripping. The adapter that lived here was defensive
/// code for a producer that never materialised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePolicy {
    pub l1: CacheLayerPolicy,
    /// **Not yet honoured** — landed when `tars-storage` ships L2.
    pub l2: CacheLayerPolicy,
    /// **Not yet honoured** — landed when `ExplicitCacheProvider`
    /// (D-1) ships.
    pub l3: CacheLayerPolicy,
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
            l1: CacheLayerPolicy::Default,
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        }
    }
}

impl CachePolicy {
    /// Disable caching entirely — useful for tests / debugging /
    /// explicit `--no-cache` flag down the road.
    pub fn off() -> Self {
        Self {
            l1: CacheLayerPolicy::Disabled,
            l2: CacheLayerPolicy::Disabled,
            l3: CacheLayerPolicy::Disabled,
        }
    }

    /// True iff at least one tier is enabled. The middleware short-
    /// circuits the entire cache pipeline (no key compute, no lookup,
    /// no write) when this returns false.
    pub fn any_enabled(&self) -> bool {
        self.l1.is_enabled() || self.l2.is_enabled() || self.l3.is_enabled()
    }

    /// L1 TTL override, observing the "ignored when L1 is off" rule.
    /// Returns `None` when L1 is `Disabled` *or* when it's `Default`
    /// (no per-request override).
    pub fn l1_ttl_effective(&self) -> Option<Duration> {
        self.l1.ttl_override()
    }

    /// See [`Self::l1_ttl_effective`].
    pub fn l2_ttl_effective(&self) -> Option<Duration> {
        self.l2.ttl_override()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_l1_and_l2() {
        let p = CachePolicy::default();
        assert!(p.l1.is_enabled());
        assert!(p.l2.is_enabled());
        assert!(!p.l3.is_enabled(), "L3 stays opt-in per Doc 03 §2.2");
        assert!(p.any_enabled());
    }

    #[test]
    fn off_disables_everything() {
        assert!(!CachePolicy::off().any_enabled());
    }

    #[test]
    fn any_enabled_covers_all_eight_states() {
        for bits in 0u8..8 {
            let on = |i: u8| bits & (1 << i) != 0;
            let p = CachePolicy {
                l1: if on(0) {
                    CacheLayerPolicy::Default
                } else {
                    CacheLayerPolicy::Disabled
                },
                l2: if on(1) {
                    CacheLayerPolicy::Default
                } else {
                    CacheLayerPolicy::Disabled
                },
                l3: if on(2) {
                    CacheLayerPolicy::Default
                } else {
                    CacheLayerPolicy::Disabled
                },
            };
            assert_eq!(p.any_enabled(), bits != 0, "bits={bits:03b}");
        }
    }

    // ─── Wire-format pins ─────────────────────────────────────────────

    #[test]
    fn layer_policy_serialises_as_snake_case_tagged_enum() {
        // Default external tag, snake_case rename. These are the JSON
        // shapes anything reading `cache.policy` from an attribute map
        // will see; pinning the strings keeps drift detectable.
        assert_eq!(
            serde_json::to_value(CacheLayerPolicy::Disabled).unwrap(),
            serde_json::json!("disabled")
        );
        assert_eq!(
            serde_json::to_value(CacheLayerPolicy::Default).unwrap(),
            serde_json::json!("default")
        );
        assert_eq!(
            serde_json::to_value(CacheLayerPolicy::Override {
                ttl: Duration::from_secs(60)
            })
            .unwrap(),
            serde_json::json!({"override": {"ttl": {"secs": 60, "nanos": 0}}})
        );
    }

    #[test]
    fn policy_serialises_as_three_named_layers() {
        let p = CachePolicy::default();
        assert_eq!(
            serde_json::to_value(p).unwrap(),
            serde_json::json!({
                "l1": "default",
                "l2": "default",
                "l3": "disabled",
            })
        );
    }

    #[test]
    fn policy_round_trips_through_serde_for_all_layer_variants() {
        let p = CachePolicy {
            l1: CacheLayerPolicy::Override {
                ttl: Duration::from_secs(60),
            },
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        };
        let json = serde_json::to_value(p).unwrap();
        let back: CachePolicy = serde_json::from_value(json).unwrap();
        assert_eq!(p, back);
    }
}
