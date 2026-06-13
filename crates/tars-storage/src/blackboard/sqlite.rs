//! The SQLite backing of the [`Blackboard`] model — **generic** over a
//! [`BlackboardCodec`]. The consumer writes no SQL: this owns the schema (two
//! tables) and the write/read logic; the codec supplies only domain glue.
//!
//! One valid way to honor the backing contract (Doc 19 §4): a single connection
//! behind `Arc<Mutex>` (writes serialize; WAL lets reads proceed), idempotent
//! keyed upserts, an append-only event log with per-event provenance.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use rusqlite::{params, params_from_iter, Connection};

use super::{BbError, Blackboard, BlackboardCodec, Scope, Transition};

/// SQLite-backed blackboard, scoped to one run (`run_id`), generic over the
/// consumer's [`BlackboardCodec`]. Builds + owns its two tables.
pub struct SqliteBlackboard<C: BlackboardCodec> {
    conn: Arc<Mutex<Connection>>,
    run_id: String,
    _codec: PhantomData<fn() -> C>,
}

impl<C: BlackboardCodec> SqliteBlackboard<C> {
    /// Open over an existing connection (the consumer may share its db),
    /// creating the blackboard tables if absent. Run-scoped by `run_id`.
    pub fn open(conn: Arc<Mutex<Connection>>, run_id: impl Into<String>) -> Result<Self, BbError> {
        {
            let guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            guard.execute_batch(
                "CREATE TABLE IF NOT EXISTS bb_entities (
                     bb_key         TEXT PRIMARY KEY,
                     value          TEXT NOT NULL,
                     status         TEXT NOT NULL,
                     first_seen_run TEXT NOT NULL,
                     last_seen_run  TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS bb_events (
                     bb_key   TEXT NOT NULL,
                     run_id   TEXT NOT NULL,
                     kind     TEXT NOT NULL,
                     at       INTEGER NOT NULL,
                     version  TEXT,
                     reason   TEXT,
                     -- UNIQUE event identity ⇒ law #3 (idempotent on key+run+kind)
                     PRIMARY KEY (bb_key, run_id, kind)
                 );",
            )?;
        }
        Ok(Self { conn, run_id: run_id.into(), _codec: PhantomData })
    }

    /// Convenience: a private in-memory board (tests / ephemeral use).
    pub fn in_memory(run_id: impl Into<String>) -> Result<Self, BbError> {
        let conn = Connection::open_in_memory()?;
        Self::open(Arc::new(Mutex::new(conn)), run_id)
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    fn read_timeline(conn: &Connection, key: &str) -> Result<Vec<C::Event>, BbError> {
        let mut stmt = conn.prepare(
            "SELECT kind FROM bb_events WHERE bb_key = ?1 ORDER BY at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([key], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(C::event_from_str(&r?));
        }
        Ok(out)
    }
}

impl<C: BlackboardCodec> Blackboard for SqliteBlackboard<C> {
    type Entity = C::Entity;
    type Event = C::Event;

    fn view(&self, scope: &Scope) -> Result<Vec<C::Entity>, BbError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut values: Vec<String> = Vec::new();
        match scope {
            Scope::All => {
                let mut stmt =
                    conn.prepare("SELECT value FROM bb_entities ORDER BY bb_key")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                for r in rows {
                    values.push(r?);
                }
            }
            Scope::WithStatus(statuses) => {
                if statuses.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders = vec!["?"; statuses.len()].join(",");
                let sql = format!(
                    "SELECT value FROM bb_entities WHERE status IN ({placeholders}) ORDER BY bb_key"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_from_iter(statuses), |r| r.get::<_, String>(0))?;
                for r in rows {
                    values.push(r?);
                }
            }
            Scope::FirstSeenIn(run) => {
                let mut stmt = conn.prepare(
                    "SELECT value FROM bb_entities WHERE first_seen_run = ?1 ORDER BY bb_key",
                )?;
                let rows = stmt.query_map([run], |r| r.get::<_, String>(0))?;
                for r in rows {
                    values.push(r?);
                }
            }
        }
        values.iter().map(|v| C::decode(v)).collect()
    }

    fn timeline(&self, key: &str) -> Result<Vec<C::Event>, BbError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Self::read_timeline(&conn, key)
    }

    fn commit(&self, e: &C::Entity, t: Transition<C::Event>) -> Result<(), BbError> {
        let key = C::key(e);
        let value = C::encode(e)?;
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // Law #2 (atomic): the entity upsert + event append are ONE transaction.
        let tx = conn.transaction()?;

        // Upsert the value; preserve first_seen_run (the entity's birth run is
        // immutable), refresh value + last_seen_run. Status is set by the
        // re-projection below, NOT here — except the INSERT seeds the initial
        // status for a brand-new entity whose timeline doesn't project yet.
        tx.execute(
            "INSERT INTO bb_entities (bb_key, value, status, first_seen_run, last_seen_run)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(bb_key) DO UPDATE SET
                 value = excluded.value,
                 last_seen_run = excluded.last_seen_run",
            params![key, value, C::initial_status(e), self.run_id],
        )?;

        // Append the event idempotently (law #3): same (key, run, kind) ⇒ no
        // second row.
        tx.execute(
            "INSERT INTO bb_events (bb_key, run_id, kind, at, version, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(bb_key, run_id, kind) DO NOTHING",
            params![key, self.run_id, C::event_str(t.kind), t.at, t.version, t.reason],
        )?;

        // Law #5 (value ≡ timeline): re-derive status from the (post-append)
        // timeline via the consumer's projection. `None` ⇒ keep the seeded
        // initial status (a bare sighting is not a transition).
        let timeline = Self::read_timeline(&tx, &key)?;
        if let Some(status) = C::project_status(&timeline) {
            tx.execute(
                "UPDATE bb_entities SET status = ?2 WHERE bb_key = ?1",
                params![key, status],
            )?;
        }

        tx.commit()?;
        Ok(())
    }
}
