//! Typed errors for the durable runtime driver.
//!
//! Errors are carried typed (`#[from]` / `#[source]`) so callers branch on
//! the variant, never `.to_string().contains(..)`. The storage layer's own
//! [`DurableStoreError`] (which itself carries a typed `BbError` /
//! `rusqlite::Error`) bubbles up transparently.

use tars_storage::DurableStoreError;

use crate::worker::WorkerError;

/// Everything that can go wrong submitting or driving a durable job.
#[derive(Debug, thiserror::Error)]
pub enum DurableError {
    /// A read/write on the always-on durable store failed. Carries the
    /// typed [`DurableStoreError`].
    #[error("durable store: {0}")]
    Store(#[from] DurableStoreError),

    /// (De)serialising a persisted value (`AgentMessage`, `Usage`, `Plan`)
    /// to/from its JSON column — the result-side decode of the store's
    /// opaque payload. Carries the typed serde error.
    #[error("durable encode/decode: {0}")]
    Serde(#[from] serde_json::Error),

    /// A worker invocation failed while driving a step. Carries the
    /// worker's own typed error (boxed — `WorkerError` is large, and
    /// keeping `DurableError` small keeps every `Result<_, DurableError>`
    /// cheap to return).
    #[error("worker (step `{step_id}`): {source}")]
    Worker {
        step_id: String,
        #[source]
        source: Box<WorkerError>,
    },

    /// A worker returned a non-`PartialResult` envelope — the answer store
    /// only checkpoints `AgentMessage::PartialResult`.
    #[error("worker (step `{step_id}`) returned a non-PartialResult message: {got}")]
    UnexpectedOutput { step_id: String, got: String },

    /// The plan failed `Plan::validate` before any step ran.
    #[error("invalid plan: {0}")]
    InvalidPlan(String),

    /// A step's `worker_role` has no registered worker (and no default).
    /// Surfaced up front, before any step runs.
    #[error("no worker registered for role `{role}` (step `{step_id}`)")]
    NoWorkerForRole { role: String, step_id: String },

    /// No job row exists for this id (never submitted, or a different
    /// store).
    #[error("job `{0}` not found")]
    JobNotFound(String),

    /// The DAG driver made a full pass with nothing ready to run and not
    /// every step resolved — steps whose deps never became present.
    /// `Plan::validate` rules out the cycle that would normally cause this,
    /// so it is a defensive guard, not an expected state.
    #[error("plan stalled: steps {0:?} never became ready")]
    Stalled(Vec<String>),
}
