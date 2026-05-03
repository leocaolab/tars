//! SQLite-backed [`EventStore`] — Personal-mode persistence.
//!
//! Same scaffolding pattern as `tars-cache::SqliteCacheRegistry`:
//! single connection in `Arc<Mutex>`, blocking calls inside
//! `tokio::task::spawn_blocking`, WAL + `synchronous=NORMAL` +
//! `temp_store=MEMORY` pragmas, schema version pinned via SQLite's
//! `user_version` PRAGMA. A future helper extraction makes sense
//! when there's a 3rd SQLite-backed crate; today the duplication is
//! 100 lines of pure plumbing and the abstraction would be its own
//! design problem.
//!
//! Concurrency: all writes serialise on the single connection's
//! mutex so per-trajectory `sequence_no` stays gap-free. SQLite WAL
//! lets concurrent **readers** proceed even while a writer holds the
//! mutex inside `spawn_blocking`. For Personal-mode workloads this
//! is more than enough; Team mode (Postgres) gets per-row locking
//! via `SELECT ... FOR UPDATE`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusqlite::{params, Connection};

use tars_types::TrajectoryId;

use crate::error::StorageError;
use crate::event_store::{EventRecord, EventStore};

const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Debug)]
pub struct SqliteEventStoreConfig {
    pub path: PathBuf,
}

impl SqliteEventStoreConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone)]
pub struct SqliteEventStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteEventStore {
    pub fn open(config: SqliteEventStoreConfig) -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open(&config.path).map_err(|e| {
            StorageError::Backend(format!("opening event store at {:?}: {e}", config.path))
        })?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self { conn: Arc::new(Mutex::new(conn)) }))
    }

    /// In-memory store for tests. Each call returns a fresh empty
    /// store; the database disappears with the connection.
    pub fn in_memory() -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StorageError::Backend(format!("opening in-memory event store: {e}")))?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self { conn: Arc::new(Mutex::new(conn)) }))
    }

    fn pragma_setup(conn: &Connection) -> Result<(), StorageError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| StorageError::Backend(format!("pragma journal_mode: {e}")))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| StorageError::Backend(format!("pragma synchronous: {e}")))?;
        conn.pragma_update(None, "temp_store", "MEMORY")
            .map_err(|e| StorageError::Backend(format!("pragma temp_store: {e}")))?;
        // foreign_keys is off by default in SQLite; we don't have FKs in
        // the M3 schema but turning it on is the right hygiene for
        // future schema additions (trajectory parent links, etc.).
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| StorageError::Backend(format!("pragma foreign_keys: {e}")))?;
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
            // Unknown prior schema — events are durable user data, so
            // unlike the cache we DO NOT wipe. Surface the version
            // mismatch and let the operator decide.
            return Err(StorageError::Backend(format!(
                "incompatible event store schema (file version {current}, code version {SCHEMA_VERSION}). \
                 Refusing to migrate automatically — back up the file and run a manual migration.",
            )));
        }

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                trajectory_id   TEXT    NOT NULL,
                sequence_no     INTEGER NOT NULL,
                timestamp_ms    INTEGER NOT NULL,
                payload_json    BLOB    NOT NULL,
                PRIMARY KEY (trajectory_id, sequence_no)
            ) STRICT;

            -- The PK already covers (trajectory_id, sequence_no) lookups;
            -- a separate index for "list trajectories" is cheaper than a
            -- DISTINCT scan over the PK.
            CREATE INDEX IF NOT EXISTS idx_events_trajectory
                ON events(trajectory_id);
            "#,
        )
        .map_err(|e| StorageError::Backend(format!("create schema: {e}")))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| StorageError::Backend(format!("set user_version: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn append(
        &self,
        trajectory_id: &TrajectoryId,
        payloads: &[serde_json::Value],
    ) -> Result<u64, StorageError> {
        // Pre-encode to bytes outside the lock so the spawn_blocking
        // body holds the connection for as little time as possible.
        let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(payloads.len());
        for v in payloads {
            encoded.push(serde_json::to_vec(v)?);
        }
        if encoded.is_empty() {
            return self.high_water(trajectory_id).await;
        }

        let conn = self.conn.clone();
        let traj = trajectory_id.clone();
        let now = now_ms();

        let last_seq = tokio::task::spawn_blocking(move || -> Result<u64, StorageError> {
            let mut conn = conn.lock().expect("event store mutex poisoned");
            let tx = conn
                .transaction()
                .map_err(|e| StorageError::Backend(format!("begin transaction: {e}")))?;

            // Compute the next sequence_no inside the transaction so a
            // concurrent writer to the same trajectory can't race us.
            let current_high: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(sequence_no), 0) FROM events WHERE trajectory_id = ?",
                    params![traj.as_ref()],
                    |r| r.get(0),
                )
                .map_err(|e| StorageError::Backend(format!("query max seq: {e}")))?;
            let mut next_seq = (current_high as u64).saturating_add(1);

            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO events (trajectory_id, sequence_no, timestamp_ms, payload_json) \
                         VALUES (?, ?, ?, ?)",
                    )
                    .map_err(|e| StorageError::Backend(format!("prepare insert: {e}")))?;
                for blob in &encoded {
                    stmt.execute(params![traj.as_ref(), next_seq as i64, now, blob])
                        .map_err(|e| StorageError::Backend(format!("insert event: {e}")))?;
                    next_seq = next_seq.saturating_add(1);
                }
            }

            tx.commit()
                .map_err(|e| StorageError::Backend(format!("commit: {e}")))?;
            // `next_seq` is one past the last written.
            Ok(next_seq.saturating_sub(1))
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(last_seq)
    }

    async fn read_all(
        &self,
        trajectory_id: &TrajectoryId,
    ) -> Result<Vec<EventRecord>, StorageError> {
        self.read_since(trajectory_id, 0).await
    }

    async fn read_since(
        &self,
        trajectory_id: &TrajectoryId,
        since: u64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        let conn = self.conn.clone();
        let traj = trajectory_id.clone();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<EventRecord>, StorageError> {
            let conn = conn.lock().expect("event store mutex poisoned");
            let mut stmt = conn
                .prepare(
                    "SELECT sequence_no, timestamp_ms, payload_json \
                     FROM events \
                     WHERE trajectory_id = ? AND sequence_no > ? \
                     ORDER BY sequence_no ASC",
                )
                .map_err(|e| StorageError::Backend(format!("prepare select: {e}")))?;
            let iter = stmt
                .query_map(params![traj.as_ref(), since as i64], |r| {
                    let seq: i64 = r.get(0)?;
                    let ts: i64 = r.get(1)?;
                    let blob: Vec<u8> = r.get(2)?;
                    Ok((seq as u64, ts, blob))
                })
                .map_err(|e| StorageError::Backend(format!("query select: {e}")))?;

            let mut out = Vec::new();
            for row in iter {
                let (seq, ts, blob) =
                    row.map_err(|e| StorageError::Backend(format!("row: {e}")))?;
                let payload: serde_json::Value = serde_json::from_slice(&blob)?;
                out.push(EventRecord {
                    trajectory_id: traj.clone(),
                    sequence_no: seq,
                    timestamp_ms: ts,
                    payload,
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(rows)
    }

    async fn high_water(
        &self,
        trajectory_id: &TrajectoryId,
    ) -> Result<u64, StorageError> {
        let conn = self.conn.clone();
        let traj = trajectory_id.clone();
        let high = tokio::task::spawn_blocking(move || -> Result<u64, StorageError> {
            let conn = conn.lock().expect("event store mutex poisoned");
            let max: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(sequence_no), 0) FROM events WHERE trajectory_id = ?",
                    params![traj.as_ref()],
                    |r| r.get(0),
                )
                .map_err(|e| StorageError::Backend(format!("query high water: {e}")))?;
            Ok(max as u64)
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;
        Ok(high)
    }

    async fn list_trajectories(&self) -> Result<Vec<TrajectoryId>, StorageError> {
        let conn = self.conn.clone();
        let ids = tokio::task::spawn_blocking(move || -> Result<Vec<TrajectoryId>, StorageError> {
            let conn = conn.lock().expect("event store mutex poisoned");
            let mut stmt = conn
                .prepare("SELECT DISTINCT trajectory_id FROM events")
                .map_err(|e| StorageError::Backend(format!("prepare list: {e}")))?;
            let iter = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| StorageError::Backend(format!("query list: {e}")))?;
            let mut out = Vec::new();
            for row in iter {
                let s = row.map_err(|e| StorageError::Backend(format!("row: {e}")))?;
                out.push(TrajectoryId::new(s));
            }
            Ok(out)
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;
        Ok(ids)
    }
}

/// Default location: `$XDG_DATA_HOME/tars/events.sqlite` (or platform
/// equivalent). Personal-mode binaries (`tars-cli`, future `tars chat`)
/// land here unless overridden.
pub fn default_personal_event_store_path() -> Option<PathBuf> {
    // data_dir is the XDG/macOS location for "long-lived user-state
    // files". cache_dir was right for the cache; events are NOT cache
    // (they're durable user history).
    dirs::data_dir().map(|d| d.join("tars").join("events.sqlite"))
}

/// Open at `path`, creating the parent directory if needed.
pub fn open_event_store_at_path(path: &Path) -> Result<Arc<SqliteEventStore>, StorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            StorageError::Backend(format!("create event store dir {parent:?}: {e}"))
        })?;
    }
    SqliteEventStore::open(SqliteEventStoreConfig::new(path))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn traj(id: &str) -> TrajectoryId {
        TrajectoryId::new(id)
    }

    #[tokio::test]
    async fn append_then_read_all_round_trips() {
        let store = SqliteEventStore::in_memory().unwrap();
        let t = traj("t1");
        let payloads = vec![
            json!({"kind": "start", "task": "summarise"}),
            json!({"kind": "delta", "text": "Hello "}),
            json!({"kind": "delta", "text": "world"}),
            json!({"kind": "finish", "tokens": 12}),
        ];
        let last = store.append(&t, &payloads).await.unwrap();
        assert_eq!(last, 4, "1-indexed; 4 events → last seq=4");

        let read = store.read_all(&t).await.unwrap();
        assert_eq!(read.len(), 4);
        assert_eq!(read[0].sequence_no, 1);
        assert_eq!(read[3].sequence_no, 4);
        assert_eq!(read[0].payload, payloads[0]);
        assert_eq!(read[3].payload, payloads[3]);
    }

    #[tokio::test]
    async fn append_increments_across_calls() {
        let store = SqliteEventStore::in_memory().unwrap();
        let t = traj("t");
        store.append(&t, &[json!({"a": 1})]).await.unwrap();
        store.append(&t, &[json!({"a": 2})]).await.unwrap();
        let last = store.append(&t, &[json!({"a": 3})]).await.unwrap();
        assert_eq!(last, 3);
        let high = store.high_water(&t).await.unwrap();
        assert_eq!(high, 3);
    }

    #[tokio::test]
    async fn empty_payloads_is_no_op_returning_high_water() {
        let store = SqliteEventStore::in_memory().unwrap();
        let t = traj("t");
        // Empty append on empty trajectory.
        let r = store.append(&t, &[]).await.unwrap();
        assert_eq!(r, 0);
        // After real appends, empty append still reports current high.
        store.append(&t, &[json!({"x": 1}), json!({"x": 2})]).await.unwrap();
        let r = store.append(&t, &[]).await.unwrap();
        assert_eq!(r, 2);
    }

    #[tokio::test]
    async fn distinct_trajectories_are_isolated() {
        let store = SqliteEventStore::in_memory().unwrap();
        store.append(&traj("a"), &[json!({"k": "a1"})]).await.unwrap();
        store.append(&traj("b"), &[json!({"k": "b1"}), json!({"k": "b2"})]).await.unwrap();
        store.append(&traj("a"), &[json!({"k": "a2"})]).await.unwrap();

        let a = store.read_all(&traj("a")).await.unwrap();
        let b = store.read_all(&traj("b")).await.unwrap();
        // Each trajectory's seq_no starts at 1 independently.
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].sequence_no, 1);
        assert_eq!(a[1].sequence_no, 2);
        assert_eq!(a[0].payload, json!({"k": "a1"}));
        assert_eq!(a[1].payload, json!({"k": "a2"}));
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].sequence_no, 1);
        assert_eq!(b[1].sequence_no, 2);
    }

    #[tokio::test]
    async fn read_since_filters_by_sequence_no() {
        let store = SqliteEventStore::in_memory().unwrap();
        let t = traj("t");
        for i in 1..=5 {
            store.append(&t, &[json!({"i": i})]).await.unwrap();
        }
        let r = store.read_since(&t, 2).await.unwrap();
        assert_eq!(r.len(), 3, "events at seq 3,4,5");
        assert_eq!(r[0].sequence_no, 3);
        assert_eq!(r[2].sequence_no, 5);
    }

    #[tokio::test]
    async fn high_water_returns_zero_for_unknown_trajectory() {
        let store = SqliteEventStore::in_memory().unwrap();
        assert_eq!(store.high_water(&traj("never_used")).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn read_all_returns_empty_for_unknown_trajectory() {
        let store = SqliteEventStore::in_memory().unwrap();
        assert!(store.read_all(&traj("never_used")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_trajectories_enumerates_distinct_ids() {
        let store = SqliteEventStore::in_memory().unwrap();
        store.append(&traj("a"), &[json!({})]).await.unwrap();
        store.append(&traj("b"), &[json!({})]).await.unwrap();
        store.append(&traj("a"), &[json!({})]).await.unwrap();
        let mut ids: Vec<String> =
            store.list_trajectories().await.unwrap().into_iter().map(|t| t.into_inner()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn append_survives_close_and_reopen() {
        // Doc 04 §3 recovery-from-checkpoint guarantee: events written
        // before a crash must be readable after restart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        {
            let store = open_event_store_at_path(&path).unwrap();
            store
                .append(
                    &traj("crash_test"),
                    &[json!({"phase": "before"}), json!({"phase": "before-2"})],
                )
                .await
                .unwrap();
            // Drop store → connection closes → WAL flushes on next open.
        }
        let store = open_event_store_at_path(&path).unwrap();
        let read = store.read_all(&traj("crash_test")).await.unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].payload, json!({"phase": "before"}));
        assert_eq!(read[1].payload, json!({"phase": "before-2"}));
    }

    #[tokio::test]
    async fn schema_version_marker_is_set_on_fresh_db() {
        let store = SqliteEventStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn reopen_with_unknown_schema_version_errors_does_not_wipe() {
        // Events are durable user data — unlike the cache, an unknown
        // schema version refuses to migrate rather than silently
        // dropping rows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        // Open at version 1 + populate.
        {
            let store = open_event_store_at_path(&path).unwrap();
            store
                .append(&traj("durable"), &[json!({"x": 1})])
                .await
                .unwrap();
        }
        // Forge a future schema version so the next open thinks we're behind.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 999_i64).unwrap();
        }
        // Reopen should error.
        let result = open_event_store_at_path(&path);
        match result {
            Err(StorageError::Backend(msg)) => {
                assert!(
                    msg.contains("incompatible") && msg.contains("999"),
                    "error must call out the version mismatch: {msg}",
                );
            }
            Err(other) => panic!("expected Backend error, got {other:?}"),
            // SqliteEventStore isn't Debug; can't print on success.
            Ok(_) => panic!("expected migration error, got Ok"),
        }
    }
}
