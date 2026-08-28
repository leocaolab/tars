//! [`PipelineEventLog`] — durable stream of one event per
//! `Pipeline.call` boundary.
//!
//! Distinct from recovery's `AgentEventLog` (tars-storage, the
//! trajectory event log, keyed by `TrajectoryId`). Different access
//! patterns: this trait queries
//! by tenant + time range + tags; trajectory queries by id + sequence.
//! This justifies two independent traits over a generic `EventStore<E>`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use sqlx::Row;
use tars_storage::Db;

use tars_types::TenantId;

use super::{PipelineEvent, StoreError};

/// Embedded versioned schema (`migrations/pipeline_event_log/`). Applied
/// once at open on the store's own pool; `_sqlx_migrations` is the
/// version-of-record.
///
/// Two migrations, mirroring the old hand-rolled `PRAGMA user_version`
/// schema (SCHEMA_VERSION was 2):
/// - `0001_baseline.sql` — the v1 `pipeline_events` table.
/// - `0002_unresolved_to_null.sql` — the v1→v2 data rewrite
///   (`ARC-L5-SW-10`): `LlmCallFinished.provider_id` moved from a
///   `"unresolved"` sentinel string to `Option<ProviderId>`, so any
///   payload carrying `provider_id: "unresolved"` is rewritten to
///   `provider_id: null`. Idempotent.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/pipeline_event_log");

/// Filter for `PipelineEventLog::query`. All fields are `AND`-ed
/// together; `None` means "don't filter on this dimension."
#[derive(Clone, Debug, Default)]
pub struct PipelineEventQuery {
    pub tenant_id: Option<TenantId>,
    /// Earliest event timestamp to include (`>=`). `None` = no lower
    /// bound. Compared against the event's `timestamp` field.
    pub since: Option<SystemTime>,
    /// Latest event timestamp to include (`<`). `None` = no upper
    /// bound.
    pub until: Option<SystemTime>,
    /// Hard cap on returned rows. Default impl returns at most 10_000
    /// even when `None` to protect against accidental full scans.
    pub limit: Option<u32>,
}

#[async_trait]
pub trait PipelineEventLog: Send + Sync + 'static {
    /// Append events. Each event carries its own `event_id`; storage
    /// preserves insertion order via `created_at` index. Idempotent
    /// on duplicate `event_id` (last write wins is fine — call sites
    /// don't re-emit, but a retried write should not ON CONFLICT
    /// fail).
    async fn append(&self, events: &[PipelineEvent]) -> Result<(), StoreError>;

    /// Query events. Returns up to 10_000 by default; pass `limit` to
    /// override. Order is `timestamp ASC, event_id ASC` for stability.
    async fn query(&self, q: &PipelineEventQuery) -> Result<Vec<PipelineEvent>, StoreError>;

    /// Drop events older than `cutoff`. Returns count removed.
    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StoreError>;

    /// Drop a tenant's entire event footprint. Required for tenant-
    /// delete compliance.
    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StoreError>;
}

#[derive(Clone, Debug)]
pub struct SqlitePipelineEventLogConfig {
    pub path: PathBuf,
}

impl SqlitePipelineEventLogConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone)]
pub struct SqlitePipelineEventLog {
    /// The one pool for this store's DB file. Cheap to clone (Arc inside sqlx).
    db: Db,
}

impl SqlitePipelineEventLog {
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
    pub async fn open(config: SqlitePipelineEventLogConfig) -> Result<Arc<Self>, StoreError> {
        let db = Db::open_sqlite(&config.path).await.map_err(|e| {
            StoreError::backend_source(
                format!("opening pipeline event store at {:?}", config.path),
                e,
            )
        })?;
        Self::new(db).await
    }

    pub async fn in_memory() -> Result<Arc<Self>, StoreError> {
        let db = Db::sqlite_in_memory()
            .await
            .map_err(|e| StoreError::backend_source("opening in-memory pipeline event store", e))?;
        Self::new(db).await
    }
}

const DEFAULT_QUERY_LIMIT: u32 = 10_000;

/// Convert a `SystemTime` to milliseconds since the Unix epoch.
///
/// Uses signed math so pre-epoch times map to negative values (rather
/// than collapsing to `0` and colliding with a real epoch-instant
/// event), and clamps the far-future case so `as i64` never wraps:
/// `duration_since` saturates at `u128`, and `i64::MAX` ms is the year
/// ~292 million, well beyond any real event.
fn ts_to_ms(t: SystemTime) -> i64 {
    use std::time::UNIX_EPOCH;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        // Pre-epoch: `t` is before UNIX_EPOCH, so `err` carries the
        // magnitude of the gap. Negate (clamped) to a signed offset.
        Err(err) => {
            let behind = err.duration().as_millis().min(i64::MAX as u128) as i64;
            -behind
        }
    }
}

/// Pull the columns needed for indexed query out of a `PipelineEvent`.
/// Returns `(event_id_str, event_type, timestamp_ms, tenant_id_str)`.
///
/// Returns `Err` for variants that carry no stable identity. The
/// `append` contract promises idempotency on `event_id`; a `PipelineEvent::Other`
/// has no `event_id` or `timestamp`, so persisting it would require a
/// random UUID + `now()`, which silently violates idempotency (a
/// retried write would land a *second* row). Reject instead so the
/// caller learns it emitted something this store can't durably key.
fn inline_columns(ev: &PipelineEvent) -> Result<(String, &'static str, i64, String), StoreError> {
    match ev {
        PipelineEvent::LlmCallFinished(e) => Ok((
            e.event_id.to_string(),
            "llm_call_finished",
            ts_to_ms(e.timestamp),
            e.tenant_id.as_ref().to_string(),
        )),
        PipelineEvent::EvaluationScored(e) => Ok((
            e.event_id.to_string(),
            "evaluation_scored",
            ts_to_ms(e.timestamp),
            e.tenant_id.as_ref().to_string(),
        )),
        PipelineEvent::Other => Err(StoreError::backend(
            "cannot append PipelineEvent::Other: it has no event_id/timestamp, so it \
             cannot satisfy the idempotency-on-event_id contract",
        )),
    }
}

#[async_trait]
impl PipelineEventLog for SqlitePipelineEventLog {
    async fn append(&self, events: &[PipelineEvent]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        // Pre-encode the rows up front so a serde failure aborts before we
        // touch the DB.
        let mut rows: Vec<(String, &'static str, i64, String, Vec<u8>)> =
            Vec::with_capacity(events.len());
        for ev in events {
            let (id, ty, ts, tenant) = inline_columns(ev)?;
            let blob = serde_json::to_vec(ev)?;
            rows.push((id, ty, ts, tenant, blob));
        }

        // One transaction for the batch: either all events land or none do.
        let mut tx = self
            .db
            .sqlite()
            .begin()
            .await
            .map_err(|e| StoreError::backend_source("begin tx", e))?;
        for (id, ty, ts, tenant, blob) in &rows {
            sqlx::query(
                "INSERT OR REPLACE INTO pipeline_events \
                 (event_id, event_type, timestamp_ms, tenant_id, payload_json) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(*ty)
            .bind(ts)
            .bind(tenant)
            .bind(blob)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::backend_source("insert", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| StoreError::backend_source("commit", e))?;

        Ok(())
    }

    async fn query(&self, q: &PipelineEventQuery) -> Result<Vec<PipelineEvent>, StoreError> {
        let tenant = q.tenant_id.as_ref().map(|t| t.as_ref().to_string());
        let since = q.since.map(ts_to_ms);
        let until = q.until.map(ts_to_ms);
        let limit = q.limit.unwrap_or(DEFAULT_QUERY_LIMIT) as i64;

        // Build SQL incrementally — keep the where clause to indexed
        // columns only (tenant_id, timestamp_ms).
        let mut sql = String::from("SELECT payload_json FROM pipeline_events WHERE 1=1");
        if tenant.is_some() {
            sql.push_str(" AND tenant_id = ?");
        }
        if since.is_some() {
            sql.push_str(" AND timestamp_ms >= ?");
        }
        if until.is_some() {
            sql.push_str(" AND timestamp_ms < ?");
        }
        sql.push_str(" ORDER BY timestamp_ms ASC, event_id ASC LIMIT ?");

        // Bind in the same order the clauses were appended.
        let mut query = sqlx::query(&sql);
        if let Some(t) = &tenant {
            query = query.bind(t);
        }
        if let Some(s) = since {
            query = query.bind(s);
        }
        if let Some(u) = until {
            query = query.bind(u);
        }
        query = query.bind(limit);

        let rows = query
            .fetch_all(self.db.sqlite())
            .await
            .map_err(|e| StoreError::backend_source("query", e))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let blob: Vec<u8> = row
                .try_get("payload_json")
                .map_err(|e| StoreError::backend_source("read payload_json", e))?;
            out.push(serde_json::from_slice(&blob)?);
        }
        Ok(out)
    }

    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StoreError> {
        let cutoff_ms = ts_to_ms(cutoff);
        let res = sqlx::query("DELETE FROM pipeline_events WHERE timestamp_ms < ?")
            .bind(cutoff_ms)
            .execute(self.db.sqlite())
            .await
            .map_err(|e| StoreError::backend_source("purge_before", e))?;
        Ok(res.rows_affected())
    }

    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StoreError> {
        let tenant = tenant_id.as_ref().to_string();
        let res = sqlx::query("DELETE FROM pipeline_events WHERE tenant_id = ?")
            .bind(tenant)
            .execute(self.db.sqlite())
            .await
            .map_err(|e| StoreError::backend_source("purge_tenant", e))?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CallResult, ContentRef, LlmCallFinished};
    use super::*;
    use std::time::Duration;
    use tars_types::{ProviderId, TelemetryAccumulator, Usage, ValidationSummary};
    use uuid::Uuid;

    async fn store() -> Arc<SqlitePipelineEventLog> {
        SqlitePipelineEventLog::in_memory()
            .await
            .expect("open store")
    }

    fn fake_event(tenant: &str, ts: SystemTime) -> PipelineEvent {
        PipelineEvent::LlmCallFinished(Box::new(LlmCallFinished {
            event_id: Uuid::new_v4(),
            timestamp: ts,
            tenant_id: TenantId::new(tenant),
            session_id: None,
            trace_id: None,
            provider_id: Some(ProviderId::new("p")),
            actual_model: "m".into(),
            request_fingerprint: [0u8; 32],
            request_ref: ContentRef::from_content(TenantId::new(tenant), b"req"),
            has_tools: false,
            has_thinking: false,
            has_structured_output: false,
            temperature: Some(0.0),
            max_output_tokens: None,
            response_ref: None,
            usage: Usage::default(),
            stop_reason: None,
            telemetry: TelemetryAccumulator::default(),
            validation_summary: ValidationSummary::default(),
            validation_reason: None,
            result: CallResult::Ok,
            tags: vec!["dogfood".into()],
        }))
    }

    #[tokio::test]
    async fn append_then_query_round_trips() {
        let s = store().await;
        let ev = fake_event("t1", SystemTime::now());
        s.append(std::slice::from_ref(&ev)).await.unwrap();

        let got = s
            .query(&PipelineEventQuery {
                tenant_id: Some(TenantId::new("t1")),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        match &got[0] {
            PipelineEvent::LlmCallFinished(e) => assert_eq!(e.tenant_id.as_ref(), "t1"),
            _ => panic!("expected LlmCallFinished"),
        }
    }

    #[tokio::test]
    async fn query_filters_by_tenant() {
        let s = store().await;
        s.append(&[fake_event("a", SystemTime::now())])
            .await
            .unwrap();
        s.append(&[fake_event("b", SystemTime::now())])
            .await
            .unwrap();

        let got = s
            .query(&PipelineEventQuery {
                tenant_id: Some(TenantId::new("a")),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[tokio::test]
    async fn query_filters_by_time_range() {
        let s = store().await;
        let now = SystemTime::now();
        let earlier = now - Duration::from_secs(60);
        let much_earlier = now - Duration::from_secs(3600);

        s.append(&[fake_event("t1", much_earlier)]).await.unwrap();
        s.append(&[fake_event("t1", earlier)]).await.unwrap();
        s.append(&[fake_event("t1", now)]).await.unwrap();

        let got = s
            .query(&PipelineEventQuery {
                since: Some(earlier - Duration::from_secs(1)),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn query_returns_in_timestamp_order() {
        let s = store().await;
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        s.append(&[fake_event("t", t2)]).await.unwrap();
        s.append(&[fake_event("t", t0)]).await.unwrap();
        s.append(&[fake_event("t", t1)]).await.unwrap();

        let got = s.query(&PipelineEventQuery::default()).await.unwrap();
        assert_eq!(got.len(), 3);
        let timestamps: Vec<_> = got
            .iter()
            .map(|e| match e {
                PipelineEvent::LlmCallFinished(x) => x.timestamp,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(timestamps, vec![t0, t1, t2]);
    }

    #[tokio::test]
    async fn purge_tenant_drops_only_that_tenant() {
        let s = store().await;
        s.append(&[fake_event("a", SystemTime::now())])
            .await
            .unwrap();
        s.append(&[fake_event("b", SystemTime::now())])
            .await
            .unwrap();

        let n = s.purge_tenant(&TenantId::new("a")).await.unwrap();
        assert_eq!(n, 1);

        let remaining = s.query(&PipelineEventQuery::default()).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn append_idempotent_on_same_event_id() {
        let s = store().await;
        let ev = fake_event("t1", SystemTime::now());
        s.append(std::slice::from_ref(&ev)).await.unwrap();
        s.append(std::slice::from_ref(&ev)).await.unwrap();
        let got = s.query(&PipelineEventQuery::default()).await.unwrap();
        assert_eq!(got.len(), 1);
    }

    /// Pin the v1→v2 migration (`0002_unresolved_to_null.sql`,
    /// ARC-L5-SW-10): adopting a *pre-existing v1 DB* — the
    /// `pipeline_events` table already populated but with no
    /// `_sqlx_migrations` tracking — must, on first `MIGRATOR.run`,
    /// rewrite an `LlmCallFinished` payload carrying
    /// `provider_id: "unresolved"` (the legacy sentinel) into
    /// `provider_id: null`, leave already-resolved and already-null
    /// rows untouched, and keep the row count unchanged. Idempotent —
    /// running the migrator again is a no-op.
    #[tokio::test]
    async fn migrate_v1_to_v2_rewrites_unresolved_sentinel_to_null() {
        // Hand-build a v1 store: an in-memory database with the v1 schema
        // created and legacy rows inserted *before* the migrator has ever run
        // (so `_sqlx_migrations` does not yet exist — exactly the "adopt an old
        // pre-sqlx DB" path).
        let db = Db::sqlite_in_memory().await.unwrap();

        sqlx::query(
            "CREATE TABLE pipeline_events (
                event_id        TEXT    NOT NULL PRIMARY KEY,
                event_type      TEXT    NOT NULL,
                timestamp_ms    INTEGER NOT NULL,
                tenant_id       TEXT    NOT NULL,
                payload_json    BLOB    NOT NULL
            ) STRICT;",
        )
        .execute(db.sqlite())
        .await
        .unwrap();

        let legacy_payload = serde_json::json!({
            "LlmCallFinished": {
                "event_id": "00000000-0000-0000-0000-000000000001",
                "tenant_id": "t1",
                "provider_id": "unresolved",
                "actual_model": "gpt-4o",
            }
        });
        let already_resolved_payload = serde_json::json!({
            "LlmCallFinished": {
                "event_id": "00000000-0000-0000-0000-000000000002",
                "tenant_id": "t1",
                "provider_id": "openai-1",
                "actual_model": "gpt-4o",
            }
        });
        let already_null_payload = serde_json::json!({
            "LlmCallFinished": {
                "event_id": "00000000-0000-0000-0000-000000000003",
                "tenant_id": "t1",
                "provider_id": null,
                "actual_model": "gpt-4o",
            }
        });
        for (id, payload) in [
            ("00000000-0000-0000-0000-000000000001", &legacy_payload),
            (
                "00000000-0000-0000-0000-000000000002",
                &already_resolved_payload,
            ),
            (
                "00000000-0000-0000-0000-000000000003",
                &already_null_payload,
            ),
        ] {
            sqlx::query(
                "INSERT INTO pipeline_events (event_id, event_type, timestamp_ms, \
                 tenant_id, payload_json) VALUES (?, 'llm_call_finished', 0, 't1', ?)",
            )
            .bind(id)
            .bind(serde_json::to_vec(payload).unwrap())
            .execute(db.sqlite())
            .await
            .unwrap();
        }

        // Run the migrator — 0001 is a no-op (`CREATE TABLE IF NOT
        // EXISTS`), 0002 performs the data rewrite.
        db.migrate(&MIGRATOR).await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pipeline_events")
            .fetch_one(db.sqlite())
            .await
            .unwrap();
        assert_eq!(n, 3);

        let payload_for = |id: &'static str| {
            let db = db.clone();
            async move {
                let blob: Vec<u8> = sqlx::query_scalar(
                    "SELECT payload_json FROM pipeline_events WHERE event_id = ?",
                )
                .bind(id)
                .fetch_one(db.sqlite())
                .await
                .unwrap();
                serde_json::from_slice::<serde_json::Value>(&blob).unwrap()
            }
        };

        let v = payload_for("00000000-0000-0000-0000-000000000001").await;
        assert!(
            v["LlmCallFinished"]["provider_id"].is_null(),
            "legacy sentinel rewritten to null: {v}"
        );

        let v = payload_for("00000000-0000-0000-0000-000000000002").await;
        assert_eq!(v["LlmCallFinished"]["provider_id"], "openai-1");

        // The already-null row is also untouched (idempotent on the new
        // shape — no double-rewrite, no spurious row write).
        let v = payload_for("00000000-0000-0000-0000-000000000003").await;
        assert!(v["LlmCallFinished"]["provider_id"].is_null());

        // Running the migrator again is a no-op (0002 already applied).
        db.migrate(&MIGRATOR).await.unwrap();
    }
}
