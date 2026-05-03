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
}
