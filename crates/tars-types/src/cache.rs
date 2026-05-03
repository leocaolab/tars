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
    pub created_at: SystemTime,
    pub expires_at: SystemTime,
    pub size_estimate_bytes: Option<u64>,
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
    /// Whether an explicit handle was used.
    pub used_explicit_handle: bool,
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
}
