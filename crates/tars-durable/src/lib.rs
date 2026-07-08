//! tars-durable — the durable agent-task runtime (Doc: `durable-agent-runtime.md`).
//!
//! A durable execution layer where **the persisted step-result store IS
//! the checkpoint**: an agent task's DAG is re-driven from a per-step
//! answer store, so resume is a *memoized re-run* — a step whose answer
//! is already stored is skipped and the LLM is never re-called.
//!
//! ## Scope (M0 + M1)
//!
//! - **M0 — the always-on durability store** ([`store`]): [`DurableStore`]
//!   holds three tables (`answers` = the [`AnswerStore`]; `result_events`
//!   = an append-only monotonic-seq log; `jobs` = the status of record)
//!   in its OWN sqlite file. Every step is checkpointed through ONE
//!   [`tars_storage::SqliteBlackboard`] `commit` transaction — atomic
//!   `{answer + event + job-state}`.
//! - **M1 — the memoized-re-run driver** ([`scheduler`]):
//!   [`DurableScheduler`] derives readiness/skip entirely from the answer
//!   store and executes un-done steps via the existing `Worker::run` seam.
//!
//! ## Critical invariant — durability is independent of observability
//!
//! The durability store is **always-on** and built on its OWN connection.
//! It NEVER layers correctness on `tars_storage::EventStore` /
//! `Runtime::append`, which are the *off-able* observability sink
//! (`StoreScope::Off`, `ARC_TARS_EVENTS_OFF`, `CONCER_TARS_EVENTS_OFF`).
//! With observability events fully off/absent, a step's answer, the
//! job's state, and `result_events` still persist and a job still
//! resumes. (Regression: `events_off_still_persists_and_resumes`.)
//!
//! ## Not in this round (M2–M5)
//!
//! JobManager + `reconcile_on_open` + persisted cancel (M2); the
//! `delivery` cursor/ack outbox worker (M3); the ephemeral `EventBus` +
//! streaming + Zustand projection (M4); the concer CUJ-1 wiring (M5).

pub mod error;
pub mod scheduler;
pub mod store;

pub use error::DurableError;
pub use scheduler::DurableScheduler;
pub use store::{
    DurableBoard, DurableStore, ResultEventKind, ResultEventRecord, StepAnswer,
    JOB_STATUS_DONE, JOB_STATUS_RUNNING,
};

/// The persistent, no-TTL, step-identity-keyed result store IS the
/// checkpoint — the one genuinely new abstraction (§14). Named alias for
/// [`DurableStore`], which owns the `answers` table.
pub use store::DurableStore as AnswerStore;
