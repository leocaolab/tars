//! SQLite-backed [`AgentEventLog`] — Personal-mode persistence.
//!
//! Same scaffolding pattern as `tars_melt::event::SqliteLlmRecordStore`:
//! one `sqlx::SqlitePool` per DB file, WAL + `synchronous=NORMAL` +
//! `busy_timeout`, schema applied once at open through an embedded
//! `sqlx::migrate!` set (`_sqlx_migrations` is the version-of-record).
//!
//! Concurrency: `append` computes the next per-trajectory `sequence_no`
//! inside a `sqlx::Transaction` (SELECT COALESCE(MAX)+INSERT), so a
//! concurrent writer to the same trajectory can't race the seq. SQLite
//! WAL lets concurrent readers proceed while a writer holds its
//! transaction. Team mode (Postgres) gets per-row locking via
//! `SELECT ... FOR UPDATE`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sqlx::Row;
use crate::db::Db;

use tars_types::TrajectoryId;

use crate::error::StorageError;
use crate::agent_event_log::{EventRecord, AgentEventLog};

/// Embedded versioned schema (`migrations/agent_event_log/`). Applied once at
/// open on the store's own pool; `_sqlx_migrations` is the version-of-record
/// (replaces the old refinery `refinery_schema_history` / `user_version` gate).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/agent_event_log");

#[derive(Clone, Debug)]
pub struct SqliteAgentEventLogConfig {
    pub path: PathBuf,
}

impl SqliteAgentEventLogConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone)]
pub struct SqliteAgentEventLog {
    /// The database this store reads and writes. Cheap to clone.
    db: Db,
}

impl SqliteAgentEventLog {
    /// Construct on an **injected** [`Db`] — the composition root opens it once
    /// and hands it in; the store carries a handle, never a path. Runs this
    /// store's migrator on it.
    pub async fn new(db: Db) -> Result<Arc<Self>, StorageError> {
        db.migrate(&MIGRATOR)
            .await
            .map_err(|e| StorageError::backend_source("schema migration", e))?;
        Ok(Arc::new(Self { db }))
    }

    /// Thin convenience over the DI seam: open the file + [`new`](Self::new).
    /// Prefer `Db::open_sqlite(path)` + `new(db)` at the composition root.
    pub async fn open(config: SqliteAgentEventLogConfig) -> Result<Arc<Self>, StorageError> {
        let db = Db::open_sqlite(&config.path).await.map_err(|e| {
            StorageError::backend_source(format!("opening event store at {:?}", config.path), e)
        })?;
        Self::new(db).await
    }

    /// In-memory store for tests. Each call returns a fresh empty store; the
    /// database disappears with the handle.
    pub async fn in_memory() -> Result<Arc<Self>, StorageError> {
        let db = Db::sqlite_in_memory()
            .await
            .map_err(|e| StorageError::backend_source("opening in-memory event store", e))?;
        Self::new(db).await
    }
}

#[async_trait]
impl AgentEventLog for SqliteAgentEventLog {
    async fn append(
        &self,
        trajectory_id: &TrajectoryId,
        payloads: &[serde_json::Value],
    ) -> Result<u64, StorageError> {
        // Pre-encode to bytes before touching the DB.
        let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(payloads.len());
        for v in payloads {
            encoded.push(serde_json::to_vec(v)?);
        }
        if encoded.is_empty() {
            return self.high_water(trajectory_id).await;
        }

        let traj = trajectory_id.as_ref();
        let now = now_ms()?;

        // Compute the next sequence_no inside the transaction so a concurrent
        // writer to the same trajectory can't race us.
        let mut tx = self.db.sqlite().begin()
            .await
            .map_err(|e| StorageError::backend_source("begin transaction", e))?;

        let current_high: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence_no), 0) FROM events WHERE trajectory_id = ?",
        )
        .bind(traj)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| StorageError::backend_source("query max seq", e))?;

        let mut next_seq = (current_high as u64).saturating_add(1);
        for blob in &encoded {
            sqlx::query(
                "INSERT INTO events (trajectory_id, sequence_no, timestamp_ms, payload_json) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(traj)
            .bind(next_seq as i64)
            .bind(now)
            .bind(blob)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::backend_source("insert event", e))?;
            next_seq = next_seq.saturating_add(1);
        }

        tx.commit()
            .await
            .map_err(|e| StorageError::backend_source("commit", e))?;
        // `next_seq` is one past the last written.
        Ok(next_seq.saturating_sub(1))
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
        let rows = sqlx::query(
            "SELECT sequence_no, timestamp_ms, payload_json \
             FROM events \
             WHERE trajectory_id = ? AND sequence_no > ? \
             ORDER BY sequence_no ASC",
        )
        .bind(trajectory_id.as_ref())
        .bind(since as i64)
        .fetch_all(self.db.sqlite())
        .await
        .map_err(|e| StorageError::backend_source("query select", e))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.try_get("sequence_no").map_err(row_err)?;
            let ts: i64 = row.try_get("timestamp_ms").map_err(row_err)?;
            let blob: Vec<u8> = row.try_get("payload_json").map_err(row_err)?;
            let payload: serde_json::Value = serde_json::from_slice(&blob)?;
            out.push(EventRecord {
                trajectory_id: trajectory_id.clone(),
                sequence_no: seq as u64,
                timestamp_ms: ts,
                payload,
            });
        }
        Ok(out)
    }

    async fn high_water(&self, trajectory_id: &TrajectoryId) -> Result<u64, StorageError> {
        let max: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence_no), 0) FROM events WHERE trajectory_id = ?",
        )
        .bind(trajectory_id.as_ref())
        .fetch_one(self.db.sqlite())
        .await
        .map_err(|e| StorageError::backend_source("query high water", e))?;
        Ok(max as u64)
    }

    async fn list_trajectories(&self) -> Result<Vec<TrajectoryId>, StorageError> {
        let rows = sqlx::query("SELECT DISTINCT trajectory_id FROM events")
            .fetch_all(self.db.sqlite())
            .await
            .map_err(|e| StorageError::backend_source("query list", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let s: String = row.try_get("trajectory_id").map_err(row_err)?;
            out.push(TrajectoryId::new(s));
        }
        Ok(out)
    }
}

/// Default location: `$XDG_DATA_HOME/tars/events.sqlite` (or platform
/// equivalent). Personal-mode binaries (`tars-cli`, future `tars chat`)
/// land here unless overridden.
pub fn default_personal_agent_event_log_path() -> Option<PathBuf> {
    // data_dir is the XDG/macOS location for "long-lived user-state
    // files". cache_dir was right for the cache; events are NOT cache
    // (they're durable user history).
    dirs::data_dir().map(|d| d.join("tars").join("events.sqlite"))
}

/// Open at `path`, creating the parent directory if needed.
pub async fn open_agent_event_log_at_path(
    path: &Path,
) -> Result<Arc<SqliteAgentEventLog>, StorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            StorageError::backend_source(format!("create event store dir {parent:?}"), e)
        })?;
    }
    SqliteAgentEventLog::open(SqliteAgentEventLogConfig::new(path)).await
}

/// Decode failure on a fetched row → a `Backend` error preserving the source.
fn row_err(e: sqlx::Error) -> StorageError {
    StorageError::backend_source("decode row", e)
}

/// Current wall-clock time as milliseconds since the Unix epoch.
///
/// Returns `Err` if the system clock is set before `UNIX_EPOCH`: every
/// event would otherwise stamp `0`, silently corrupting the `timestamp_ms`
/// column that read-back ordering and retention rely on. The far-future
/// case is clamped to `i64::MAX` (year ~292 million — unreachable for a
/// real `now()`), which keeps the `as i64` cast from wrapping into a
/// negative timestamp; that clamp is documented and intentional.
fn now_ms() -> Result<i64, StorageError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .map_err(|e| StorageError::backend_source("system clock is before the Unix epoch", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    // Legacy-DB fixtures below build a pre-sqlx pool by hand to prove
    // non-destructive adoption; the store's own pool comes from `crate::pool`.

    fn traj(id: &str) -> TrajectoryId {
        TrajectoryId::new(id)
    }

    #[tokio::test]
    async fn append_then_read_all_round_trips() {
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
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
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
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
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
        let t = traj("t");
        // Empty append on empty trajectory.
        let r = store.append(&t, &[]).await.unwrap();
        assert_eq!(r, 0);
        // After real appends, empty append still reports current high.
        store
            .append(&t, &[json!({"x": 1}), json!({"x": 2})])
            .await
            .unwrap();
        let r = store.append(&t, &[]).await.unwrap();
        assert_eq!(r, 2);
    }

    #[tokio::test]
    async fn distinct_trajectories_are_isolated() {
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
        store
            .append(&traj("a"), &[json!({"k": "a1"})])
            .await
            .unwrap();
        store
            .append(&traj("b"), &[json!({"k": "b1"}), json!({"k": "b2"})])
            .await
            .unwrap();
        store
            .append(&traj("a"), &[json!({"k": "a2"})])
            .await
            .unwrap();

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
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
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
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
        assert_eq!(store.high_water(&traj("never_used")).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn read_all_returns_empty_for_unknown_trajectory() {
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
        assert!(
            store
                .read_all(&traj("never_used"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn list_trajectories_enumerates_distinct_ids() {
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
        store.append(&traj("a"), &[json!({})]).await.unwrap();
        store.append(&traj("b"), &[json!({})]).await.unwrap();
        store.append(&traj("a"), &[json!({})]).await.unwrap();
        let mut ids: Vec<String> = store
            .list_trajectories()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.into_inner())
            .collect();
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
            let store = open_agent_event_log_at_path(&path).await.unwrap();
            store
                .append(
                    &traj("crash_test"),
                    &[json!({"phase": "before"}), json!({"phase": "before-2"})],
                )
                .await
                .unwrap();
            // Drop store → pool closes → WAL flushes on next open.
        }
        let store = open_agent_event_log_at_path(&path).await.unwrap();
        let read = store.read_all(&traj("crash_test")).await.unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].payload, json!({"phase": "before"}));
        assert_eq!(read[1].payload, json!({"phase": "before-2"}));
    }

    #[tokio::test]
    async fn sqlx_history_stamps_baseline_on_fresh_db() {
        // E2E-1 (was `refinery_history_stamps_v1_on_fresh_db`): a fresh DB gets
        // `_sqlx_migrations` with the baseline (version 1) applied — sqlx is the
        // version-of-record now (the old `user_version=1` / refinery history is
        // gone). Same intent, new mechanism.
        let store = SqliteAgentEventLog::in_memory().await.unwrap();
        let v: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(store.db.sqlite())
            .await
            .unwrap();
        assert_eq!(v, 1, "baseline (0001_) stamped in _sqlx_migrations");
    }

    #[tokio::test]
    async fn adopts_pre_sqlx_db_preserving_rows() {
        // E2E-2 (the "好用" gate, was `adopts_pre_refinery_db_preserving_rows`):
        // open a DB created by the OLD inline-DDL code (tables present, a row,
        // NO `_sqlx_migrations`) and confirm baseline adoption is non-destructive
        // — the pre-existing row survives, sqlx stamps the baseline, and new
        // appends still work. `IF NOT EXISTS` makes the baseline a no-op.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        {
            let legacy = Db::open_sqlite(&path).await.unwrap();
            let pool = legacy.sqlite().clone();
            sqlx::raw_sql(
                "CREATE TABLE events (
                     trajectory_id TEXT    NOT NULL,
                     sequence_no   INTEGER NOT NULL,
                     timestamp_ms  INTEGER NOT NULL,
                     payload_json  BLOB    NOT NULL,
                     PRIMARY KEY (trajectory_id, sequence_no)
                 ) STRICT;
                 CREATE INDEX idx_events_trajectory ON events(trajectory_id);",
            )
            .execute(&pool)
            .await
            .unwrap();
            let payload = serde_json::to_vec(&json!({"old": 1})).unwrap();
            sqlx::query(
                "INSERT INTO events (trajectory_id, sequence_no, timestamp_ms, payload_json) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind("legacy")
            .bind(1_i64)
            .bind(123_i64)
            .bind(payload)
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Re-open through the migrating path.
        let store = open_agent_event_log_at_path(&path).await.unwrap();

        // The pre-existing row is intact (no re-create / no wipe).
        let read = store.read_all(&traj("legacy")).await.unwrap();
        assert_eq!(read.len(), 1, "pre-sqlx row survived adoption");
        assert_eq!(read[0].payload, json!({"old": 1}));

        // sqlx now owns the version-of-record.
        let v: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(store.db.sqlite())
            .await
            .unwrap();
        assert_eq!(v, 1, "baseline stamped in _sqlx_migrations");

        // And the adopted store is fully live.
        store.append(&traj("legacy"), &[json!({"new": 2})]).await.unwrap();
        assert_eq!(store.read_all(&traj("legacy")).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejects_db_with_unknown_applied_migration() {
        // E2E-3: fail-closed forward-version protection. A DB whose history
        // claims an applied migration this binary doesn't have (a newer writer
        // touched it) must refuse to open — never silently proceed. sqlx's
        // migrator returns `MigrateError::VersionMissing` for an applied
        // version absent from the embedded set (fail-closed, like refinery's
        // `abort_missing`).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        // Normal open creates `_sqlx_migrations` with the baseline (v1).
        drop(open_agent_event_log_at_path(&path).await.unwrap());
        // Forge a ghost v2 the embedded set doesn't contain.
        {
            let existing = Db::open_sqlite(&path).await.unwrap();
            let pool = existing.sqlite().clone();
            sqlx::query(
                "INSERT INTO _sqlx_migrations \
                 (version, description, success, checksum, execution_time) \
                 VALUES (2, 'ghost', 1, ?, 0)",
            )
            .bind(vec![0u8; 32])
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }
        match open_agent_event_log_at_path(&path).await {
            Err(_) => {} // sqlx VersionMissing — fail-closed, as required.
            Ok(_) => panic!("expected fail-closed on unknown applied migration, got Ok"),
        }
    }
}
