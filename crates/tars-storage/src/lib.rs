//! tars-storage — persistent stores for the TARS Runtime. Doc 09 + Doc 14 §6.1.
//!
//! ## Surfaces
//!
//! - **`EventStore`** (M3) — append-only trajectory event log keyed by
//!   `TrajectoryId`. Backs Runtime Trajectory replay (Doc 04 §3) and
//!   recovery-from-checkpoint.
//! - **`PipelineEventStore`** (B-20.W3 enabler, Doc 17) — durable stream
//!   of one event per `Pipeline.call` boundary. Distinct from
//!   `EventStore`: different access patterns (query by tenant + time
//!   range), different ID concept (UUID per event vs sequence per
//!   trajectory). Two independent traits per Doc 17 Q1.
//! - **`BodyStore`** (B-20.W3 enabler, Doc 17 §6.1) — tenant-scoped CAS
//!   for ChatRequest / ChatResponse bodies referenced from
//!   `PipelineEvent`. Tenant-isolation enforced via `ContentRef`
//!   carrying tenant_id internally.
//!
//! Still deferred until they have a concrete consumer:
//! - `KVStore` — generic small-value persistence. Lands when
//!   BudgetMiddleware needs cross-restart token-bucket state.
//!
//! ## Why `serde_json::Value` at the `EventStore` trait boundary
//!
//! `EventStore` stays monomorphic — `Arc<dyn EventStore>` works without
//! erasing a generic. Callers serialize at the boundary; one helper
//! line hides the ceremony for typed events:
//!
//! ```ignore
//! let payload = serde_json::to_value(&my_event)?;
//! store.append(&trajectory_id, &[payload]).await?;
//! ```
//!
//! The cost vs. a generic `<E>` impl is one extra serde round-trip on
//! read; given that we're already writing JSON to SQLite (debuggable
//! via `sqlite3 events.db`), the round-trip is a feature.
//!
//! `PipelineEventStore` takes typed `PipelineEvent` directly because
//! its access patterns (query by tenant+time, subscribe by filter)
//! benefit from inline columns extracted from the typed shape; the
//! payload still goes to SQLite as JSON for debug-ability.

mod body_store;
mod error;
mod event_store;
mod pipeline_event_store;
mod sqlite;

pub use body_store::{BodyStore, SqliteBodyStore, SqliteBodyStoreConfig};
pub use error::StorageError;
pub use event_store::{EventRecord, EventStore};
pub use pipeline_event_store::{
    PipelineEventQuery, PipelineEventStore, SqlitePipelineEventStore,
    SqlitePipelineEventStoreConfig,
};
pub use sqlite::{
    default_personal_event_store_path, open_event_store_at_path, SqliteEventStore,
    SqliteEventStoreConfig,
};
