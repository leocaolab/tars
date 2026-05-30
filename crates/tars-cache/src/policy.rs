//! Per-request cache policy. Threaded through `RequestContext.attributes`
//! by upstream callers (Agent layer / explicit override).
//!
//! M1 only honours `l1`. The `l2`/`l3` fields are placeholders that
//! match Doc 03 §2.2's full policy shape so the public API doesn't
//! change when the other levels land.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Per-level cache policy. Sum-as-product fix for the original
/// `(bool, Option<Duration>)` shape that admitted the meaningless
/// "disabled but has a TTL override" state (`arc scan --judge`
/// finding `ARC-L5-B-7`).
///
/// - `Disabled` — level off, no key compute / lookup / write.
/// - `Default` — level on, use the registry's configured TTL.
/// - `Override { ttl }` — level on, use `ttl` instead of the default.
///
/// The on-the-wire JSON shape stays as the legacy
/// `(l1: bool, l1_ttl: Option<Duration>)` flat fields on
/// [`CachePolicy`] (preserved via custom `Serialize` / `Deserialize`
/// below), so persisted attribute payloads and event-store replays
/// continue to round-trip unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
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

    /// Internal-helper for the serde adapter: lift the legacy
    /// `(enabled, ttl)` wire pair into the typed enum.
    fn lift(enabled: bool, ttl: Option<Duration>) -> Self {
        match (enabled, ttl) {
            (false, _) => Self::Disabled,
            (true, None) => Self::Default,
            (true, Some(t)) => Self::Override { ttl: t },
        }
    }

    /// Internal-helper for the serde adapter: project to the legacy
    /// `(enabled, ttl)` wire pair.
    fn project(&self) -> (bool, Option<Duration>) {
        match self {
            Self::Disabled => (false, None),
            Self::Default => (true, None),
            Self::Override { ttl } => (true, Some(*ttl)),
        }
    }
}

/// Cache policy across all three tiers. Internally typed
/// [`CacheLayerPolicy`] per level; serialised to/from the legacy
/// flat `(l1: bool, l1_ttl: Option<Duration>)` × 3 JSON shape via
/// the custom `Serialize` / `Deserialize` below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

// ─── Wire-format adapter — preserves legacy flat JSON shape ────────
//
// `RequestContext.attributes` JSON, event-store payloads, and any
// other persisted form of CachePolicy stays as
//   {"l1": true, "l2": true, "l3": false,
//    "l1_ttl": null, "l2_ttl": null, "l3_ttl": null}
// even though the Rust type is now the typed enum per layer. Without
// this adapter, switching to `pub l1: CacheLayerPolicy` would change
// the JSON to a tagged enum form and break consumers reading older
// payloads.

#[derive(Serialize, Deserialize)]
struct CachePolicyWire {
    l1: bool,
    l2: bool,
    l3: bool,
    #[serde(default)]
    l1_ttl: Option<Duration>,
    #[serde(default)]
    l2_ttl: Option<Duration>,
    #[serde(default)]
    l3_ttl: Option<Duration>,
}

impl Serialize for CachePolicy {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let (l1, l1_ttl) = self.l1.project();
        let (l2, l2_ttl) = self.l2.project();
        let (l3, l3_ttl) = self.l3.project();
        CachePolicyWire {
            l1,
            l2,
            l3,
            l1_ttl,
            l2_ttl,
            l3_ttl,
        }
        .serialize(ser)
    }
}

impl<'de> Deserialize<'de> for CachePolicy {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let w = CachePolicyWire::deserialize(de)?;
        Ok(Self {
            l1: CacheLayerPolicy::lift(w.l1, w.l1_ttl),
            l2: CacheLayerPolicy::lift(w.l2, w.l2_ttl),
            l3: CacheLayerPolicy::lift(w.l3, w.l3_ttl),
        })
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

    // ─── Wire-format compatibility ────────────────────────────────

    #[test]
    fn legacy_wire_form_round_trips_unchanged() {
        // The exact JSON shape used by every existing
        // ctx.attributes / event-store payload — must continue to
        // round-trip through the typed CachePolicy.
        let legacy = serde_json::json!({
            "l1": true,
            "l2": true,
            "l3": false,
            "l1_ttl": null,
            "l2_ttl": null,
            "l3_ttl": null,
        });
        let parsed: CachePolicy = serde_json::from_value(legacy.clone()).unwrap();
        assert!(parsed.l1.is_enabled());
        assert_eq!(parsed.l1, CacheLayerPolicy::Default);
        assert_eq!(parsed.l3, CacheLayerPolicy::Disabled);

        let re = serde_json::to_value(&parsed).unwrap();
        assert_eq!(re, legacy, "round-trip drift");
    }

    #[test]
    fn legacy_wire_with_ttl_override_maps_to_override_variant() {
        let v = serde_json::json!({
            "l1": true,
            "l2": false,
            "l3": false,
            "l1_ttl": {"secs": 60, "nanos": 0},
            "l2_ttl": null,
            "l3_ttl": null,
        });
        let p: CachePolicy = serde_json::from_value(v).unwrap();
        assert_eq!(p.l1, CacheLayerPolicy::Override { ttl: Duration::from_secs(60) });
        assert_eq!(p.l2, CacheLayerPolicy::Disabled);
    }

    #[test]
    fn disabled_layer_with_legacy_ttl_field_is_normalised_to_disabled() {
        // The pre-typing footgun: someone wrote `{"l1": false,
        // "l1_ttl": 60s}`. The typed enum collapses this to
        // `Disabled` so the meaningless-but-representable state
        // disappears at the boundary.
        let v = serde_json::json!({
            "l1": false,
            "l2": false,
            "l3": false,
            "l1_ttl": {"secs": 60, "nanos": 0},
            "l2_ttl": null,
            "l3_ttl": null,
        });
        let p: CachePolicy = serde_json::from_value(v).unwrap();
        assert_eq!(p.l1, CacheLayerPolicy::Disabled);
        assert!(p.l1_ttl_effective().is_none());

        // Re-serialise drops the stale ttl too, normalising the wire.
        let re = serde_json::to_value(&p).unwrap();
        assert_eq!(re["l1"], false);
        assert!(re["l1_ttl"].is_null());
    }

    #[test]
    fn missing_ttl_fields_default_to_none() {
        // serde's `default` attribute on the wire struct means a JSON
        // body without `*_ttl` keys still deserialises.
        let v = serde_json::json!({
            "l1": true,
            "l2": true,
            "l3": false,
        });
        let p: CachePolicy = serde_json::from_value(v).unwrap();
        assert_eq!(p, CachePolicy::default());
    }
}
