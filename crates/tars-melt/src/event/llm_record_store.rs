//! [`LlmRecordStore`] — tenant-scoped CAS for the per-call `LlmRecord`
//! (ChatRequest / ChatResponse content) referenced from
//! `PipelineEvent`. See
//! [Doc 17 §6.1](../../../docs/architecture/17-pipeline-event-store.md).
//!
//! `LlmRecordStore::fetch(&ContentRef)` resolves records; `ContentRef`
//! itself carries `tenant_id`, so the store can't be tricked into
//! cross-tenant fetches.
//!
//! Retention: `purge_before(cutoff)` and `purge_tenant(id)` are first-
//! class trait methods so v2 backends (codex-style date-partitioned
//! sqlite-per-day, S3 with lifecycle rules, postgres bytea) can
//! implement these as physical operations rather than full-table
//! scans.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use tars_storage::Db;

use tars_types::TenantId;

use super::{ContentRef, StoreError};

/// Embedded versioned schema (`migrations/llm_record_store/`). Applied once at
/// open on the store's own pool; `_sqlx_migrations` is the version-of-record.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/llm_record_store");

#[async_trait]
pub trait LlmRecordStore: Send + Sync + 'static {
    /// Store record bytes under `r`. Idempotent — re-storing identical
    /// `(tenant_id, content_hash)` is a no-op (CAS semantic).
    async fn put(&self, r: &ContentRef, bytes: Bytes) -> Result<(), StoreError>;

    /// Fetch record bytes for `r`. `Ok(None)` means "no such record"
    /// (e.g. purged); errors are reserved for backend faults.
    async fn fetch(&self, r: &ContentRef) -> Result<Option<Bytes>, StoreError>;

    /// Drop all records older than `cutoff`. Returns count removed.
    /// Implementations CAN do this efficiently (codex-style date dirs
    /// → `rm -rf`); v1 sqlite impl runs `DELETE WHERE created_at < ?`.
    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StoreError>;

    /// Drop a tenant's entire record footprint. Required for tenant-
    /// delete compliance. Implementations MUST partition by
    /// `tenant_id` so this is O(tenant), not O(all records).
    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StoreError>;
}

#[derive(Clone, Debug)]
pub struct SqliteLlmRecordStoreConfig {
    pub path: PathBuf,
}

impl SqliteLlmRecordStoreConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone)]
pub struct SqliteLlmRecordStore {
    /// The one pool for this store's DB file. Cheap to clone (Arc inside sqlx).
    db: Db,
}

impl SqliteLlmRecordStore {
    /// Construct on an **injected** pool (connection 下沉) — the composition
    /// root opens it once via [`crate::pool::open`] and hands it in; the store
    /// carries a pool, never a path. Runs this store's migrator on the pool.
    pub async fn new(db: Db) -> Result<Arc<Self>, StoreError> {
        db.migrate(&MIGRATOR)
            .await
            .map_err(|e| StoreError::backend_source("schema migration", e))?;
        Ok(Arc::new(Self { db }))
    }

    /// Thin convenience over the DI seam: open the file pool + [`new`](Self::new).
    /// Prefer `Db::open_sqlite(path)` + `new(pool)` at the composition root.
    pub async fn open(config: SqliteLlmRecordStoreConfig) -> Result<Arc<Self>, StoreError> {
        let db = Db::open_sqlite(&config.path).await.map_err(|e| {
            StoreError::backend_source(format!("opening llm record store at {:?}", config.path), e)
        })?;
        Self::new(db).await
    }

    /// In-memory store for tests (its own single-connection in-memory pool).
    pub async fn in_memory() -> Result<Arc<Self>, StoreError> {
        let db = Db::sqlite_in_memory()
            .await
            .map_err(|e| StoreError::backend_source("opening in-memory llm record store", e))?;
        Self::new(db).await
    }
}

/// Current wall-clock time as milliseconds since the Unix epoch, for
/// stamping `created_at`.
///
/// Returns `Err` if the clock is before `UNIX_EPOCH`: falling back to
/// `0` would stamp every record with the smallest possible `created_at`,
/// making it instantly eligible for `purge_before` and silently
/// dropping freshly-written records. Far-future is clamped to `i64::MAX`
/// so the `as i64` cast can't wrap negative.
fn now_ms() -> Result<i64, StoreError> {
    use std::time::UNIX_EPOCH;
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .map_err(|e| StoreError::backend_source("system clock is before the Unix epoch", e))
}

/// Convert a caller-supplied `purge_before` cutoff to epoch ms.
///
/// Returns `Err` for a pre-epoch cutoff rather than silently flooring to
/// `0` (which would make the `DELETE WHERE created_at < 0` a guaranteed
/// no-op and mask the invalid input). Far-future is clamped so the cast
/// can't wrap.
fn cutoff_to_ms(t: SystemTime) -> Result<i64, StoreError> {
    use std::time::UNIX_EPOCH;
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .map_err(|_| {
            StoreError::backend(
                "purge_before cutoff is before the Unix epoch; refusing to interpret \
                 a pre-epoch cutoff (would silently match nothing)",
            )
        })
}

#[async_trait]
impl LlmRecordStore for SqliteLlmRecordStore {
    async fn put(&self, r: &ContentRef, bytes: Bytes) -> Result<(), StoreError> {
        // Stamp `created_at` at the actual write moment (not when `put` was
        // called) so a delayed write under load can't be stamped as already
        // eligible for a concurrent `purge_before`.
        let now = now_ms()?;
        let tenant = r.tenant_id().as_ref().to_string();
        let hash = r.content_hash().to_vec();
        let content = bytes.to_vec();

        // INSERT OR IGNORE — idempotent CAS write. Re-storing identical bytes
        // for the same (tenant, hash) is a no-op.
        sqlx::query(
            "INSERT OR IGNORE INTO llm_records (tenant_id, content_hash, content, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind(hash)
        .bind(content)
        .bind(now)
        .execute(self.db.sqlite())
        .await
        .map_err(|e| StoreError::backend_source("insert llm record", e))?;

        Ok(())
    }

    async fn fetch(&self, r: &ContentRef) -> Result<Option<Bytes>, StoreError> {
        let tenant = r.tenant_id().as_ref().to_string();
        let hash = r.content_hash().to_vec();

        let bytes = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT content FROM llm_records WHERE tenant_id = ? AND content_hash = ?",
        )
        .bind(tenant)
        .bind(hash)
        .fetch_optional(self.db.sqlite())
        .await
        .map_err(|e| StoreError::backend_source("fetch llm record", e))?;

        Ok(bytes.map(Bytes::from))
    }

    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StoreError> {
        let cutoff_ms = cutoff_to_ms(cutoff)?;

        let res = sqlx::query("DELETE FROM llm_records WHERE created_at < ?")
            .bind(cutoff_ms)
            .execute(self.db.sqlite())
            .await
            .map_err(|e| StoreError::backend_source("purge_before", e))?;

        Ok(res.rows_affected())
    }

    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StoreError> {
        let tenant = tenant_id.as_ref().to_string();

        let res = sqlx::query("DELETE FROM llm_records WHERE tenant_id = ?")
            .bind(tenant)
            .execute(self.db.sqlite())
            .await
            .map_err(|e| StoreError::backend_source("purge_tenant", e))?;

        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn store() -> Arc<SqliteLlmRecordStore> {
        SqliteLlmRecordStore::in_memory()
            .await
            .expect("open in-memory store")
    }

    fn cref(tenant: &str, body: &[u8]) -> ContentRef {
        ContentRef::from_content(TenantId::new(tenant), body)
    }

    #[tokio::test]
    async fn put_then_fetch_round_trips() {
        let s = store().await;
        let r = cref("t1", b"hello");
        s.put(&r, Bytes::from_static(b"hello")).await.unwrap();
        let got = s.fetch(&r).await.unwrap().expect("body present");
        assert_eq!(&got[..], b"hello");
    }

    #[tokio::test]
    async fn fetch_missing_returns_none_not_error() {
        let s = store().await;
        let r = cref("t1", b"nonexistent");
        assert!(s.fetch(&r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let s = store().await;
        let r = cref("t1", b"hello");
        s.put(&r, Bytes::from_static(b"hello")).await.unwrap();
        // Second put with same key is a no-op (CAS).
        s.put(&r, Bytes::from_static(b"hello")).await.unwrap();
        let got = s.fetch(&r).await.unwrap().expect("still there");
        assert_eq!(&got[..], b"hello");
    }

    #[tokio::test]
    async fn cross_tenant_fetch_misses() {
        let s = store().await;
        let body = b"shared";
        let a = cref("tenant-a", body);
        let b = cref("tenant-b", body);
        s.put(&a, Bytes::from_static(body)).await.unwrap();
        // Even though body bytes are identical and hash matches,
        // different tenant prefix = cache miss for tenant-b. This is
        // the explicit Doc 17 §6 contract — Doc 06 isolation trumps
        // dedup.
        assert!(s.fetch(&b).await.unwrap().is_none());
        // tenant-a still hits.
        assert!(s.fetch(&a).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn purge_tenant_drops_only_that_tenant() {
        let s = store().await;
        let a = cref("tenant-a", b"a-data");
        let b = cref("tenant-b", b"b-data");
        s.put(&a, Bytes::from_static(b"a-data")).await.unwrap();
        s.put(&b, Bytes::from_static(b"b-data")).await.unwrap();

        let n = s.purge_tenant(&TenantId::new("tenant-a")).await.unwrap();
        assert_eq!(n, 1);
        assert!(s.fetch(&a).await.unwrap().is_none());
        assert!(s.fetch(&b).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn purge_before_uses_created_at_cutoff() {
        let s = store().await;
        let r = cref("t1", b"old");
        s.put(&r, Bytes::from_static(b"old")).await.unwrap();
        // Cutoff in the future — should drop everything.
        let future = SystemTime::now() + Duration::from_secs(60);
        let n = s.purge_before(future).await.unwrap();
        assert_eq!(n, 1);
        assert!(s.fetch(&r).await.unwrap().is_none());
    }
}
