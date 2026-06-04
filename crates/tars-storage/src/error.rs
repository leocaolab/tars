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
    ///
    /// Carries a human-readable `context` describing the operation that
    /// failed plus an optional `source` so the original error chain
    /// (e.g. the `rusqlite::Error`) is preserved for
    /// `std::error::Error::source()` walkers and `{:?}` rendering rather
    /// than being flattened into a string. Build with
    /// [`StorageError::backend`] (context only) or
    /// [`StorageError::backend_source`] (context + source).
    #[error("backend: {context}")]
    Backend {
        context: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Caller asked for a trajectory that has no rows. Distinct from
    /// `Backend` so consumers can treat it as "fresh start" rather
    /// than "the store is broken".
    #[error("trajectory not found: {0}")]
    NotFound(String),
}

impl StorageError {
    /// Backend failure with contextual message but no underlying source
    /// (e.g. an invariant we detected ourselves, like a clock or schema
    /// mismatch).
    pub fn backend(context: impl Into<String>) -> Self {
        StorageError::Backend {
            context: context.into(),
            source: None,
        }
    }

    /// Backend failure wrapping an underlying error as the source so the
    /// chain is preserved. `context` should name the operation; the
    /// source carries the original error.
    pub fn backend_source(
        context: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        StorageError::Backend {
            context: context.into(),
            source: Some(source.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_render_with_helpful_prefix() {
        let err = StorageError::backend("disk full");
        assert!(err.to_string().contains("backend"));
        assert!(err.to_string().contains("disk full"));
    }

    #[test]
    fn backend_source_is_preserved_in_chain() {
        use std::error::Error as _;
        let io = std::io::Error::other("underlying boom");
        let err = StorageError::backend_source("writing row", io);
        // Display shows the context; source() exposes the original.
        assert!(err.to_string().contains("writing row"));
        let src = err.source().expect("source preserved");
        assert!(src.to_string().contains("underlying boom"));
    }
}
