//! Reads why a task failed: the one read a decision path may make of a sealed journal.
//!
//! The reason is not on the task's row; it is the producer's own `Failed` event in the
//! journal, so once the journal is sealed, asking why costs one row-group read. Every
//! caller that turns a `Failed` row into an explanation goes through here.
use tars_types::{Event, EventKind, JournaledReason, TaskId};

use crate::durable::DurableStoreError;

/// Returns the reason of the last `Failed` event in `events`, a task's journal in `seq`
/// order.
pub(crate) fn last_failure(events: Vec<Event>) -> Option<JournaledReason> {
    events.into_iter().rev().find_map(|e| match e.kind {
        EventKind::Failed { reason } => Some(reason),
        _ => None,
    })
}

/// Why a `Failed` task's reason cannot be told. Its `Display` is the wording every
/// reader gives; what a reader does with it is the reader's.
#[derive(Debug, thiserror::Error)]
pub enum UntoldFailure {
    /// A `Failed` row with no `Failed` event beside it: a gap in the record. Every write
    /// that fails a task appends that event, so only a lost event leaves one: a
    /// [`MemDurableStore`](crate::durable::MemDurableStore) dropping its oldest past its cap, or a
    /// trajectory edited outside the durable store.
    #[error("task `{id}` is Failed with no recorded reason")]
    Unrecorded { id: TaskId },
    /// The journal could not be read, which is not "none was recorded".
    #[error("task `{id}` is Failed and reading its reason failed")]
    Unreadable {
        id: TaskId,
        #[source]
        source: DurableStoreError,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::durable::Writer;

    use crate::durable::journal::{MemTrajectoryLog, TrajectoryLog};
    use serde_json::json;
    use tars_types::{AgentKey, JobId, Reason, TaskSpec};

    use super::*;
    use crate::durable::SealJournals;
    use crate::durable::{DurableStore, DurableStoreRead, FiberStore, MemDurableStore};

    /// Claims `t` and fails it for good with `reason`.
    async fn failed_task(store: &MemDurableStore, reason: &str) {
        store
            .enqueue(
                &TaskSpec::new(
                    TaskId::new("t"),
                    JobId::new("j"),
                    AgentKey::new("k"),
                    json!("t"),
                ),
                0,
            )
            .await
            .unwrap();
        let claim = store
            .claim_ready("r", 0, 10)
            .await
            .unwrap()
            .map(|c| Writer::Claim(c.token))
            .expect("the task is ready");
        let reason = JournaledReason::from(&Reason::Execution(reason.into()));
        store
            .fail(&TaskId::new("t"), claim, 1, reason, 1)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_failed_row_whose_failed_event_was_dropped_is_unrecorded() {
        let store = MemDurableStore::new(tars_types::TenantId::new("test"));
        failed_task(&store, "boom").await;
        store
            .enqueue(
                &TaskSpec::new(
                    TaskId::new("filler"),
                    JobId::new("j"),
                    AgentKey::new("k"),
                    json!(null),
                ),
                2,
            )
            .await
            .unwrap();
        for n in 0..MemDurableStore::TRAJECTORY_CAP {
            store
                .append_delta(Writer::Unclaimed, &TaskId::new("filler"), json!(n), 0, 2)
                .await
                .unwrap();
        }
        assert_eq!(
            store.get(&TaskId::new("t")).await.unwrap().unwrap().status,
            tars_types::Status::Failed,
            "premise: the row still says Failed"
        );

        let untold = store.failure_reason(&TaskId::new("t")).await.unwrap_err();

        assert!(
            matches!(&untold, UntoldFailure::Unrecorded { id } if id.as_str() == "t"),
            "{untold:?}"
        );
    }

    #[tokio::test]
    async fn a_journal_that_cannot_be_read_is_unreadable_and_carries_the_store_error() {
        let journal = Arc::new(MemTrajectoryLog::new());
        let store =
            MemDurableStore::new(tars_types::TenantId::new("test")).with_journal(journal.clone());
        failed_task(&store, "the disk said no").await;
        let sealed = store
            .seal_tasks(&[TaskId::try_from("t".to_string()).unwrap()], 1_000)
            .await
            .unwrap();
        let readable = store
            .failure_reason(&TaskId::new("t"))
            .await
            .unwrap()
            .record();
        assert!(readable.contains("the disk said no"), "premise: {readable}");

        journal.remove(&sealed[0].file).await.unwrap();

        let untold = store.failure_reason(&TaskId::new("t")).await.unwrap_err();
        let UntoldFailure::Unreadable { id, source } = &untold else {
            panic!("a failed read is not an unrecorded reason: {untold:?}");
        };
        assert_eq!(id.as_str(), "t");
        assert!(
            tars_utils::display_chain(source).contains("is not stored"),
            "the store's own words: {source}"
        );
    }
}
