//! [`EventStore`] trait — append-only per-trajectory event log.
//!
//! Doc 04 §3 describes the Trajectory as an event-sourced tree; this
//! trait is the durable backing for that. Two implementors today:
//! the SQLite-backed [`crate::SqliteEventStore`] (Personal mode) and
//! a future Postgres impl (Team mode, Doc 14 M6).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use tars_types::TrajectoryId;

use crate::error::StorageError;

/// One event as the store sees it. The store assigns
/// `(sequence_no, timestamp_ms)` on append; callers don't pre-compute.
///
/// `payload` is the verbatim JSON the caller appended. Deserialize
/// to your event type at the call site:
///
/// ```ignore
/// let typed: AgentEvent = serde_json::from_value(record.payload)?;
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    pub trajectory_id: TrajectoryId,
    /// 1-indexed, monotonic per-trajectory. Gaps shouldn't appear under
    /// normal operation; if they do it indicates a partial transaction
    /// (a bug in the impl, not just expected loss).
    pub sequence_no: u64,
    /// Wall-clock at append time, ms since UNIX epoch. Diagnostics
    /// only — ordering is via `sequence_no`.
    pub timestamp_ms: i64,
    pub payload: serde_json::Value,
}

/// Append-only per-trajectory event log.
///
/// Concurrency: implementations must serialise per-trajectory writes
/// so `sequence_no` stays gap-free + monotonic. Concurrent writes to
/// **different** trajectories are expected to make progress in
/// parallel where the backend allows it.
///
/// All methods are `async` so impls can offload SQLite / Postgres
/// blocking work to a runtime executor without forcing every caller
/// into a thread pool.
#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    /// Append a batch of payloads to `trajectory_id`. Returns the
    /// `sequence_no` of the **last** event written. Empty `payloads`
    /// is a no-op that returns the current high-water (or 0 if none).
    async fn append(
        &self,
        trajectory_id: &TrajectoryId,
        payloads: &[serde_json::Value],
    ) -> Result<u64, StorageError>;

    /// Read every event for `trajectory_id` in `sequence_no` order.
    /// Returns an empty `Vec` (NOT `NotFound`) when the trajectory has
    /// no events — "haven't appended yet" is a normal state for a
    /// just-created trajectory.
    async fn read_all(
        &self,
        trajectory_id: &TrajectoryId,
    ) -> Result<Vec<EventRecord>, StorageError>;

    /// Read events with `sequence_no > since`, in order. Used for
    /// replay-from-checkpoint and incremental tailing. `since = 0`
    /// is equivalent to `read_all`.
    async fn read_since(
        &self,
        trajectory_id: &TrajectoryId,
        since: u64,
    ) -> Result<Vec<EventRecord>, StorageError>;

    /// Highest `sequence_no` recorded for `trajectory_id`, or 0 if
    /// the trajectory has no events yet. Cheap — useful to skip a
    /// `read_all` when the caller already has events buffered up to
    /// some point.
    async fn high_water(
        &self,
        trajectory_id: &TrajectoryId,
    ) -> Result<u64, StorageError>;

    /// List every trajectory that has at least one event. Used for
    /// admin / recovery scans. Order is unspecified.
    async fn list_trajectories(&self) -> Result<Vec<TrajectoryId>, StorageError>;
}
