//! [`SqliteBlackboard`] — the SQLite **orchestrator**. It owns the run's
//! connection (`Arc<Mutex<Connection>>`) + `run_id` (the run context) and the
//! transaction; it does NOT own any domain row. Every storage operation is an
//! injected [`BlackboardStore`] free function — tars never learns where the
//! connection came from, what the rows are, or how the consumer configured it.
//!
//! The five laws live in this orchestration: `commit` opens ONE transaction
//! (atomic, law #2), calls the injected ops IN ORDER — upsert, append (the
//! store's UNIQUE key gives idempotency, law #3), then `sync_status` (the
//! value≡timeline fold, law #5) — and commits.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::{BbError, Blackboard, BlackboardStore, Scope, Transition};

/// SQLite-backed blackboard, scoped to one run, generic over an injected
/// [`BlackboardStore`]. Holds the connection the consumer handed in — tars does
/// not open, configure, or know the schema of it.
pub struct SqliteBlackboard<S: BlackboardStore> {
    conn: Arc<Mutex<Connection>>,
    run_id: String,
    _store: PhantomData<fn() -> S>,
}

impl<S: BlackboardStore> SqliteBlackboard<S> {
    /// Wrap a connection the consumer owns, scoped to `run_id`. Calls the
    /// store's `init` (a no-op for a store over existing tables).
    pub fn open(conn: Arc<Mutex<Connection>>, run_id: impl Into<String>) -> Result<Self, BbError> {
        {
            let guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            S::init(&guard)?;
        }
        Ok(Self { conn, run_id: run_id.into(), _store: PhantomData })
    }

    /// Convenience: a private in-process SQLite db (tests / ephemeral runs).
    pub fn in_memory(run_id: impl Into<String>) -> Result<Self, BbError> {
        Self::open(Arc::new(Mutex::new(Connection::open_in_memory()?)), run_id)
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl<S: BlackboardStore> Blackboard for SqliteBlackboard<S> {
    type Entity = S::Entity;
    type Event = S::Event;

    fn view(&self, scope: &Scope) -> Result<Vec<S::Entity>, BbError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        S::view(&conn, scope)
    }

    fn timeline(&self, key: &str) -> Result<Vec<S::Event>, BbError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        S::read_timeline(&conn, key)
    }

    fn commit(&self, e: &S::Entity, t: Transition<S::Event>) -> Result<(), BbError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // Law #2 (atomic): the whole transition is ONE transaction.
        let tx = conn.transaction()?;
        S::upsert(&tx, e)?;
        // Law #3 (idempotent): the store's UNIQUE(key, run, kind) absorbs a re-append.
        S::append_event(
            &tx,
            e,
            &self.run_id,
            t.kind,
            t.at,
            t.version.as_deref(),
            t.reason.as_deref(),
        )?;
        // Law #5 (value ≡ timeline): re-derive status from the post-append log.
        S::sync_status(&tx, &S::key(e))?;
        tx.commit()?;
        Ok(())
    }
}
