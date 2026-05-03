//! Storage-layer errors.
//!
//! Three failure shapes — keep them distinct so callers can
//! decide-by-class (e.g. an `EventStore` consumer might retry on
//! `Backend` but propagate `Serde` immediately as a programmer error).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    /// JSON encoding / decoding of an event payload failed. Almost
    /// always a programmer error (the event type's `Serialize` /
    /// `Deserialize` impl produced something the storage round-trip
    /// can't handle).
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// Underlying storage failure (SQLite I/O, schema migration,
    /// constraint violation). Caller logs + may retry.
    #[error("backend: {0}")]
    Backend(String),

    /// Caller asked for a trajectory that has no rows. Distinct from
    /// `Backend` so consumers can treat it as "fresh start" rather
    /// than "the store is broken".
    #[error("trajectory not found: {0}")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_render_with_helpful_prefix() {
        let err = StorageError::Backend("disk full".into());
        assert!(err.to_string().contains("backend"));
        assert!(err.to_string().contains("disk full"));
    }
}
