//! Runtime-layer errors.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Underlying storage failure. Caller may retry; the trajectory
    /// is still recoverable from the event store on next attempt.
    #[error("storage: {0}")]
    Storage(#[from] tars_storage::StorageError),

    /// Event payload couldn't be encoded / decoded. Almost always a
    /// programmer error in `AgentEvent`'s `Serialize` / `Deserialize`
    /// impl (e.g. a non-string-keyable map slipping in).
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// The trajectory the caller asked about doesn't exist in the
    /// store yet. Distinct from `Storage` so consumers can treat as
    /// "not started" rather than "broken".
    #[error("trajectory not found: {0}")]
    TrajectoryNotFound(String),

    /// An event was handed to `append` whose `trajectory_id` doesn't
    /// match the append target. This is a *caller* bug (an event built
    /// for the wrong trajectory), NOT an I/O fault — kept distinct from
    /// `Storage` so retry logic doesn't pointlessly retry a
    /// deterministic programming error.
    #[error("event trajectory mismatch: event targets `{event}` but append target is `{target}`")]
    TrajectoryMismatch { event: String, target: String },
}
