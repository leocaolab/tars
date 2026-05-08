//! [`PipelineEventStore`] — durable stream of one event per
//! `Pipeline.call` boundary. See
//! [Doc 17](../../../docs/17-pipeline-event-store.md).
//!
//! Distinct from [`crate::EventStore`] (trajectory event log, keyed
//! by `TrajectoryId`). Different access patterns: this trait queries
//! by tenant + time range + tags; trajectory queries by id + sequence.
//! Q1 in Doc 17 explicitly chose two independent traits over a
//! generic `EventStore<E>`.
//!
//! Phase 1 ships `append` + a minimal `query` (filter by tenant +
//! time range). `subscribe()` (live consumers for OnlineEvaluatorRunner)
//! lands in Phase 2 with the W3 main body.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use rusqlite::{params, Connection};

use tars_types::{PipelineEvent, TenantId};

use crate::error::StorageError;

const SCHEMA_VERSION: i64 = 1;

/// Filter for `PipelineEventStore::query`. All fields are `AND`-ed
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
pub trait PipelineEventStore: Send + Sync + 'static {
    /// Append events. Each event carries its own `event_id`; storage
    /// preserves insertion order via `created_at` index. Idempotent
    /// on duplicate `event_id` (last write wins is fine — call sites
    /// don't re-emit, but a retried write should not ON CONFLICT
    /// fail).
    async fn append(&self, events: &[PipelineEvent]) -> Result<(), StorageError>;

    /// Query events. Returns up to 10_000 by default; pass `limit` to
    /// override. Order is `timestamp ASC, event_id ASC` for stability.
    async fn query(
        &self,
        q: &PipelineEventQuery,
    ) -> Result<Vec<PipelineEvent>, StorageError>;

    /// Drop events older than `cutoff`. Returns count removed.
    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StorageError>;

    /// Drop a tenant's entire event footprint. Required for tenant-
    /// delete compliance.
    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64, StorageError>;
}

#[derive(Clone, Debug)]
pub struct SqlitePipelineEventStoreConfig {
    pub path: PathBuf,
}

impl SqlitePipelineEventStoreConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone)]
pub struct SqlitePipelineEventStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqlitePipelineEventStore {
    pub fn open(
        config: SqlitePipelineEventStoreConfig,
    ) -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open(&config.path).map_err(|e| {
            StorageError::Backend(format!(
                "opening pipeline event store at {:?}: {e}",
                config.path
            ))
        })?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self { conn: Arc::new(Mutex::new(conn)) }))
    }

    pub fn in_memory() -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            StorageError::Backend(format!("opening in-memory pipeline event store: {e}"))
        })?;
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
                "incompatible pipeline event store schema (file v{current}, code v{SCHEMA_VERSION})"
            )));
        }
        // Schema notes:
        // - `event_id` is TEXT for UUID readability; PRIMARY KEY makes
        //   re-append idempotent (INSERT OR REPLACE).
        // - Inline columns are pulled out of payload_json so cohort
        //   queries (WHERE tenant + time range) don't have to parse
        //   JSON for every row.
        // - `tags_json` left as TEXT JSON; SQLite's json_each can
        //   filter on it. The full `payload_json` is the source of
        //   truth — inline columns are derived for query speed.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS pipeline_events (
                event_id        TEXT    NOT NULL PRIMARY KEY,
                event_type      TEXT    NOT NULL,
                timestamp_ms    INTEGER NOT NULL,
                tenant_id       TEXT    NOT NULL,
                payload_json    BLOB    NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_pe_tenant_ts
                ON pipeline_events(tenant_id, timestamp_ms);
            CREATE INDEX IF NOT EXISTS idx_pe_ts
                ON pipeline_events(timestamp_ms);
            "#,
        )
        .map_err(|e| StorageError::Backend(format!("create pipeline event schema: {e}")))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| StorageError::Backend(format!("set user_version: {e}")))?;
        Ok(())
    }
}

const DEFAULT_QUERY_LIMIT: u32 = 10_000;

fn ts_to_ms(t: SystemTime) -> i64 {
    use std::time::UNIX_EPOCH;
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Pull the columns needed for indexed query out of a `PipelineEvent`.
/// Returns `(event_id_str, event_type, timestamp_ms, tenant_id_str)`.
fn inline_columns(ev: &PipelineEvent) -> (String, &'static str, i64, String) {
    match ev {
        PipelineEvent::LlmCallFinished(e) => (
            e.event_id.to_string(),
            "llm_call_finished",
            ts_to_ms(e.timestamp),
            e.tenant_id.as_ref().to_string(),
        ),
        PipelineEvent::EvaluationScored(e) => (
            e.event_id.to_string(),
            "evaluation_scored",
            ts_to_ms(e.timestamp),
            e.tenant_id.as_ref().to_string(),
        ),
        // `Other` is a forward-compat catchall — caller code shouldn't
        // construct it, but if it shows up we still need to persist
        // *something*. Use a synthesized event_id so the PK stays
        // unique; tenant_id is unknown so use empty string.
        PipelineEvent::Other => (
            uuid::Uuid::new_v4().to_string(),
            "other",
            ts_to_ms(SystemTime::now()),
            String::new(),
        ),
        // `#[non_exhaustive]` — future variants we haven't added a
        // matcher for yet. Same treatment as `Other`.
        _ => (
            uuid::Uuid::new_v4().to_string(),
            "unknown",
            ts_to_ms(SystemTime::now()),
            String::new(),
        ),
    }
}

#[async_trait]
impl PipelineEventStore for SqlitePipelineEventStore {
    async fn append(&self, events: &[PipelineEvent]) -> Result<(), StorageError> {
        if events.is_empty() {
            return Ok(());
        }
        // Pre-encode outside the lock.
        let mut rows: Vec<(String, &'static str, i64, String, Vec<u8>)> =
            Vec::with_capacity(events.len());
        for ev in events {
            let (id, ty, ts, tenant) = inline_columns(ev);
            let blob = serde_json::to_vec(ev)?;
            rows.push((id, ty, ts, tenant, blob));
        }

        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let mut conn = conn.lock().expect("pipeline event store mutex poisoned");
            let tx = conn
                .transaction()
                .map_err(|e| StorageError::Backend(format!("begin tx: {e}")))?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT OR REPLACE INTO pipeline_events \
                         (event_id, event_type, timestamp_ms, tenant_id, payload_json) \
                         VALUES (?, ?, ?, ?, ?)",
                    )
                    .map_err(|e| StorageError::Backend(format!("prepare insert: {e}")))?;
                for (id, ty, ts, tenant, blob) in &rows {
                    stmt.execute(params![id, ty, ts, tenant, blob])
                        .map_err(|e| StorageError::Backend(format!("insert: {e}")))?;
                }
            }
            tx.commit()
                .map_err(|e| StorageError::Backend(format!("commit: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(())
    }

    async fn query(
        &self,
        q: &PipelineEventQuery,
    ) -> Result<Vec<PipelineEvent>, StorageError> {
        let conn = self.conn.clone();
        let tenant = q.tenant_id.as_ref().map(|t| t.as_ref().to_string());
        let since = q.since.map(ts_to_ms);
        let until = q.until.map(ts_to_ms);
        let limit = q.limit.unwrap_or(DEFAULT_QUERY_LIMIT) as i64;

        let blobs = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<u8>>, StorageError> {
            let conn = conn.lock().expect("pipeline event store mutex poisoned");

            // Build SQL incrementally — keep the where clause to
            // indexed columns only (tenant_id, timestamp_ms).
            let mut sql = String::from(
                "SELECT payload_json FROM pipeline_events WHERE 1=1",
            );
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

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StorageError::Backend(format!("prepare query: {e}")))?;

            // Build param list dynamically to match optional clauses.
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(t) = &tenant {
                params_vec.push(Box::new(t.clone()));
            }
            if let Some(s) = since {
                params_vec.push(Box::new(s));
            }
            if let Some(u) = until {
                params_vec.push(Box::new(u));
            }
            params_vec.push(Box::new(limit));

            let param_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();

            let iter = stmt
                .query_map(rusqlite::params_from_iter(param_refs), |r| {
                    r.get::<_, Vec<u8>>(0)
                })
                .map_err(|e| StorageError::Backend(format!("query: {e}")))?;

            let mut blobs = Vec::new();
            for row in iter {
                blobs.push(row.map_err(|e| StorageError::Backend(format!("row: {e}")))?);
            }
            Ok(blobs)
        })
        .await
        .map_err(|e| StorageError::Backend(format!("spawn_blocking: {e}")))??;

        let mut out = Vec::with_capacity(blobs.len());
        for b in blobs {
            out.push(serde_json::from_slice(&b)?);
        }
        Ok(out)
    }

    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64, StorageError> {
        let conn = self.conn.clone();
        let cutoff_ms = ts_to_ms(cutoff);
        let n = tokio::task::spawn_blocking(move || -> Result<u64, StorageError> {
            let conn = conn.lock().expect("pipeline event store mutex poisoned");
            let n = conn
                .execute(
                    "DELETE FROM pipeline_events WHERE timestamp_ms < ?",
                    params![cutoff_ms],
                )
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
            let conn = conn.lock().expect("pipeline event store mutex poisoned");
            let n = conn
                .execute(
                    "DELETE FROM pipeline_events WHERE tenant_id = ?",
                    params![tenant],
                )
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
    use tars_types::{
        CallResult, ContentRef, LlmCallFinished, ProviderId, TelemetryAccumulator, Usage,
        ValidationSummary,
    };
    use uuid::Uuid;

    async fn store() -> Arc<SqlitePipelineEventStore> {
        SqlitePipelineEventStore::in_memory().expect("open store")
    }

    fn fake_event(tenant: &str, ts: SystemTime) -> PipelineEvent {
        PipelineEvent::LlmCallFinished(Box::new(LlmCallFinished {
            event_id: Uuid::new_v4(),
            timestamp: ts,
            tenant_id: TenantId::new(tenant),
            session_id: None,
            trace_id: None,
            provider_id: ProviderId::new("p"),
            actual_model: "m".into(),
            request_fingerprint: [0u8; 32],
            request_ref: ContentRef::from_body(TenantId::new(tenant), b"req"),
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
        s.append(&[fake_event("a", SystemTime::now())]).await.unwrap();
        s.append(&[fake_event("b", SystemTime::now())]).await.unwrap();

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
        // earlier + now match, much_earlier dropped.
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn query_returns_in_timestamp_order() {
        let s = store().await;
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        // Insert out of order.
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
        s.append(&[fake_event("a", SystemTime::now())]).await.unwrap();
        s.append(&[fake_event("b", SystemTime::now())]).await.unwrap();

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
        // Second append of same event_id replaces (no PK violation).
        s.append(std::slice::from_ref(&ev)).await.unwrap();
        let got = s.query(&PipelineEventQuery::default()).await.unwrap();
        assert_eq!(got.len(), 1);
    }
}
