//! The blackboard MODEL (Doc 19 §4.1) — **generic framework infrastructure**.
//!
//! A blackboard is a keyed set of **entities**; each entity has a current value
//! and an **append-only timeline of self-describing events**. Two operations —
//! `view` + `commit` — and five laws. It is the single framework-owned write
//! path: a step appends ITS event at source, so a finding's timeline can never
//! end up holding only the last event (the bug the model exists to kill).
//!
//! tars supplies the whole framework: the [`Blackboard`] trait, BOTH backings
//! ([`SqliteBlackboard`] + [`MemBlackboard`]), and the [`BlackboardCodec`] seam.
//! A **consumer supplies only a `BlackboardCodec`** — how to key / (de)serialize
//! its entity, map its event kind to the wire, and project the value≡timeline
//! status. The consumer writes NO storage code and owns NO laws: it declares its
//! domain, tars runs the model. (Reference: A.R.C. binds `Entity = Finding`,
//! `Event = EventKind` via one `FindingCodec` — and reuses these backings.)
//!
//! ## The five laws (Doc 19 §4.1)
//! 1. **Append-only** — `commit` never deletes or mutates a prior event.
//! 2. **Atomic** — value-set + event-append is one unit per commit.
//! 3. **Idempotent on `(Key, run, kind)`** — a re-committed transition is absorbed.
//! 4. **Read-your-writes** — after `commit(e, ..)`, `view(scope ∋ e)` sees it.
//! 5. **Value ≡ timeline** — status is a projection of the event log; the log is
//!    the truth.
//!
//! The `Scope` and provenance dimensions are framework-universal (every
//! blackboard has a status and a birth run), so they are concrete here — the
//! consumer does not reinvent them.

mod memory;
mod sqlite;

pub use memory::MemBlackboard;
pub use sqlite::SqliteBlackboard;

#[cfg(test)]
mod laws;

/// A read selector over entities — the framework-universal dimensions.
#[derive(Debug, Clone)]
pub enum Scope {
    /// Every entity on the board.
    All,
    /// Entities whose current status is one of these.
    WithStatus(Vec<String>),
    /// Entities first seen in a given run.
    FirstSeenIn(String),
}

/// One transition appended to an entity's timeline (Doc 19 §4.1). `version` is
/// the state-of-world it happened in (a commit sha, say); `at` is captured AT
/// the step, never at a run-end batch.
#[derive(Debug, Clone)]
pub struct Transition<Ev> {
    pub kind: Ev,
    pub at: i64,
    pub version: Option<String>,
    pub reason: Option<String>,
}

impl<Ev> Transition<Ev> {
    pub fn new(kind: Ev, at: i64, version: Option<String>) -> Self {
        Self { kind, at, version, reason: None }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Blackboard failure. `Codec` carries the consumer's (de)serialization error as
/// a message at the model boundary.
#[derive(Debug, thiserror::Error)]
pub enum BbError {
    #[error("blackboard sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("blackboard codec: {0}")]
    Codec(String),
}

/// What a consumer supplies to use the framework backings: how to key /
/// (de)serialize its entity, map its event kind to/from the wire, and project
/// the value≡timeline status. tars owns storage; this owns the domain.
///
/// The consumer's `Key` is projected to a `String` (the backing's primary key) —
/// any stable identity (a fingerprint, a UUID) maps cleanly. `Event` is a small
/// `Copy` enum the consumer maps to/from a wire string.
pub trait BlackboardCodec: Send + Sync + 'static {
    /// The current value: key + attributes + state.
    type Entity: Clone + Send + Sync;
    /// The transition kind — a closed set.
    type Event: Copy + Eq + Send + Sync;

    /// Stable identity of an entity (the blackboard Key), unchanged across runs.
    fn key(e: &Self::Entity) -> String;

    /// Serialize the entity value; the backing stores it opaquely.
    fn encode(e: &Self::Entity) -> Result<String, BbError>;
    /// Inverse of [`Self::encode`].
    fn decode(s: &str) -> Result<Self::Entity, BbError>;

    /// The status an entity carries when its timeline implies no transition
    /// (e.g. only a `found` sighting) — the value the consumer ships it with.
    fn initial_status(e: &Self::Entity) -> String;

    /// Wire string for an event kind (stored in the timeline).
    fn event_str(ev: Self::Event) -> String;
    /// Parse a wire string back to an event kind (the consumer's own fallback
    /// for an unknown token — never panic, never drop the row).
    fn event_from_str(s: &str) -> Self::Event;

    /// The **value ≡ timeline** projection (law #5): fold a timeline of event
    /// kinds into the status it implies. `None` ⇒ no transition yet, keep the
    /// entity's initial/current status. Both backings call THIS, so they agree.
    fn project_status(timeline: &[Self::Event]) -> Option<String>;
}

/// The blackboard model (Doc 19 §4.1). A handle is **scoped to one run** (it
/// carries the run id), so `commit` stamps that run automatically; entities
/// PERSIST across runs (found in run 1, fixed in run 2 — same key, two events).
pub trait Blackboard: Send + Sync {
    /// Current value: key + attributes + state.
    type Entity;
    /// Transition kind — a closed set.
    type Event: Copy + Eq;

    /// Project the current VALUE of every entity matching `scope`.
    fn view(&self, scope: &Scope) -> Result<Vec<Self::Entity>, BbError>;

    /// Project the append-only TIMELINE of one entity (by key), oldest first.
    fn timeline(&self, key: &str) -> Result<Vec<Self::Event>, BbError>;

    /// Append `t` to `e`'s timeline and set `e`'s value, atomically (both or
    /// neither). Idempotent on `(key, run, kind)`.
    fn commit(&self, e: &Self::Entity, t: Transition<Self::Event>) -> Result<(), BbError>;
}
