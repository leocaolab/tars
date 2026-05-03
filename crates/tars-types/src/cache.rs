//! Cache directives + Provider-side cache handles.
//!
//! Two distinct concerns live here:
//!
//! 1. [`CacheDirective`] — what the *caller* asks the Provider to do
//!    about caching for *this request* (auto / mark boundary / use
//!    pre-created handle).
//! 2. [`ProviderCacheHandle`] — opaque reference to a Provider-side
//!    cache object (Gemini cachedContent, Anthropic cache_control
//!    block, …). Strict invariants (Doc 03 §10):
//!      - never serialized to clients
//!      - tenant_namespace is mandatory
//!      - validated by the Provider on use

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::ids::{ProviderId, TenantId};

/// Directs how the Provider should treat caching for this request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CacheDirective {
    /// Let the provider auto-cache (OpenAI's implicit prefix mode).
    Auto,
    /// Mark a cache boundary at this point in the messages stream
    /// (Anthropic `cache_control` markers).
    MarkBoundary {
        #[serde(with = "duration_secs")]
        ttl: Duration,
    },
    /// Reference an already-created Provider-side cache object
    /// (Gemini `cachedContent`).
    UseExplicit { handle: ProviderCacheHandle },
}

/// Opaque handle to a Provider-side cache object.
///
/// **Security**: `external_id` is a bearer-style "claim ticket" (Doc 03
/// §10.4) — handed back to the provider it grants access to whatever
/// content it wraps. Never expose to clients/logs in plaintext.
///
/// `Debug` is implemented manually to redact `external_id`. The default
/// derive would print it via `{:?}`, defeating the "treat as secret"
/// invariant. Audit findings `tars-types-src-cache-2..4`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderCacheHandle {
    pub provider: ProviderId,
    /// Provider-side ID (e.g. `cachedContents/abc123`). Treat as secret.
    pub external_id: String,
    /// Mandatory — used by Provider to reject cross-tenant use.
    pub tenant_namespace: TenantId,
    /// Audit `tars-types-src-cache-1`: serialized as epoch-millis (i64
    /// `ms_since_epoch`) so the on-disk / on-wire format is portable
    /// across platforms and Rust versions. Bare `SystemTime` serde uses
    /// a tagged `(secs, nanos)` struct that is *not* a stable wire
    /// format — pre-Unix-epoch times even error out on some platforms.
    #[serde(with = "systemtime_millis")]
    pub created_at: SystemTime,
    #[serde(with = "systemtime_millis")]
    pub expires_at: SystemTime,
    pub size_estimate_bytes: Option<u64>,
}

impl ProviderCacheHandle {
    /// Reject a handle that's expired, malformed, or from a different
    /// tenant. Provider adapters should call this before using the
    /// `external_id` to fetch / reference cached content. Audit
    /// `tars-types-src-cache-9` (encapsulation of security invariants).
    pub fn validate_for_use(&self, expected_tenant: &TenantId) -> Result<(), &'static str> {
        if self.tenant_namespace != *expected_tenant {
            return Err("ProviderCacheHandle: cross-tenant use rejected");
        }
        if self.expires_at <= SystemTime::now() {
            return Err("ProviderCacheHandle: expired");
        }
        if self.created_at > self.expires_at {
            return Err("ProviderCacheHandle: created_at > expires_at");
        }
        if self.external_id.is_empty() {
            return Err("ProviderCacheHandle: external_id is empty");
        }
        Ok(())
    }
}

impl std::fmt::Debug for ProviderCacheHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCacheHandle")
            .field("provider", &self.provider)
            .field("external_id", &format!("<redacted:{}>", self.external_id.len()))
            .field("tenant_namespace", &self.tenant_namespace)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("size_estimate_bytes", &self.size_estimate_bytes)
            .finish()
    }
}

/// Diagnostic info about a cache hit, surfaced through [`crate::events::ChatEvent::Started`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheHitInfo {
    /// Tokens served from prefix cache (Provider-implicit or explicit).
    pub cached_input_tokens: u64,
    /// Whether an explicit handle was used (L3 / `cachedContent`).
    pub used_explicit_handle: bool,
    /// True when the *response* was served entirely from L1/L2 cache —
    /// no provider call happened. Distinct from `cached_input_tokens`,
    /// which only marks L3 / provider-implicit prefix discounts. M1
    /// cache middleware sets this on full L1 replay.
    #[serde(default)]
    pub replayed_from_cache: bool,
}

/// Portable epoch-millis serde for `SystemTime`. Stored as `i64` so the
/// pre-1970 case (rare but possible on time-skewed test fixtures) is
/// representable instead of erroring out the way the default
/// `SystemTime` serde does on negative durations.
///
/// Public so other crates that persist `SystemTime` (cache L2,
/// future event store) can reuse the same wire format via
/// `#[serde(with = "tars_types::systemtime_millis")]`.
pub mod systemtime_millis {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let millis = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        };
        s.serialize_i64(millis)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let millis = i64::deserialize(d)?;
        if millis >= 0 {
            Ok(UNIX_EPOCH + Duration::from_millis(millis as u64))
        } else {
            Ok(UNIX_EPOCH - Duration::from_millis((-millis) as u64))
        }
    }
}

/// Custom serde for `Duration` as integer seconds — TOML / JSON friendly.
mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_round_trips() {
        let d = CacheDirective::MarkBoundary { ttl: Duration::from_secs(300) };
        let v = serde_json::to_value(&d).unwrap();
        let back: CacheDirective = serde_json::from_value(v).unwrap();
        if let CacheDirective::MarkBoundary { ttl } = back {
            assert_eq!(ttl, Duration::from_secs(300));
        } else {
            panic!("wrong variant");
        }
    }

    /// Audit `tars-types-src-cache-7`: only MarkBoundary was previously
    /// covered. Auto and UseExplicit need to round-trip too — the
    /// latter especially since it transports `ProviderCacheHandle`
    /// with non-trivial `SystemTime` fields.
    #[test]
    fn auto_directive_round_trips() {
        let d = CacheDirective::Auto;
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["mode"], "auto");
        let back: CacheDirective = serde_json::from_value(v).unwrap();
        assert!(matches!(back, CacheDirective::Auto));
    }

    #[test]
    fn use_explicit_directive_round_trips_with_handle() {
        let now = SystemTime::now();
        let handle = ProviderCacheHandle {
            provider: ProviderId::new("anthropic"),
            external_id: "cachedContents/abc123".into(),
            tenant_namespace: TenantId::new("tenant-1"),
            created_at: now,
            expires_at: now + Duration::from_secs(3600),
            size_estimate_bytes: Some(2048),
        };
        let d = CacheDirective::UseExplicit { handle: handle.clone() };
        let v = serde_json::to_value(&d).unwrap();
        // Portable epoch-millis on the wire — not a tagged secs/nanos struct.
        assert!(v["handle"]["created_at"].is_i64());
        let back: CacheDirective = serde_json::from_value(v).unwrap();
        if let CacheDirective::UseExplicit { handle: h } = back {
            assert_eq!(h.external_id, "cachedContents/abc123");
            assert_eq!(h.tenant_namespace, handle.tenant_namespace);
            assert_eq!(h.size_estimate_bytes, Some(2048));
            // Round-trip preserves the absolute time to ms precision;
            // we accept the truncation from sub-ms loss.
            let original_ms = now.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
            let recovered_ms = h
                .created_at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            assert_eq!(original_ms, recovered_ms);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn handle_validate_rejects_cross_tenant() {
        let now = SystemTime::now();
        let h = ProviderCacheHandle {
            provider: ProviderId::new("p"),
            external_id: "ext".into(),
            tenant_namespace: TenantId::new("t1"),
            created_at: now,
            expires_at: now + Duration::from_secs(60),
            size_estimate_bytes: None,
        };
        assert!(h.validate_for_use(&TenantId::new("t1")).is_ok());
        assert!(h.validate_for_use(&TenantId::new("t2")).is_err());
    }

    #[test]
    fn handle_validate_rejects_expired() {
        let h = ProviderCacheHandle {
            provider: ProviderId::new("p"),
            external_id: "ext".into(),
            tenant_namespace: TenantId::new("t1"),
            created_at: SystemTime::now() - Duration::from_secs(120),
            expires_at: SystemTime::now() - Duration::from_secs(60),
            size_estimate_bytes: None,
        };
        assert!(h.validate_for_use(&TenantId::new("t1")).is_err());
    }

    #[test]
    fn handle_debug_redacts_external_id() {
        let h = ProviderCacheHandle {
            provider: ProviderId::new("p"),
            external_id: "super-secret-cache-id".into(),
            tenant_namespace: TenantId::new("t1"),
            created_at: SystemTime::UNIX_EPOCH,
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(60),
            size_estimate_bytes: None,
        };
        let s = format!("{h:?}");
        assert!(!s.contains("super-secret-cache-id"));
        assert!(s.contains("redacted"));
    }
}
