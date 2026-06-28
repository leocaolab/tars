//! The blackboard MODEL (Doc 19 §4.1) — **framework mechanism**, zero domain.
//!
//! A blackboard is a keyed set of **entities**; each has a current value and an
//! **append-only timeline of self-describing events**. Two operations —
//! `view` + `commit` — and five laws. It is the single framework-owned write
//! path: a step appends ITS event at source, so a timeline can never collapse to
//! only its last event (the bug the model exists to kill).
//!
//! ## Responsibility split (this is the whole point)
//! tars owns the **mechanism**, bound to the run/pipeline context but NOT to any
//! domain:
//!   - [`Blackboard`] — the contract (view/timeline/commit).
//!   - [`BlackboardStore`] — the **injection port**: the storage operations a
//!     consumer plugs in (upsert / append_event / read_timeline / sync_status /
//!     view), each taking a `&Connection`. tars never writes a domain row.
//!   - [`SqliteBlackboard`] — the **orchestrator**: holds the run's connection +
//!     `run_id` (the context), and on `commit` opens one transaction and calls
//!     the injected ops IN ORDER. That orchestration is where the five laws are
//!     enforced (atomic = the tx; idempotent = the store's UNIQUE key;
//!     value≡timeline = it calls `sync_status` after the append).
//!
//! The consumer owns the **domain + storage**: a `BlackboardStore` impl that
//! reuses ITS OWN tables and queries. (Reference: A.R.C.'s `FindingStore` wraps
//! its existing `findings`/`finding_events` writers — no blob, its report/board
//! keep reading the same tables.)
//!
//! ## The five laws (Doc 19 §4.1)
//! 1. **Append-only** — `commit` never deletes/mutates a prior event.
//! 2. **Atomic** — value-set + event-append is one transaction per commit.
//! 3. **Idempotent on `(key, run, kind)`** — a re-committed transition is absorbed.
//! 4. **Read-your-writes** — after `commit(e, ..)`, `view(scope ∋ e)` sees it.
//! 5. **Value ≡ timeline** — status is a projection of the event log.

mod memory;
mod sqlite;

pub use memory::InMemoryBlackboard;
pub use sqlite::SqliteBlackboard;

#[cfg(test)]
mod laws;

use rusqlite::Connection;

/// A read selector over entities — the framework-universal dimensions (every
/// blackboard has a status and a birth run).
#[derive(Debug, Clone)]
pub enum Scope {
    All,
    WithStatus(Vec<String>),
    FirstSeenIn(String),
}

/// One transition appended to an entity's timeline (Doc 19 §4.1). `version` is
/// the state-of-world it happened in; `at` is captured AT the step.
#[derive(Debug, Clone)]
pub struct Transition<Ev> {
    pub kind: Ev,
    pub at: i64,
    pub version: Option<String>,
    pub reason: Option<String>,
    /// The ROLE that produced this transition — recorded at the SOURCE by the
    /// sending agent, never derived from the event kind downstream.
    pub role: Option<String>,
}

impl<Ev> Transition<Ev> {
    pub fn new(kind: Ev, at: i64, version: Option<String>) -> Self {
        Self { kind, at, version, reason: None, role: None }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }
}

/// Blackboard failure. `Store` carries the consumer's OWN typed storage error
/// boxed (so a caller can downcast back to it — never stringified) at the model
/// boundary; tars doesn't know the concrete type.
#[derive(Debug, thiserror::Error)]
pub enum BbError {
    #[error("blackboard sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("blackboard store: {0}")]
    Store(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl BbError {
    /// Wrap a consumer storage error, keeping it typed (downcastable).
    pub fn store(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Store(Box::new(e))
    }
}

/// The **domain seam** shared by BOTH backings: how a consumer's entity is
/// keyed, how its event maps to the wire, and how a timeline projects to a
/// status. Pure domain — no storage. [`InMemoryBlackboard`] needs only this;
/// [`BlackboardStore`] extends it with the SQLite storage ops.
pub trait BlackboardDomain: Send + Sync + 'static {
    /// Current value: key + attributes + state.
    type Entity: Clone + Send + Sync;
    /// Transition kind — a closed set.
    type Event: Copy + Eq + Send + Sync;

    /// Stable identity of an entity (the blackboard key), across runs.
    fn key(e: &Self::Entity) -> String;

    /// The status an entity carries when its timeline implies no transition
    /// (e.g. only a `found` sighting) — the value the consumer ships it with.
    fn initial_status(e: &Self::Entity) -> String;

    /// Wire string for an event kind.
    fn event_str(ev: Self::Event) -> String;
    /// Parse a wire string back to an event kind (the consumer's own fallback).
    fn event_from_str(s: &str) -> Self::Event;

    /// The **value ≡ timeline** projection (law #5): fold a timeline into the
    /// status it implies. `None` ⇒ keep the entity's initial/current status.
    fn project_status(timeline: &[Self::Event]) -> Option<String>;

    /// Stamp the projected `status` back onto an entity value. The framework
    /// knows the status STRING but not where the entity keeps it — so
    /// [`InMemoryBlackboard`] calls this when projecting a `view`, keeping its
    /// returned entity's status consistent with the timeline (law #5), exactly
    /// like a SQLite store's `view` reads the synced status column. A consumer
    /// whose entity has no status field returns it unchanged.
    fn with_status(e: &Self::Entity, status: &str) -> Self::Entity;
}

/// The **SQLite injection port**: the storage operations a consumer plugs into
/// [`SqliteBlackboard`]. Each is a free function over `&Connection` (the
/// orchestrator hands it the run's connection, inside a transaction for the
/// write path) — so a consumer reuses its OWN tables/queries instead of a
/// tars-imposed schema. tars never learns what the rows are.
///
/// Contract the orchestrator relies on to keep the five laws:
/// - `append_event` is idempotent on `(key, run, kind)` (a UNIQUE key), returns
///   `true` only on a NEW row, and never mutates a prior event (append-only).
/// - `sync_status` re-derives status from the timeline (the value≡timeline fold).
pub trait BlackboardStore: BlackboardDomain {
    /// Create whatever tables/indexes this store needs (idempotent). A store
    /// over EXISTING consumer tables leaves this a no-op (the default).
    fn init(conn: &Connection) -> Result<(), BbError> {
        let _ = conn;
        Ok(())
    }

    /// Set the entity's current value (no event). Idempotent on key.
    fn upsert(conn: &Connection, e: &Self::Entity) -> Result<(), BbError>;

    /// Append `ev` to `e`'s timeline for `run`, idempotently on
    /// `(key, run, ev)`. `e` is passed whole so the store can stamp any
    /// location it keeps (file/line). Returns `true` iff a new row was inserted.
    fn append_event(
        conn: &Connection,
        e: &Self::Entity,
        run: &str,
        ev: Self::Event,
        at: i64,
        version: Option<&str>,
        reason: Option<&str>,
        role: Option<&str>,
    ) -> Result<bool, BbError>;

    /// Read an entity's timeline, oldest first.
    fn read_timeline(conn: &Connection, key: &str) -> Result<Vec<Self::Event>, BbError>;

    /// Re-derive + persist the entity's status from its timeline
    /// (the value≡timeline projection).
    fn sync_status(conn: &Connection, key: &str) -> Result<(), BbError>;

    /// Read the current value of every entity matching `scope`.
    fn view(conn: &Connection, scope: &Scope) -> Result<Vec<Self::Entity>, BbError>;
}

/// The blackboard model (Doc 19 §4.1). A handle is **scoped to one run** (it
/// carries the run id), so `commit` stamps that run automatically; entities
/// PERSIST across runs (found in run 1, fixed in run 2 — same key, two events).
pub trait Blackboard: Send + Sync {
    type Entity;
    type Event: Copy + Eq;

    fn view(&self, scope: &Scope) -> Result<Vec<Self::Entity>, BbError>;
    fn timeline(&self, key: &str) -> Result<Vec<Self::Event>, BbError>;
    fn commit(&self, e: &Self::Entity, t: Transition<Self::Event>) -> Result<(), BbError>;
}
