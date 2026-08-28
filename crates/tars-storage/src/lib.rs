//! tars-storage — the database, and the stores built on it.
//!
//! ## Surfaces
//!
//! - [`Db`] — the handle every store is built from, and the one type here that
//!   names a driver. Opening and migrating go through it; see its module docs
//!   for what it abstracts and what it deliberately does not.
//! - [`AgentEventLog`] — append-only trajectory event log keyed by
//!   `TrajectoryId`, backing trajectory replay and recovery-from-checkpoint.
//! - [`query_read_only`] — one read-only statement against a database this
//!   crate does not own, with no migrator.
//!
//! The observability stores (`PipelineEventLog` + `LlmRecordStore`) live in
//! `tars_melt::event`, not here: they are MELT, not recovery truth.
//!
//! ## Why `serde_json::Value` at the `AgentEventLog` trait boundary
//!
//! `AgentEventLog` stays monomorphic — `Arc<dyn AgentEventLog>` works
//! without erasing a generic. Callers serialize at the boundary; one
//! helper line hides the ceremony for typed events:
//!
//! ```ignore
//! let payload = serde_json::to_value(&my_event)?;
//! store.append(&trajectory_id, &[payload]).await?;
//! ```
//!
//! The cost vs. a generic `<E>` impl is one extra serde round-trip on
//! read; given that we're already writing JSON to SQLite (debuggable
//! via `sqlite3 events.db`), the round-trip is a feature.

mod agent_event_log;
mod db;
mod error;
mod read_only_query;
mod sqlite;

pub use agent_event_log::{AgentEventLog, EventRecord};
pub use db::{Db, DbError};
pub use error::StorageError;
pub use read_only_query::{Cell, QueryResult, ReadOnlyQueryError, query_read_only};
pub use sqlite::{
    SqliteAgentEventLog, SqliteAgentEventLogConfig, default_personal_agent_event_log_path,
    open_agent_event_log_at_path,
};
