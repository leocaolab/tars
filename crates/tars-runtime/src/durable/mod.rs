//! The durable agent-task runtime driver (Doc: `durable-agent-runtime.md`).
//!
//! A durable execution layer where **the persisted step-result store IS the
//! checkpoint**: an agent task's DAG is re-driven from a per-step answer
//! store, so resume is a *memoized re-run* — a step whose answer is already
//! stored is skipped and the LLM is never re-called.
//!
//! ## Layering
//!
//! - The **persistence + SQL** live in [`tars_storage::SqliteDurableStore`]
//!   (the payload-agnostic [`tars_storage::DurableStore`] contract) — the
//!   only place raw `rusqlite` touches this feature.
//! - The **typed domain** lives here: [`StepAnswer`] (message as a concrete
//!   [`crate::AgentMessage`]) + the [`AnswerStore`] adapter that owns the
//!   result-side JSON decode, and [`DurableScheduler`] the memoized-re-run
//!   driver. This module gains NO sqlite/rusqlite dependency.
//!
//! ## Critical invariant — durability is independent of observability
//!
//! The durability store is **always-on** and built on its OWN connection.
//! It NEVER layers correctness on `tars_storage::AgentEventLog` /
//! `Runtime::append`, which are the *off-able* observability sink
//! (`StoreScope::Off`, `ARC_TARS_EVENTS_OFF`). With observability events
//! fully off/absent, a step's answer, the job's state, and `result_events`
//! still persist and a job still resumes. (Regression:
//! `events_off_still_persists_and_resumes`.)

mod error;
mod scheduler;
mod store;

pub use error::DurableError;
pub use scheduler::DurableScheduler;
pub use store::{AnswerStore, StepAnswer};

// Re-export the storage-layer value types that appear on this module's
// public surface, so callers name the durable API from one place.
pub use tars_storage::{
    JOB_STATUS_DONE, JOB_STATUS_RUNNING, ResultEventKind, ResultEventRecord,
};
