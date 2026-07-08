//! Typed errors for the durable runtime.
//!
//! Errors are carried typed (`#[from]` / `#[source]`) so callers branch
//! on the variant, never `.to_string().contains(..)`. The blackboard's
//! own `BbError` (which itself boxes a consumer storage error
//! downcastably) bubbles up transparently.

use tars_runtime::WorkerError;
use tars_storage::BbError;

/// Everything that can go wrong submitting or driving a durable job.
#[derive(Debug, thiserror::Error)]
pub enum DurableError {
    /// A write through the always-on blackboard failed (answer + event
    /// + job-state transaction). Carries the typed [`BbError`].
    #[error("durable store: {0}")]
    Store(#[from] BbError),

    /// A direct SQLite read/write on the durable store's own tables
    /// (outside the blackboard `commit` path — e.g. reading the answer
    /// set, loading a plan).
    #[error("durable sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// (De)serialising a persisted value (`AgentMessage`, `Usage`,
    /// `Plan`) to/from its JSON column.
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

    /// A worker returned a non-`PartialResult` envelope — the answer
    /// store only checkpoints `AgentMessage::PartialResult`.
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

    /// The DAG driver made a full pass with nothing ready to run and
    /// not every step resolved — steps whose deps never became present.
    /// `Plan::validate` rules out the cycle that would normally cause
    /// this, so it is a defensive guard, not an expected state.
    #[error("plan stalled: steps {0:?} never became ready")]
    Stalled(Vec<String>),
}
