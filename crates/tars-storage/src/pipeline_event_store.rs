//! [`PipelineEventStore`] — durable stream of one event per
//! `Pipeline.call` boundary. See
//! [Doc 17](../../../docs/architecture/17-pipeline-event-store.md).
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
use rusqlite::{Connection, params};

use tars_types::{PipelineEvent, TenantId};

use crate::error::StorageError;

/// Pipeline event store schema version.
///
/// **v1 → v2** (`ARC-L5-SW-10`): `LlmCallFinished.provider_id` changed
/// from `ProviderId` (with a `"unresolved"` sentinel string for the
/// "no provider ran" case) to `Option<ProviderId>`. The v1→v2
/// migration walks the `pipeline_events` rows and rewrites any
/// payload carrying `provider_id: "unresolved"` to
/// `provider_id: null`. Idempotent.
const SCHEMA_VERSION: i64 = 2;

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
    async fn query(&self, q: &PipelineEventQuery) -> Result<Vec<PipelineEvent>, StorageError>;

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
    pub fn open(config: SqlitePipelineEventStoreConfig) -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open(&config.path).map_err(|e| {
            StorageError::Backend(format!(
                "opening pipeline event store at {:?}: {e}",
                config.path
            ))
        })?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self {
            conn: Arc::new(Mutex::new(conn)),
        }))
    }

    pub fn in_memory() -> Result<Arc<Self>, StorageError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            StorageError::Backend(format!("opening in-memory pipeline event store: {e}"))
        })?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;
        Ok(Arc::new(Self {
            conn: Arc::new(Mutex::new(conn)),
        }))
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
        if current != 0 && current != 1 {
            return Err(StorageError::Backend(format!(
                "incompatible pipeline event store schema (file v{current}, code v{SCHEMA_VERSION})"
            )));
        }
        if current == 0 {
            // Schema notes:
            // - `event_id` is TEXT for UUID readability; PRIMARY KEY
            //   makes re-append idempotent (INSERT OR REPLACE).
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
        }

        if current <= 1 {
            // v1→v2: ARC-L5-SW-10. Rewrite any LlmCallFinished payload
            // that carries the legacy `provider_id: "unresolved"`
            // sentinel into `provider_id: null`. We do this in
            // Rust-space rather than SQL because the column is a JSON
            // blob — SQLite's `json_set` would work for shallow
            // overrides but the payload is the *source of truth* for
            // event-replay code, and a one-shot Rust pass keeps the
            // transform colocated with the type it's rewriting.
            migrate_v1_to_v2_unresolved_to_null(conn)?;
        }

        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| StorageError::Backend(format!("set user_version: {e}")))?;
        Ok(())
    }
}

/// v1→v2 schema migration (ARC-L5-SW-10). Walks `pipeline_events` for
/// rows whose `event_type = 'llm_call_finished'` and whose payload
/// carries `provider_id: "unresolved"`, and rewrites that field to
/// `null` so the payload matches the new `Option<ProviderId>` shape.
/// Idempotent — payloads already on the new shape are skipped without
/// re-serializing (no spurious row writes).
fn migrate_v1_to_v2_unresolved_to_null(conn: &Connection) -> Result<(), StorageError> {
    // Scope: only `llm_call_finished` carries `provider_id`. Filter by
    // event_type so we don't waste cycles deserializing/scanning other
    // payload shapes.
    let mut stmt = conn
        .prepare(
            "SELECT event_id, payload_json FROM pipeline_events \
             WHERE event_type = 'llm_call_finished'",
        )
        .map_err(|e| {
            StorageError::Backend(format!("v1→v2 migrate: prepare select: {e}"))
        })?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .map_err(|e| StorageError::Backend(format!("v1→v2 migrate: query: {e}")))?;

    let mut updates: Vec<(String, Vec<u8>)> = Vec::new();
    for row in rows {
        let (event_id, payload) = row
            .map_err(|e| StorageError::Backend(format!("v1→v2 migrate: read row: {e}")))?;
        let mut v: serde_json::Value = match serde_json::from_slice(&payload) {
            Ok(v) => v,
            Err(_) => {
                // A malformed payload predates our integrity checks;
                // skip rather than fail the open so the operator can
                // still read newer rows.
                continue;
            }
        };
        // Pipeline events use externally-tagged enum encoding; the
        // LlmCallFinished body lives under v["LlmCallFinished"].
        let body = match v.get_mut("LlmCallFinished") {
            Some(b) => b,
            None => continue,
        };
        let needs_rewrite = body
            .get("provider_id")
            .and_then(|p| p.as_str())
            .is_some_and(|s| s == "unresolved");
        if !needs_rewrite {
            continue;
        }
        if let Some(obj) = body.as_object_mut() {
            obj.insert("provider_id".into(), serde_json::Value::Null);
        }
        let new_payload = serde_json::to_vec(&v).map_err(|e| {
            StorageError::Backend(format!(
                "v1→v2 migrate: re-encode payload for {event_id}: {e}"
            ))
        })?;
        updates.push((event_id, new_payload));
    }
    drop(stmt);

    if updates.is_empty() {
        return Ok(());
    }

    let mut update_stmt = conn
        .prepare("UPDATE pipeline_events SET payload_json = ?1 WHERE event_id = ?2")
        .map_err(|e| {
            StorageError::Backend(format!("v1→v2 migrate: prepare update: {e}"))
        })?;
    for (event_id, payload) in updates {
        update_stmt
            .execute(rusqlite::params![payload, event_id])
            .map_err(|e| {
                StorageError::Backend(format!(
                    "v1→v2 migrate: update {event_id}: {e}"
                ))
            })?;
    }
    Ok(())
}

const DEFAULT_QUERY_LIMIT: u32 = 10_000;

fn ts_to_ms(t: SystemTime) -> i64 {
    use std::time::UNIX_EPOCH;
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

    async fn query(&self, q: &PipelineEventQuery) -> Result<Vec<PipelineEvent>, StorageError> {
        let conn = self.conn.clone();
        let tenant = q.tenant_id.as_ref().map(|t| t.as_ref().to_string());
        let since = q.since.map(ts_to_ms);
        let until = q.until.map(ts_to_ms);
        let limit = q.limit.unwrap_or(DEFAULT_QUERY_LIMIT) as i64;

        let blobs = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<u8>>, StorageError> {
            let conn = conn.lock().expect("pipeline event store mutex poisoned");

            // Build SQL incrementally — keep the where clause to
            // indexed columns only (tenant_id, timestamp_ms).
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
            provider_id: Some(ProviderId::new("p")),
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
        // Second append of same event_id replaces (no PK violation).
        s.append(std::slice::from_ref(&ev)).await.unwrap();
        let got = s.query(&PipelineEventQuery::default()).await.unwrap();
        assert_eq!(got.len(), 1);
    }

    /// Pin v1→v2 migration: a v1 database carrying an
    /// `LlmCallFinished` payload with `provider_id: "unresolved"`
    /// (the legacy SW-10 sentinel) must come out the other side with
    /// `provider_id: null`, and the row count must be unchanged.
    /// Idempotent on a v2-shape payload (already-null stays null,
    /// already-Some stays Some).
    #[test]
    fn migrate_v1_to_v2_rewrites_unresolved_sentinel_to_null() {
        // Hand-build a v1 store: open in-memory, force user_version=1,
        // create the v1 schema, hand-insert a row carrying the legacy
        // sentinel payload.
        let conn = Connection::open_in_memory().unwrap();
        SqlitePipelineEventStore::pragma_setup(&conn).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE pipeline_events (
                event_id        TEXT    NOT NULL PRIMARY KEY,
                event_type      TEXT    NOT NULL,
                timestamp_ms    INTEGER NOT NULL,
                tenant_id       TEXT    NOT NULL,
                payload_json    BLOB    NOT NULL
            ) STRICT;
            "#,
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();

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
            ("00000000-0000-0000-0000-000000000003", &already_null_payload),
        ] {
            conn.execute(
                "INSERT INTO pipeline_events (event_id, event_type, timestamp_ms, \
                 tenant_id, payload_json) VALUES (?1, 'llm_call_finished', 0, 't1', ?2)",
                rusqlite::params![id, serde_json::to_vec(payload).unwrap()],
            )
            .unwrap();
        }

        // Run the same migrate() the production path runs at open.
        SqlitePipelineEventStore::migrate(&conn).unwrap();

        // user_version is now SCHEMA_VERSION (2).
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        // Row count unchanged.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM pipeline_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);

        // The "unresolved" row is now null.
        let body: Vec<u8> = conn
            .query_row(
                "SELECT payload_json FROM pipeline_events WHERE event_id = ?1",
                ["00000000-0000-0000-0000-000000000001"],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            v["LlmCallFinished"]["provider_id"].is_null(),
            "legacy sentinel rewritten to null: {v}"
        );

        // The already-resolved row is untouched.
        let body: Vec<u8> = conn
            .query_row(
                "SELECT payload_json FROM pipeline_events WHERE event_id = ?1",
                ["00000000-0000-0000-0000-000000000002"],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["LlmCallFinished"]["provider_id"], "openai-1");

        // The already-null row is also untouched (idempotent on the
        // new shape — no double-rewrite, no spurious row write).
        let body: Vec<u8> = conn
            .query_row(
                "SELECT payload_json FROM pipeline_events WHERE event_id = ?1",
                ["00000000-0000-0000-0000-000000000003"],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["LlmCallFinished"]["provider_id"].is_null());

        // Running migrate() again is a no-op (already at SCHEMA_VERSION).
        SqlitePipelineEventStore::migrate(&conn).unwrap();
    }
}
