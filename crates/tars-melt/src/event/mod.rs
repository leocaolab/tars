//! `tars_melt::event` — the read-able **E-pillar** event store (Doc 08
//! §3, Doc 17). Two durable, full-fidelity, never-sampled stores read
//! back by eval / `tars events` / debug / replay:
//!
//! - [`PipelineEventLog`] — one [`tars_types::PipelineEvent`] per
//!   `Pipeline.call` boundary. Distinct from recovery's `AgentEventLog`
//!   (trajectory truth, tars-storage) — there is no shared generic
//!   `EventStore<E>` (Doc 09 §2.2, Doc 17 Q1).
//! - [`LlmRecordStore`] — tenant-scoped CAS holding the per-call
//!   `LlmRecord` (`ChatRequest` + `ChatResponse`) referenced from a
//!   [`tars_types::PipelineEvent`] via [`tars_types::ContentRef`].
//!
//! The producing pipeline emits once into melt and never reads these
//! back; the M/L/T egress is fired independently (Doc 08 §3).

mod llm_record_store;
mod pipeline_event_log;

pub use llm_record_store::{LlmRecordStore, SqliteLlmRecordStore, SqliteLlmRecordStoreConfig};
pub use pipeline_event_log::{
    PipelineEventLog, PipelineEventQuery, SqlitePipelineEventLog, SqlitePipelineEventLogConfig,
};

use thiserror::Error;

/// Errors from the `tars_melt::event` stores (`PipelineEventLog` +
/// `LlmRecordStore`).
///
/// Three failure shapes — kept distinct so callers can decide-by-class
/// (retry a `Backend` fault, propagate `Serde` as a programmer error).
#[derive(Debug, Error)]
pub enum StoreError {
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
    /// [`StoreError::backend`] (context only) or
    /// [`StoreError::backend_source`] (context + source).
    #[error("backend: {context}")]
    Backend {
        context: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl StoreError {
    /// Backend failure with contextual message but no underlying source
    /// (e.g. an invariant we detected ourselves, like a clock or schema
    /// mismatch).
    pub fn backend(context: impl Into<String>) -> Self {
        StoreError::Backend {
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
        StoreError::Backend {
            context: context.into(),
            source: Some(source.into()),
        }
    }
}
