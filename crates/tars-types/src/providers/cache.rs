//! Says how a request asks the provider to cache ([`CacheDirective`]) and names a cache object
//! the provider holds ([`ProviderCacheHandle`]). A handle always carries its tenant, and
//! [`ProviderCacheHandle::validate_for_use`] checks it against the tenant about to use it.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::ids::{ProviderId, TenantId};

/// How the provider should cache for this request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CacheDirective {
    /// Lets the provider cache on its own (OpenAI's implicit prefix mode).
    Auto,
    /// Marks a cache boundary at this point in the messages (Anthropic `cache_control`).
    MarkBoundary {
        #[serde(with = "duration_secs")]
        ttl: Duration,
    },
    /// Uses a cache object already created on the provider (Gemini `cachedContent`).
    UseExplicit { handle: ProviderCacheHandle },
}

/// A handle to a cache object the provider holds.
///
/// `external_id` is a bearer ticket: handed back to the provider, it grants access to the content
/// it wraps, so `Debug` redacts it. `Serialize` writes it in plaintext.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderCacheHandle {
    pub provider: ProviderId,
    /// The provider's id for the object (`cachedContents/abc123`); a secret.
    pub external_id: String,
    /// The tenant the object belongs to; [`Self::validate_for_use`] refuses any other.
    pub tenant_namespace: TenantId,
    /// Serialized as epoch milliseconds (see [`systemtime_millis`]).
    #[serde(with = "systemtime_millis")]
    pub created_at: SystemTime,
    #[serde(with = "systemtime_millis")]
    pub expires_at: SystemTime,
    pub size_estimate_bytes: Option<u64>,
}

impl ProviderCacheHandle {
    /// Checks that the handle may be used for `expected_tenant` now, before a provider sends its
    /// `external_id`. It reads the system clock.
    ///
    /// # Errors
    ///
    /// Returns why the handle is refused: the tenant differs, the handle has expired,
    /// `created_at` is after `expires_at`, or `external_id` is empty.
    pub fn validate_for_use(&self, expected_tenant: &TenantId) -> Result<(), CacheHandleError> {
        if self.tenant_namespace != *expected_tenant {
            return Err(CacheHandleError::CrossTenant);
        }
        if self.expires_at <= SystemTime::now() {
            return Err(CacheHandleError::Expired);
        }
        if self.created_at > self.expires_at {
            return Err(CacheHandleError::CreatedAfterExpiry);
        }
        if self.external_id.is_empty() {
            return Err(CacheHandleError::EmptyExternalId);
        }
        Ok(())
    }
}

/// Why a [`ProviderCacheHandle`] may not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CacheHandleError {
    #[error("ProviderCacheHandle: cross-tenant use rejected")]
    CrossTenant,
    #[error("ProviderCacheHandle: expired")]
    Expired,
    #[error("ProviderCacheHandle: created_at > expires_at")]
    CreatedAfterExpiry,
    #[error("ProviderCacheHandle: external_id is empty")]
    EmptyExternalId,
}

impl std::fmt::Debug for ProviderCacheHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCacheHandle")
            .field("provider", &self.provider)
            .field(
                "external_id",
                &format!("<redacted:{}>", self.external_id.len()),
            )
            .field("tenant_namespace", &self.tenant_namespace)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("size_estimate_bytes", &self.size_estimate_bytes)
            .finish()
    }
}

/// What the cache contributed to a response, carried by
/// [`crate::providers::events::ChatEvent::Started`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheHitInfo {
    /// Input tokens the provider served from its prefix cache, implicit or explicit.
    pub cached_input_tokens: u64,
    /// Whether an explicit handle (`cachedContent`) was used.
    pub used_explicit_handle: bool,
}

/// Serializes a `SystemTime` as `i64` epoch milliseconds, for
/// `#[serde(with = "tars_types::systemtime_millis")]`.
///
/// serde's own `SystemTime` form is a `(secs, nanos)` struct that is not a stable wire format and
/// refuses times before 1970; this one represents them as negative numbers.
pub mod systemtime_millis {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        let millis = match t.duration_since(UNIX_EPOCH) {
            // A u128 that does not fit is refused: wrapped, it would read back as a pre-1970 time.
            Ok(d) => i64::try_from(d.as_millis())
                .map_err(|_| S::Error::custom("SystemTime too far in the future for i64 millis"))?,
            Err(e) => {
                let neg = i64::try_from(e.duration().as_millis()).map_err(|_| {
                    S::Error::custom("SystemTime too far in the past for i64 millis")
                })?;
                -neg
            }
        };
        s.serialize_i64(millis)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let millis = i64::deserialize(d)?;
        if millis >= 0 {
            Ok(UNIX_EPOCH + Duration::from_millis(millis as u64))
        } else {
            // `unsigned_abs()` handles i64::MIN, which `-millis` would
            // overflow on.
            Ok(UNIX_EPOCH - Duration::from_millis(millis.unsigned_abs()))
        }
    }
}

/// Serializes a `Duration` as whole seconds, dropping any fraction.
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
        let d = CacheDirective::MarkBoundary {
            ttl: Duration::from_secs(300),
        };
        let v = serde_json::to_value(&d).unwrap();
        let back: CacheDirective = serde_json::from_value(v).unwrap();
        if let CacheDirective::MarkBoundary { ttl } = back {
            assert_eq!(ttl, Duration::from_secs(300));
        } else {
            panic!("wrong variant");
        }
    }

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
        let d = CacheDirective::UseExplicit {
            handle: handle.clone(),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert!(v["handle"]["created_at"].is_i64());
        let back: CacheDirective = serde_json::from_value(v).unwrap();
        if let CacheDirective::UseExplicit { handle: h } = back {
            assert_eq!(h.external_id, "cachedContents/abc123");
            assert_eq!(h.tenant_namespace, handle.tenant_namespace);
            assert_eq!(h.size_estimate_bytes, Some(2048));
            // The round trip keeps the time to the millisecond.
            let original_ms = now
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
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
