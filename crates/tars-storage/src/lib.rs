//! tars-storage — persistent stores for the TARS Runtime. Doc 09 + Doc 14 §6.1.
//!
//! ## Surfaces
//!
//! - **`AgentEventLog`** (M3, Doc 09 §2.2 recovery plane) — append-only
//!   trajectory event log keyed by `TrajectoryId`. Backs Runtime
//!   Trajectory replay (Doc 04 §3) and recovery-from-checkpoint.
//! - **`Blackboard`** — coordination substrate (Doc 09 §2.2).
//! - **`DurableStore`** — durable job/result board.
//!
//! The read-able observability/eval E-pillar stores (`PipelineEventLog`
//! + `LlmRecordStore`) live in `tars_melt::event`, NOT here (Doc 17 §7,
//! Doc 08 §3) — they are MELT, not recovery truth.
//!
//! Still deferred until they have a concrete consumer:
//! - `KVStore` — generic small-value persistence. Lands when
//!   BudgetMiddleware needs cross-restart token-bucket state.
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

pub mod blackboard;
mod agent_event_log;
mod durable_store;
mod error;
mod sqlite;

pub use blackboard::{
    BbError, Blackboard, BlackboardDomain, BlackboardStore, InMemoryBlackboard, Scope,
    SqliteBlackboard, Transition,
};
pub use durable_store::{
    DurableBoard, DurableStore, DurableStoreError, JOB_STATUS_DONE, JOB_STATUS_RUNNING, RawAnswer,
    ResultEventKind, ResultEventRecord, STATUS_COMPLETED, STATUS_PENDING, STATUS_SKIPPED,
    SqliteDurableStore,
};
pub use error::StorageError;
pub use agent_event_log::{AgentEventLog, EventRecord};
pub use sqlite::{
    SqliteAgentEventLog, SqliteAgentEventLogConfig, default_personal_agent_event_log_path,
    open_agent_event_log_at_path,
};
