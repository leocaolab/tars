//! tars-storage — the trajectory event log, and the SQLite behind it.
//!
//! One surface: **`AgentEventLog`**, an append-only event log keyed by
//! `TrajectoryId`. It is the record of what a run did, in order, and it is
//! what `tars trajectory` and `tars_melt::run_report` read back.
//!
//! The read-able observability stores (`PipelineEventLog`, `LlmRecordStore`)
//! live in `tars_melt::event`, NOT here — they are telemetry, and this is the
//! recovery record. The distinction is worth keeping even now that only one
//! side has a writer in this repo.
//!
//! ## Why `serde_json::Value` at the trait boundary
//!
//! `AgentEventLog` stays monomorphic, so `Arc<dyn AgentEventLog>` works
//! without erasing a generic. Callers serialize at the boundary; one helper
//! line hides the ceremony for typed events:
//!
//! ```ignore
//! let payload = serde_json::to_value(&my_event)?;
//! store.append(&trajectory_id, &[payload]).await?;
//! ```
//!
//! The cost against a generic `<E>` impl is one extra serde round-trip on
//! read. Given that we are already writing JSON into SQLite — readable with
//! `sqlite3 events.db` and no tooling — the round-trip is a feature.

mod agent_event_log;
mod error;
mod sqlite;

pub use agent_event_log::{AgentEventLog, EventRecord};
pub use error::StorageError;
pub use sqlite::{
    SqliteAgentEventLog, SqliteAgentEventLogConfig, default_personal_agent_event_log_path,
    open_agent_event_log_at_path,
};
