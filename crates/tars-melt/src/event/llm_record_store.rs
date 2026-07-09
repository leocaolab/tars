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
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};

use tars_types::{ContentRef, TenantId};

use super::StoreError;

const SCHEMA_VERSION: i64 = 1;

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
    conn: Arc<Mutex<Connection>>,
}

impl SqliteLlmRecordStore {
    pub fn open(config: SqliteLlmRecordStoreConfig) -> Result<Arc<Self>, StoreError> {
        let conn = Connection::open(&config.path).map_err(|e| {
            StoreError::backend_source(format!("opening llm record store at {:?}", config.path), e)
        })?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self {
            conn: Arc::new(Mutex::new(conn)),
        }))
    }

    pub fn in_memory() -> Result<Arc<Self>, StoreError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StoreError::backend_source("opening in-memory llm record store", e))?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self {
            conn: Arc::new(Mutex::new(conn)),
        }))
    }

    fn pragma_setup(conn: &Connection) -> Result<(), StoreError> {
        for (name, value) in [
            ("journal_mode", "WAL"),
            ("synchronous", "NORMAL"),
            ("temp_store", "MEMORY"),
        ] {
            conn.pragma_update(None, name, value)
                .map_err(|e| StoreError::backend_source("pragma {name}", e))?;
        }
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        let current: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|e| StoreError::backend_source("read user_version", e))?;
        if current == SCHEMA_VERSION {
            return Ok(());
        }
        if current != 0 {
            return Err(StoreError::backend(format!(
                "incompatible llm record store schema (file v{current}, code v{SCHEMA_VERSION})"
            )));
        }
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS llm_records (
                tenant_id   TEXT    NOT NULL,
                content_hash   BLOB    NOT NULL,
                content     BLOB    NOT NULL,
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (tenant_id, content_hash)
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_llm_records_created_at
                ON llm_records(created_at);
            "#,
        )
        .map_err(|e| StoreError::backend_source("create llm record schema", e))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| StoreError::backend_source("set user_version", e))?;
        Ok(())
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
        let conn = self.conn.clone();
        let tenant = r.tenant_id().as_ref().to_string();
        let hash = r.content_hash().to_vec();
        let content = bytes.to_vec();

        tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
            // Capture the timestamp inside the blocking closure so it
            // reflects the moment the row is actually written, not the
            // (possibly much earlier, under load) moment `put` was
            // called — otherwise a delayed write could race a concurrent
            // `purge_before` and be stamped as already-purgeable.
            let now = now_ms()?;
            let conn = conn.lock().expect("llm record store mutex poisoned");
            // INSERT OR IGNORE — idempotent CAS write. Re-storing
            // identical bytes for the same (tenant, hash) is a no-op.
            conn.execute(
                "INSERT OR IGNORE INTO llm_records (tenant_id, content_hash, content, created_at) \
                 VALUES (?, ?, ?, ?)",
                params![tenant, hash, content, now],
            )
            .map_err(|e| StoreError::backend_source("insert llm record", e))?;
            Ok(())
        })
        .await
        .map_err(|e| StoreError::backend_source("spawn_blocking", e))??;

        Ok(())
    }

    async fn fetch(&self, r: &ContentRef) -> Result<Option<Bytes>, StoreError> {
        let conn = self.conn.clone();
        let tenant = r.tenant_id().as_ref().to_string();
        let hash = r.content_hash().to_vec();

        let bytes =
            tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, StoreError> {
                let conn = conn.lock().expect("llm record store mutex poisoned");
                conn.query_row(
                    "SELECT content FROM llm_records WHERE tenant_id = ? AND content_hash = ?",
                    params![tenant, hash],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(|e| StoreError::backend_source("fetch llm record", e))
            })
            .await
            .map_err(|e| StoreError::backend_source("spawn_blocking", e))??;

        Ok(bytes.map(Bytes::from))
    }

    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StoreError> {
        let conn = self.conn.clone();
        let cutoff_ms = cutoff_to_ms(cutoff)?;

        let n = tokio::task::spawn_blocking(move || -> Result<u64, StoreError> {
            let conn = conn.lock().expect("llm record store mutex poisoned");
            let n = conn
                .execute(
                    "DELETE FROM llm_records WHERE created_at < ?",
                    params![cutoff_ms],
                )
                .map_err(|e| StoreError::backend_source("purge_before", e))?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| StoreError::backend_source("spawn_blocking", e))??;

        Ok(n)
    }

    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StoreError> {
        let conn = self.conn.clone();
        let tenant = tenant_id.as_ref().to_string();

        let n = tokio::task::spawn_blocking(move || -> Result<u64, StoreError> {
            let conn = conn.lock().expect("llm record store mutex poisoned");
            let n = conn
                .execute("DELETE FROM llm_records WHERE tenant_id = ?", params![tenant])
                .map_err(|e| StoreError::backend_source("purge_tenant", e))?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| StoreError::backend_source("spawn_blocking", e))??;

        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn store() -> Arc<SqliteLlmRecordStore> {
        SqliteLlmRecordStore::in_memory().expect("open in-memory store")
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
