//! [`BodyStore`] — tenant-scoped CAS for ChatRequest / ChatResponse
//! bodies referenced from `PipelineEvent`. See
//! [Doc 17 §6.1](../../../docs/17-pipeline-event-store.md).
//!
//! `BodyStore::fetch(&ContentRef)` resolves bodies; `ContentRef`
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
use rusqlite::{params, Connection, OptionalExtension};

use tars_types::{ContentRef, TenantId};

use crate::error::StorageError;

const SCHEMA_VERSION: i64 = 1;

#[async_trait]
pub trait BodyStore: Send + Sync + 'static {
    /// Store body bytes under `r`. Idempotent — re-storing identical
    /// `(tenant_id, body_hash)` is a no-op (CAS semantic).
    async fn put(&self, r: &ContentRef, bytes: Bytes) -> Result<(), StorageError>;

    /// Fetch body bytes for `r`. `Ok(None)` means "no such body" (e.g.
    /// purged); errors are reserved for backend faults.
    async fn fetch(&self, r: &ContentRef) -> Result<Option<Bytes>, StorageError>;

    /// Drop all bodies older than `cutoff`. Returns count removed.
    /// Implementations CAN do this efficiently (codex-style date dirs
    /// → `rm -rf`); v1 sqlite impl runs `DELETE WHERE created_at < ?`.
    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StorageError>;

    /// Drop a tenant's entire body footprint. Required for tenant-
    /// delete compliance. Implementations MUST partition by
    /// `tenant_id` so this is O(tenant), not O(all bodies).
    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StorageError>;
}

#[derive(Clone, Debug)]
pub struct SqliteBodyStoreConfig {
    pub path: PathBuf,
}

impl SqliteBodyStoreConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone)]
pub struct SqliteBodyStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteBodyStore {
    pub fn open(config: SqliteBodyStoreConfig) -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open(&config.path).map_err(|e| {
            StorageError::Backend(format!("opening body store at {:?}: {e}", config.path))
        })?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self { conn: Arc::new(Mutex::new(conn)) }))
    }

    pub fn in_memory() -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StorageError::Backend(format!("opening in-memory body store: {e}")))?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self { conn: Arc::new(Mutex::new(conn)) }))
    }

    fn pragma_setup(conn: &Connection) -> Result<(), StorageError> {
        for (name, value) in [
            ("journal_mode", "WAL"),
            ("synchronous", "NORMAL"),
            ("temp_store", "MEMORY"),
        ] {
            conn.pragma_update(None, name, value)
                .map_err(|e| StorageError::Backend(format!("pragma {name}: {e}")))?;
        }
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<(), StorageError> {
        let current: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|e| StorageError::Backend(format!("read user_version: {e}")))?;
        if current == SCHEMA_VERSION {
            return Ok(());
        }
        if current != 0 {
            return Err(StorageError::Backend(format!(
                "incompatible body store schema (file v{current}, code v{SCHEMA_VERSION})"
            )));
        }
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS bodies (
                tenant_id   TEXT    NOT NULL,
                body_hash   BLOB    NOT NULL,
                body        BLOB    NOT NULL,
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (tenant_id, body_hash)
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_bodies_created_at
                ON bodies(created_at);
            "#,
        )
        .map_err(|e| StorageError::Backend(format!("create body schema: {e}")))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| StorageError::Backend(format!("set user_version: {e}")))?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    use std::time::UNIX_EPOCH;
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cutoff_to_ms(t: SystemTime) -> i64 {
    use std::time::UNIX_EPOCH;
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[async_trait]
impl BodyStore for SqliteBodyStore {
    async fn put(&self, r: &ContentRef, bytes: Bytes) -> Result<(), StorageError> {
        let conn = self.conn.clone();
        let tenant = r.tenant_id().as_ref().to_string();
        let hash = r.body_hash().to_vec();
        let now = now_ms();
        let body = bytes.to_vec();

        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let conn = conn.lock().expect("body store mutex poisoned");
            // INSERT OR IGNORE — idempotent CAS write. Re-storing
            // identical bytes for the same (tenant, hash) is a no-op.
            conn.execute(
                "INSERT OR IGNORE INTO bodies (tenant_id, body_hash, body, created_at) \
                 VALUES (?, ?, ?, ?)",
                params![tenant, hash, body, now],
            )
            .map_err(|e| StorageError::Backend(format!("insert body: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(())
    }

    async fn fetch(&self, r: &ContentRef) -> Result<Option<Bytes>, StorageError> {
        let conn = self.conn.clone();
        let tenant = r.tenant_id().as_ref().to_string();
        let hash = r.body_hash().to_vec();

        let bytes = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, StorageError> {
            let conn = conn.lock().expect("body store mutex poisoned");
            conn.query_row(
                "SELECT body FROM bodies WHERE tenant_id = ? AND body_hash = ?",
                params![tenant, hash],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|e| StorageError::Backend(format!("fetch body: {e}")))
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(bytes.map(Bytes::from))
    }

    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StorageError> {
        let conn = self.conn.clone();
        let cutoff_ms = cutoff_to_ms(cutoff);

        let n = tokio::task::spawn_blocking(move || -> Result<u64, StorageError> {
            let conn = conn.lock().expect("body store mutex poisoned");
            let n = conn
                .execute("DELETE FROM bodies WHERE created_at < ?", params![cutoff_ms])
                .map_err(|e| StorageError::Backend(format!("purge_before: {e}")))?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(n)
    }

    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant_id.as_ref().to_string();

        let n = tokio::task::spawn_blocking(move || -> Result<u64, StorageError> {
            let conn = conn.lock().expect("body store mutex poisoned");
            let n = conn
                .execute("DELETE FROM bodies WHERE tenant_id = ?", params![tenant])
                .map_err(|e| StorageError::Backend(format!("purge_tenant: {e}")))?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn store() -> Arc<SqliteBodyStore> {
        SqliteBodyStore::in_memory().expect("open in-memory store")
    }

    fn cref(tenant: &str, body: &[u8]) -> ContentRef {
        ContentRef::from_body(TenantId::new(tenant), body)
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
