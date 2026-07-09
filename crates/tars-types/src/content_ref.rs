//! [`ContentRef`] — opaque, tenant-scoped handle to an `LlmRecord`
//! stored in some `LlmRecordStore` (defined in `tars_melt::event`). See
//! [Doc 17 §6](../../../docs/architecture/17-pipeline-event-store.md) for the full
//! design + tenant-isolation rationale.
//!
//! Self-contained — carries `tenant_id` so callers can't accidentally
//! cross-tenant fetch. `LlmRecordStore::fetch(&ContentRef)` enforces
//! scoping internally; no caller-provided tenant parameter, no
//! foot-gun, no probe vector.
//!
//! Cross-tenant record dedup is **forbidden** by Doc 06 §1
//! (tenant isolation). Same content bytes from two tenants get two
//! distinct `ContentRef` (different `tenant_id` prefixes in store
//! key). Within-tenant dedup still happens — most storage savings
//! come from re-asking the same prompt within a tenant anyway.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::TenantId;

/// Opaque handle to content in an `LlmRecordStore`. Constructed at write
/// time from the content bytes + the tenant context; resolved at read
/// time via `LlmRecordStore::fetch`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentRef {
    tenant_id: TenantId,
    content_hash: [u8; 32],
}

impl ContentRef {
    /// Compute a `ContentRef` for `content` under `tenant_id`. The hash
    /// is sha256 of the raw content bytes; tenant_id is stored alongside
    /// (NOT included in the hash) so the hash itself is content-only
    /// and can be used for analytics dedup independent of tenant.
    pub fn from_content(tenant_id: TenantId, content: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(content);
        Self {
            tenant_id,
            content_hash: h.finalize().into(),
        }
    }

    /// Construct from already-computed parts. Useful for
    /// deserialisation paths (event log replay) and for tests.
    pub fn from_parts(tenant_id: TenantId, content_hash: [u8; 32]) -> Self {
        Self {
            tenant_id,
            content_hash,
        }
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    pub fn content_hash(&self) -> &[u8; 32] {
        &self.content_hash
    }

    /// 64-char lowercase hex of the content hash. Convenient for log
    /// lines + SQLite text storage.
    pub fn content_hash_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.content_hash {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl fmt::Display for ContentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // First 8 hex chars is enough to disambiguate in logs;
        // full hash is in `content_hash_hex()` if needed.
        write!(
            f,
            "ContentRef({}, {}…)",
            self.tenant_id.as_ref(),
            &self.content_hash_hex()[..8]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_different_tenants_have_distinct_refs() {
        let content = b"hello world";
        let a = ContentRef::from_content(TenantId::new("tenant-a"), content);
        let b = ContentRef::from_content(TenantId::new("tenant-b"), content);
        // Hash is identical (content-only)…
        assert_eq!(a.content_hash(), b.content_hash());
        // …but the refs are not equal because tenant_id differs.
        assert_ne!(a, b);
    }

    #[test]
    fn same_content_same_tenant_dedups() {
        let content = b"hello world";
        let a = ContentRef::from_content(TenantId::new("tenant-a"), content);
        let b = ContentRef::from_content(TenantId::new("tenant-a"), content);
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_hex_is_64_lowercase() {
        let r = ContentRef::from_content(TenantId::new("t"), b"x");
        let hex = r.content_hash_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
