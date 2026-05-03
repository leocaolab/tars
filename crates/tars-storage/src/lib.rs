//! tars-storage — persistent stores for the TARS Runtime. Doc 09 + Doc 14 §6.1.
//!
//! M3 scope (this commit): **EventStore only** — append-only per-
//! trajectory event log. Backs the Runtime's Trajectory replay (Doc 04
//! §3) and is the durability primitive recovery-from-checkpoint relies
//! on.
//!
//! Out of scope until they have a concrete consumer (per `defer >
//! delete > implement`):
//! - `ContentStore` — large-blob content addressed by hash. Lands when
//!   image / long-context refs need to live outside the event JSON.
//! - `KVStore` — generic small-value persistence. Lands when
//!   BudgetMiddleware needs cross-restart token-bucket state. The
//!   existing `tars-cache::SqliteCacheRegistry` will likely refactor
//!   onto it at that point (it's the same SQLite scaffolding pattern).
//!
//! ## Why `serde_json::Value` at the trait boundary, not `<E>`
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

mod error;
mod event_store;
mod sqlite;

pub use error::StorageError;
pub use event_store::{EventRecord, EventStore};
pub use sqlite::{
    default_personal_event_store_path, open_event_store_at_path, SqliteEventStore,
    SqliteEventStoreConfig,
};
