//! [`ContentRef`] — opaque, tenant-scoped handle to a body stored in
//! some `BodyStore` (defined in `tars-storage`). See
//! [Doc 17 §6](../../../docs/17-pipeline-event-store.md) for the full
//! design + tenant-isolation rationale.
//!
//! Self-contained — carries `tenant_id` so callers can't accidentally
//! cross-tenant fetch. `BodyStore::fetch(&ContentRef)` enforces
//! scoping internally; no caller-provided tenant parameter, no
//! foot-gun, no probe vector.
//!
//! Cross-tenant body dedup is **forbidden** by Doc 06 §1
//! (tenant isolation). Same body bytes from two tenants get two
//! distinct `ContentRef` (different `tenant_id` prefixes in store
//! key). Within-tenant dedup still happens — most storage savings
//! come from re-asking the same prompt within a tenant anyway.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::TenantId;

/// Opaque handle to a body in a `BodyStore`. Constructed at write
/// time from the body bytes + the tenant context; resolved at read
/// time via `BodyStore::fetch`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentRef {
    tenant_id: TenantId,
    body_hash: [u8; 32],
}

impl ContentRef {
    /// Compute a `ContentRef` for `body` under `tenant_id`. The hash
    /// is sha256 of the raw body bytes; tenant_id is stored alongside
    /// (NOT included in the hash) so the hash itself is content-only
    /// and can be used for analytics dedup independent of tenant.
    pub fn from_body(tenant_id: TenantId, body: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(body);
        Self { tenant_id, body_hash: h.finalize().into() }
    }

    /// Construct from already-computed parts. Useful for
    /// deserialisation paths (event log replay) and for tests.
    pub fn from_parts(tenant_id: TenantId, body_hash: [u8; 32]) -> Self {
        Self { tenant_id, body_hash }
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    pub fn body_hash(&self) -> &[u8; 32] {
        &self.body_hash
    }

    /// 64-char lowercase hex of the body hash. Convenient for log
    /// lines + SQLite text storage.
    pub fn body_hash_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.body_hash {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl fmt::Display for ContentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // First 8 hex chars is enough to disambiguate in logs;
        // full hash is in `body_hash_hex()` if needed.
        write!(
            f,
            "ContentRef({}, {}…)",
            self.tenant_id.as_ref(),
            &self.body_hash_hex()[..8]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_body_different_tenants_have_distinct_refs() {
        let body = b"hello world";
        let a = ContentRef::from_body(TenantId::new("tenant-a"), body);
        let b = ContentRef::from_body(TenantId::new("tenant-b"), body);
        // Hash is identical (content-only)…
        assert_eq!(a.body_hash(), b.body_hash());
        // …but the refs are not equal because tenant_id differs.
        assert_ne!(a, b);
    }

    #[test]
    fn same_body_same_tenant_dedups() {
        let body = b"hello world";
        let a = ContentRef::from_body(TenantId::new("tenant-a"), body);
        let b = ContentRef::from_body(TenantId::new("tenant-a"), body);
        assert_eq!(a, b);
    }

    #[test]
    fn body_hash_hex_is_64_lowercase() {
        let r = ContentRef::from_body(TenantId::new("t"), b"x");
        let hex = r.body_hash_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
