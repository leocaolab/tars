//! M0 — the always-on durability store.
//!
//! Three tables in the runtime's OWN sqlite file, written through ONE
//! [`SqliteBlackboard::commit`] transaction per step:
//!
//! - **`answers`** — the [`AnswerStore`]: step-identity → [`StepAnswer`]
//!   (the persisted worker output). This IS the checkpoint. **No TTL**
//!   — a checkpoint that expired would silently un-memoize a completed
//!   step, so unlike `tars-cache` (24 h default) rows never expire.
//! - **`result_events`** — an append-only log with a monotonic,
//!   gap-free `seq` per job and a `since`-cursor read. Reuses the SQL
//!   *pattern* from `tars_storage::SqliteEventStore` (`sqlite.rs:107` /
//!   `event_store.rs:70`) but in OUR table — never the off-able shared
//!   `EventStore` instance.
//! - **`jobs`** — one row per durable job (the status of record).
//!   `updated_at` is advanced inside the SAME `commit` transaction as
//!   each step's answer (job-state ≡ answer, atomically).
//!
//! ## Critical invariant
//! This store is instantiated on its own `rusqlite::Connection`,
//! completely independent of `tars_storage::EventStore` /
//! `StoreScope::Off` / `*_EVENTS_OFF`. Correctness (checkpoint + resume)
//! never reads or writes the observability sink.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use tars_runtime::{AgentMessage, Plan};
use tars_storage::{
    BbError, Blackboard, BlackboardDomain, BlackboardStore, Scope, SqliteBlackboard, Transition,
};
use tars_types::Usage;

use crate::error::DurableError;

/// ASCII unit-separator — joins `(job_id, step_id)` into the blackboard
/// key. Not a legal char in a normal id, so the composite is unambiguous.
const KEY_SEP: char = '\u{1f}';

/// Terminal state a job reaches once every step is resolved.
pub const JOB_STATUS_RUNNING: &str = "running";
pub const JOB_STATUS_DONE: &str = "done";

// ─── Persisted value (C3 AnswerStore value) ────────────────────────────

/// One step's checkpointed result — the AnswerStore value. Its own type
/// (NOT the cache's `CachedResponse`, which is welded to `ChatResponse`):
/// a durable step result is an [`AgentMessage::PartialResult`] + usage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepAnswer {
    pub job_id: String,
    pub step_id: String,
    /// The worker's output — always [`AgentMessage::PartialResult`] for a
    /// completed step; a synthetic skip marker for a skipped step.
    pub message: AgentMessage,
    /// Token usage the worker reported (zeros for non-LLM / skipped).
    pub usage: Usage,
    /// `ChatResponse::created` (unix seconds) the worker carried up; `0`
    /// for non-LLM / skipped steps.
    pub created: i64,
    /// Projected from the timeline by the blackboard (`completed` /
    /// `skipped`). Ignored on write — `sync_status` overwrites it.
    #[serde(default)]
    pub status: String,
}

impl StepAnswer {
    /// A completed step: the worker's real output.
    pub fn completed(job_id: &str, step_id: &str, message: AgentMessage, usage: Usage, created: i64) -> Self {
        Self {
            job_id: job_id.to_string(),
            step_id: step_id.to_string(),
            message,
            usage,
            created,
            status: STATUS_COMPLETED.to_string(),
        }
    }

    /// A skipped step: presence in the store so it is never re-run and
    /// its dependents cascade, carrying the human-readable reason.
    pub fn skipped(job_id: &str, step_id: &str, reason: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            step_id: step_id.to_string(),
            message: AgentMessage::PartialResult {
                from_agent: tars_types::AgentId::new("durable-skip"),
                step_id: Some(step_id.to_string()),
                summary: format!("(skipped: {reason})"),
                confidence: 0.0,
            },
            usage: Usage::default(),
            created: 0,
            status: STATUS_SKIPPED.to_string(),
        }
    }

    /// Blackboard key = `job_id ␟ step_id` (globally unique across jobs
    /// sharing the file, so `sync_status`/timeline reads by key alone
    /// can't collide between two jobs that reused a step id).
    fn key(&self) -> String {
        format!("{}{KEY_SEP}{}", self.job_id, self.step_id)
    }

    pub fn is_skipped(&self) -> bool {
        self.status == STATUS_SKIPPED
    }
}

const STATUS_PENDING: &str = "pending";
const STATUS_COMPLETED: &str = "completed";
const STATUS_SKIPPED: &str = "skipped";

// ─── Transition kind (the closed event set) ─────────────────────────────

/// The closed set of transitions appended to a step's `result_events`
/// timeline. A typed enum (not a magic string) per the domain-value rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultEventKind {
    /// The worker ran and its answer was checkpointed.
    Completed,
    /// The step was skipped (condition false or a dep was skipped).
    Skipped,
}

impl ResultEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Skipped => "skipped",
        }
    }
}

/// One appended result event, as read back from the log (reuses the
/// `EventRecord` shape from `tars_storage`, in OUR table).
#[derive(Clone, Debug)]
pub struct ResultEventRecord {
    pub job_id: String,
    /// 1-indexed, monotonic, gap-free per job.
    pub seq: u64,
    pub step_id: String,
    pub kind: ResultEventKind,
    pub at: i64,
    pub reason: Option<String>,
}

// ─── BlackboardStore domain (the injection port) ────────────────────────

/// Zero-size domain marker plugged into [`SqliteBlackboard`]. Carries the
/// five-law storage ops over the durable tables. The connection + run
/// scoping live on [`SqliteBlackboard`]; the high-level API on
/// [`DurableStore`].
pub struct DurableBoard;

impl BlackboardDomain for DurableBoard {
    type Entity = StepAnswer;
    type Event = ResultEventKind;

    fn key(e: &StepAnswer) -> String {
        e.key()
    }

    fn initial_status(_e: &StepAnswer) -> String {
        STATUS_PENDING.to_string()
    }

    fn event_str(ev: ResultEventKind) -> String {
        ev.as_str().to_string()
    }

    fn event_from_str(s: &str) -> ResultEventKind {
        match s {
            "skipped" => ResultEventKind::Skipped,
            // "completed" and any unknown wire string fold to Completed —
            // the only other member of the closed set.
            _ => ResultEventKind::Completed,
        }
    }

    fn project_status(timeline: &[ResultEventKind]) -> Option<String> {
        timeline.last().map(|ev| match ev {
            ResultEventKind::Completed => STATUS_COMPLETED.to_string(),
            ResultEventKind::Skipped => STATUS_SKIPPED.to_string(),
        })
    }

    fn with_status(e: &StepAnswer, status: &str) -> StepAnswer {
        let mut out = e.clone();
        out.status = status.to_string();
        out
    }
}

impl BlackboardStore for DurableBoard {
    fn init(conn: &Connection) -> Result<(), BbError> {
        init_schema(conn).map_err(BbError::from)
    }

    fn upsert(conn: &Connection, e: &StepAnswer) -> Result<(), BbError> {
        let message_json = serde_json::to_string(&e.message).map_err(BbError::store)?;
        let usage_json = serde_json::to_string(&e.usage).map_err(BbError::store)?;
        conn.execute(
            "INSERT INTO answers (key, job_id, step_id, message_json, usage_json, created, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(key) DO UPDATE SET \
                message_json = excluded.message_json, \
                usage_json   = excluded.usage_json, \
                created      = excluded.created",
            params![
                e.key(),
                e.job_id,
                e.step_id,
                message_json,
                usage_json,
                e.created,
                e.status,
            ],
        )?;
        // Law-adjacent: advance the JOB's state in the SAME transaction as
        // the answer, so "job state ≡ answers" is atomic. The row is
        // created at submit; a missing row (0 rows updated) is fine here —
        // the answer still lands.
        conn.execute(
            "UPDATE jobs SET updated_at = ?2 WHERE job_id = ?1",
            params![e.job_id, now_ms()],
        )?;
        Ok(())
    }

    fn append_event(
        conn: &Connection,
        e: &StepAnswer,
        run: &str,
        ev: ResultEventKind,
        at: i64,
        _version: Option<&str>,
        reason: Option<&str>,
        _role: Option<&str>,
    ) -> Result<bool, BbError> {
        // Monotonic, gap-free seq per job (== `run`). Computed inside the
        // caller's transaction, exactly like SqliteEventStore::append.
        let next_seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM result_events WHERE job_id = ?1",
            params![run],
            |r| r.get(0),
        )?;
        // Idempotent on (key, kind) — key embeds job_id, so this is the
        // five-law `(key, run, kind)` uniqueness. A re-append is absorbed
        // (0 rows) and consumes no seq.
        let changed = conn.execute(
            "INSERT OR IGNORE INTO result_events (job_id, seq, key, step_id, kind, at, reason) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![run, next_seq, e.key(), e.step_id, ev.as_str(), at, reason],
        )?;
        Ok(changed == 1)
    }

    fn read_timeline(conn: &Connection, key: &str) -> Result<Vec<ResultEventKind>, BbError> {
        let mut stmt =
            conn.prepare("SELECT kind FROM result_events WHERE key = ?1 ORDER BY seq ASC")?;
        let rows = stmt.query_map(params![key], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(DurableBoard::event_from_str(&row?));
        }
        Ok(out)
    }

    fn sync_status(conn: &Connection, key: &str) -> Result<(), BbError> {
        let timeline = Self::read_timeline(conn, key)?;
        if let Some(status) = DurableBoard::project_status(&timeline) {
            conn.execute(
                "UPDATE answers SET status = ?2 WHERE key = ?1",
                params![key, status],
            )?;
        }
        Ok(())
    }

    fn view(conn: &Connection, scope: &Scope) -> Result<Vec<StepAnswer>, BbError> {
        let (sql, bound): (String, Vec<String>) = match scope {
            Scope::All => (
                "SELECT job_id, step_id, message_json, usage_json, created, status FROM answers"
                    .to_string(),
                Vec::new(),
            ),
            Scope::FirstSeenIn(job_id) => (
                "SELECT job_id, step_id, message_json, usage_json, created, status \
                 FROM answers WHERE job_id = ?1"
                    .to_string(),
                vec![job_id.clone()],
            ),
            Scope::WithStatus(statuses) => {
                if statuses.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders =
                    (1..=statuses.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
                (
                    format!(
                        "SELECT job_id, step_id, message_json, usage_json, created, status \
                         FROM answers WHERE status IN ({placeholders})"
                    ),
                    statuses.clone(),
                )
            }
        };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            bound.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(param_refs.as_slice(), row_to_answer)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(row_decode)?);
        }
        Ok(out)
    }
}

/// Map a rusqlite row-decode failure (bad JSON in a column) to a
/// `BbError::Store` carrying the typed serde error.
fn row_decode(e: rusqlite::Error) -> BbError {
    match e {
        rusqlite::Error::FromSqlConversionFailure(_, _, src) => BbError::Store(src),
        other => BbError::Sqlite(other),
    }
}

/// Raw `answers` columns (`job_id, step_id, message_json, usage_json,
/// created, status`) before JSON decode — lets [`DurableStore`] reads
/// surface a typed [`DurableError::Serde`] instead of burying it in a
/// rusqlite conversion error.
type RawAnswerRow = (String, String, String, String, i64, String);

fn raw_answer_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawAnswerRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
}

fn decode_answer(raw: RawAnswerRow) -> Result<StepAnswer, DurableError> {
    let (job_id, step_id, message_json, usage_json, created, status) = raw;
    Ok(StepAnswer {
        job_id,
        step_id,
        message: serde_json::from_str(&message_json)?,
        usage: serde_json::from_str(&usage_json)?,
        created,
        status,
    })
}

fn row_to_answer(r: &rusqlite::Row<'_>) -> rusqlite::Result<StepAnswer> {
    let message_json: String = r.get(2)?;
    let usage_json: String = r.get(3)?;
    let message: AgentMessage = serde_json::from_str(&message_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let usage: Usage = serde_json::from_str(&usage_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(StepAnswer {
        job_id: r.get(0)?,
        step_id: r.get(1)?,
        message,
        usage,
        created: r.get(4)?,
        status: r.get(5)?,
    })
}

/// Create the three durable tables (idempotent). Called by
/// [`DurableBoard::init`] on every blackboard open and by
/// [`DurableStore::open`].
fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS jobs (
            job_id           TEXT    PRIMARY KEY,
            status           TEXT    NOT NULL,
            plan_json        TEXT    NOT NULL,
            created_at       INTEGER NOT NULL,
            updated_at       INTEGER NOT NULL,
            cancel_requested INTEGER NOT NULL DEFAULT 0
        ) STRICT;

        CREATE TABLE IF NOT EXISTS answers (
            key          TEXT    PRIMARY KEY,
            job_id       TEXT    NOT NULL,
            step_id      TEXT    NOT NULL,
            message_json TEXT    NOT NULL,
            usage_json   TEXT    NOT NULL,
            created      INTEGER NOT NULL,
            status       TEXT    NOT NULL
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_answers_job ON answers(job_id);

        CREATE TABLE IF NOT EXISTS result_events (
            job_id  TEXT    NOT NULL,
            seq     INTEGER NOT NULL,
            key     TEXT    NOT NULL,
            step_id TEXT    NOT NULL,
            kind    TEXT    NOT NULL,
            at      INTEGER NOT NULL,
            reason  TEXT,
            PRIMARY KEY (job_id, seq),
            UNIQUE (key, kind)
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_result_events_job ON result_events(job_id);
        "#,
    )
}

// ─── DurableStore — the high-level always-on API ────────────────────────

/// The durable runtime's own store. Holds one `rusqlite::Connection`
/// (its OWN file — never the observability event store) and exposes the
/// checkpoint + job API. Cheap to clone (`Arc`-shared connection).
#[derive(Clone)]
pub struct DurableStore {
    conn: Arc<Mutex<Connection>>,
}

impl DurableStore {
    /// Open (creating if needed) the durable store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DurableError> {
        let conn = Connection::open(path.as_ref())?;
        Self::from_conn(conn)
    }

    /// Private in-memory store for tests / ephemeral use.
    pub fn in_memory() -> Result<Self, DurableError> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self, DurableError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        init_schema(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Persist a fresh durable job (the status of record). Idempotent:
    /// re-submitting the same id leaves the existing row untouched.
    pub fn create_job(&self, job_id: &str, plan: &Plan) -> Result<(), DurableError> {
        let plan_json = serde_json::to_string(plan)?;
        let now = now_ms();
        self.lock().execute(
            "INSERT OR IGNORE INTO jobs (job_id, status, plan_json, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![job_id, JOB_STATUS_RUNNING, plan_json, now],
        )?;
        Ok(())
    }

    /// Atomically checkpoint one step: `{answer + result event + job
    /// updated_at}` in ONE [`SqliteBlackboard::commit`] transaction.
    pub fn commit_step(
        &self,
        answer: &StepAnswer,
        kind: ResultEventKind,
        reason: Option<&str>,
    ) -> Result<(), DurableError> {
        // The blackboard is scoped to run = job_id, so `commit` stamps
        // the job automatically into `append_event`'s `run` arg.
        let bb: SqliteBlackboard<DurableBoard> =
            SqliteBlackboard::open(self.conn.clone(), answer.job_id.clone())?;
        let mut transition = Transition::new(kind, now_ms(), None);
        if let Some(r) = reason {
            transition = transition.with_reason(r);
        }
        bb.commit(answer, transition)?;
        Ok(())
    }

    /// The AnswerStore, scoped to one job: `step_id → StepAnswer` for
    /// every present (completed or skipped) step. The scheduler's
    /// readiness/skip test derives entirely from this — the frontier is
    /// DERIVED, never stored.
    pub fn answers(&self, job_id: &str) -> Result<HashMap<String, StepAnswer>, DurableError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT job_id, step_id, message_json, usage_json, created, status \
             FROM answers WHERE job_id = ?1",
        )?;
        let rows = stmt.query_map(params![job_id], raw_answer_row)?;
        let mut out = HashMap::new();
        for row in rows {
            let a = decode_answer(row?)?;
            out.insert(a.step_id.clone(), a);
        }
        Ok(out)
    }

    /// One step's checkpoint, if present.
    pub fn answer(&self, job_id: &str, step_id: &str) -> Result<Option<StepAnswer>, DurableError> {
        let key = format!("{job_id}{KEY_SEP}{step_id}");
        let conn = self.lock();
        let raw = conn
            .query_row(
                "SELECT job_id, step_id, message_json, usage_json, created, status \
                 FROM answers WHERE key = ?1",
                params![key],
                raw_answer_row,
            )
            .optional()?;
        raw.map(decode_answer).transpose()
    }

    /// Read a job's `result_events`, `seq > since`, in order — the
    /// `read_since` cursor pattern in our own always-on table.
    pub fn result_events_since(
        &self,
        job_id: &str,
        since: u64,
    ) -> Result<Vec<ResultEventRecord>, DurableError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT job_id, seq, step_id, kind, at, reason FROM result_events \
             WHERE job_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![job_id, since as i64], |r| {
            let seq: i64 = r.get(1)?;
            let kind: String = r.get(3)?;
            Ok(ResultEventRecord {
                job_id: r.get(0)?,
                seq: seq as u64,
                step_id: r.get(2)?,
                kind: DurableBoard::event_from_str(&kind),
                at: r.get(4)?,
                reason: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All of a job's result events (`read_since(0)`).
    pub fn result_events(&self, job_id: &str) -> Result<Vec<ResultEventRecord>, DurableError> {
        self.result_events_since(job_id, 0)
    }

    /// The persisted plan for a job (the status of record — how resume
    /// re-drives without the caller re-supplying it).
    pub fn load_plan(&self, job_id: &str) -> Result<Plan, DurableError> {
        let conn = self.lock();
        let plan_json: Option<String> = conn
            .query_row(
                "SELECT plan_json FROM jobs WHERE job_id = ?1",
                params![job_id],
                |r| r.get(0),
            )
            .optional()?;
        let plan_json = plan_json.ok_or_else(|| DurableError::JobNotFound(job_id.to_string()))?;
        Ok(serde_json::from_str(&plan_json)?)
    }

    /// A job's current lifecycle status, if the row exists.
    pub fn job_status(&self, job_id: &str) -> Result<Option<String>, DurableError> {
        let conn = self.lock();
        let out = conn
            .query_row(
                "SELECT status FROM jobs WHERE job_id = ?1",
                params![job_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(out)
    }

    /// Set a job's lifecycle status (e.g. mark terminal when every step
    /// resolved). A single indexed UPDATE.
    pub fn set_job_status(&self, job_id: &str, status: &str) -> Result<(), DurableError> {
        self.lock().execute(
            "UPDATE jobs SET status = ?2, updated_at = ?3 WHERE job_id = ?1",
            params![job_id, status, now_ms()],
        )?;
        Ok(())
    }
}

/// Wall-clock milliseconds since the Unix epoch (clamped forward, never
/// negative), matching `tars_storage::sqlite`'s timestamp convention.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
