//! Seals finished journals on the store's own schedule, so whoever drives tasks over the store
//! neither knows sealing exists nor waits for it.
//!
//! The schedule is a tokio task spawned on the handle the opener gives; it starts no thread of
//! its own, and a seal's file writes go through the journal store as they always do. Sealing
//! changes no answer a reader gets, only where the rows live, so a pass that fails costs a
//! larger live journal until the next one and nothing else.
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tars_types::{Millis, TaskId};
use tars_utils::display_chain;
use tokio::runtime::Handle;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::{JoinError, JoinHandle};

use crate::durable::{DurableStoreError, SealedJournal};

/// The four steps of moving finished journals into sealed files, which only the store's own
/// sealing schedule and the store's contract tests call.
#[async_trait]
pub trait SealJournals: Send + Sync {
    /// Returns up to `max_tasks` tasks whose journal may be sealed now (S(T) holds and
    /// the journal still has rows), cut so their journals together weigh at most
    /// `max_bytes`; the first always goes in.
    async fn seal_candidates(
        &self,
        max_tasks: usize,
        max_bytes: u64,
        now: Millis,
    ) -> Result<Vec<TaskId>, DurableStoreError>;

    /// Moves these finished tasks' whole journals into one sealed file, one row group
    /// each, if S(T) holds for every one ([`Unsealable`](crate::durable::Unsealable) lists the clauses).
    ///
    /// The file is named and registered `staged` first, then written, and becomes
    /// visible only in ONE commit that re-checks S(T) and the newest `seq` of every
    /// task, marks it `live`, indexes the tasks and deletes their journal rows — so a
    /// row is always in exactly one place. If any re-check fails the whole batch is
    /// left as it was; the staged file stays for [`remove_staged`](Self::remove_staged).
    /// After this, [`trajectory`](crate::durable::DurableStoreRead::trajectory) reads the file and appending is
    /// [`DurableStoreError::JournalSealed`].
    async fn seal_tasks(
        &self,
        task_ids: &[TaskId],
        now: Millis,
    ) -> Result<Vec<SealedJournal>, DurableStoreError>;

    /// Deletes every file still `staged` that was registered at or before
    /// `registered_by`, left by a seal that crashed or was refused after naming it. The
    /// registrations go first, in one transaction, then the files: a seal still
    /// writing one of them then finds its registration gone and its whole batch is
    /// refused at commit, so a `live` registration never names a deleted file. A
    /// crash in between leaves only unregistered files, for
    /// [`remove_unregistered`](Self::remove_unregistered). Returns how many.
    async fn remove_staged(&self, registered_by: Millis) -> Result<usize, DurableStoreError>;

    /// Deletes every file in the journal store that has no registration. A seal
    /// registers its file before writing it, so such a file is always a leftover —
    /// of a [`remove_staged`](Self::remove_staged) that died between its two steps —
    /// and needs no grace. Lists the whole store, so it is for start-up, not every
    /// pass. Returns how many.
    async fn remove_unregistered(&self) -> Result<usize, DurableStoreError>;
}

/// How often the store seals, and how much at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalSealPolicy {
    pub max_tasks: usize,
    /// Journal bytes per sealed file, at most. It also bounds how long a seal's commit holds
    /// the store's write lock; a single larger journal is still sealed, alone.
    pub max_bytes: u64,
    /// How long a `staged` file may sit before it is taken for a crashed seal's leftover and
    /// deleted. It must be longer than any seal takes: a seal still writing its file when the
    /// grace runs out loses it and is refused.
    pub grace: Millis,
    /// The wait between two passes.
    pub every: Duration,
}

impl Default for JournalSealPolicy {
    fn default() -> Self {
        Self {
            max_tasks: 64,
            max_bytes: 4 * 1024 * 1024,
            grace: 10 * 60 * 1000,
            every: Duration::from_secs(1),
        }
    }
}

/// What a store needs to seal on its own: when and how much, the time, and where to run.
pub struct Sealing {
    pub policy: JournalSealPolicy,
    /// The clock the store's leases are written by, in epoch millis. A task is sealed only once
    /// its lease has run out by this clock, so a different one (a test's fake clock beside the
    /// wall clock) seals tasks whose runner is still inside them.
    pub clock: Arc<dyn Fn() -> Millis + Send + Sync>,
    /// The runtime the schedule is spawned on. It must outlive the store: a schedule spawned on
    /// a runtime that shuts down stops with it.
    pub rt: Handle,
}

/// How the store's sealing has gone lately.
#[derive(Debug, Clone)]
pub enum SealHealth {
    Healthy,
    /// `streak` passes in a row have failed; `last` is the newest failure as it was raised.
    Failing {
        streak: NonZeroU32,
        last: Arc<DurableStoreError>,
    },
}

/// Why the last seal before a store closed did not complete.
#[derive(Debug, thiserror::Error)]
pub enum StopSealingError {
    #[error("the last seal pass before closing failed")]
    LastPass(#[source] DurableStoreError),
    #[error("the sealing task ended without reporting")]
    Lost(#[source] JoinError),
}

/// What a pass changed in the health, for the one log line a change deserves.
#[derive(Debug, PartialEq, Eq)]
enum HealthChange {
    Unchanged,
    StartedFailing,
    Recovered,
}

/// Returns the health after a pass that `failed` (or not), and how it changed.
fn after_pass(streak: Option<NonZeroU32>, failed: bool) -> (Option<NonZeroU32>, HealthChange) {
    match (streak, failed) {
        (None, false) => (None, HealthChange::Unchanged),
        (None, true) => (Some(NonZeroU32::MIN), HealthChange::StartedFailing),
        (Some(_), false) => (None, HealthChange::Recovered),
        (Some(n), true) => (Some(n.saturating_add(1)), HealthChange::Unchanged),
    }
}

/// A store's running seal schedule: the way to stop it, and its health.
pub(crate) struct Sealer {
    running: Mutex<Option<Running>>,
    health: watch::Receiver<SealHealth>,
}

struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), DurableStoreError>>,
}

impl Sealer {
    /// Spawns the schedule over `archive`. Dropping the `Sealer` ends it after the pass in
    /// flight, with no final pass; [`stop`](Self::stop) ends it with one.
    pub(crate) fn spawn<A: SealJournals + 'static>(archive: A, sealing: Sealing) -> Self {
        let (stop, stopped) = oneshot::channel();
        let (report, health) = watch::channel(SealHealth::Healthy);
        let task = sealing
            .rt
            .spawn(run(archive, sealing.policy, sealing.clock, stopped, report));
        Self {
            running: Mutex::new(Some(Running { stop, task })),
            health,
        }
    }

    pub(crate) fn health(&self) -> SealHealth {
        self.health.borrow().clone()
    }

    /// Stops the schedule after one last pass and returns that pass's result. A second call
    /// finds nothing running and returns `Ok`.
    pub(crate) async fn stop(&self) -> Result<(), StopSealingError> {
        let Some(Running { stop, task }) = self.running.lock().await.take() else {
            return Ok(());
        };
        // [arc:intentional-handle] reason: a send fails only when the schedule already ended,
        // and awaiting its task below reports how it ended.
        let _ = stop.send(());
        task.await
            .map_err(StopSealingError::Lost)?
            .map_err(StopSealingError::LastPass)
    }
}

async fn run<A: SealJournals>(
    archive: A,
    policy: JournalSealPolicy,
    clock: Arc<dyn Fn() -> Millis + Send + Sync>,
    mut stopped: oneshot::Receiver<()>,
    report: watch::Sender<SealHealth>,
) -> Result<(), DurableStoreError> {
    let first = archive.remove_unregistered().await.map(drop);
    record(&report, first);
    loop {
        tokio::select! {
            asked = &mut stopped => {
                return match asked {
                    Ok(()) => seal_once(&archive, &policy, clock()).await,
                    // The store was dropped without closing: whatever is left is sealed on
                    // its next open.
                    Err(_) => Ok(()),
                };
            }
            () = tokio::time::sleep(policy.every) => {
                let pass = seal_once(&archive, &policy, clock()).await;
                record(&report, pass);
            }
        }
    }
}

/// Clears what an earlier seal left staged past its grace, then seals one batch.
async fn seal_once<A: SealJournals>(
    archive: &A,
    policy: &JournalSealPolicy,
    now: Millis,
) -> Result<(), DurableStoreError> {
    archive.remove_staged(now - policy.grace).await?;
    let batch = archive
        .seal_candidates(policy.max_tasks, policy.max_bytes, now)
        .await?;
    archive.seal_tasks(&batch, now).await?;
    Ok(())
}

fn record(report: &watch::Sender<SealHealth>, pass: Result<(), DurableStoreError>) {
    let streak = match &*report.borrow() {
        SealHealth::Healthy => None,
        SealHealth::Failing { streak, .. } => Some(*streak),
    };
    let (streak, change) = after_pass(streak, pass.is_err());
    match change {
        HealthChange::Unchanged => {}
        HealthChange::StartedFailing => {
            if let Err(e) = &pass {
                tracing::warn!(error = %display_chain(e), "sealing finished journals failed; the live journal grows until a pass succeeds");
            }
        }
        HealthChange::Recovered => tracing::info!("sealing finished journals recovered"),
    }
    let health = match (streak, pass) {
        (Some(streak), Err(e)) => SealHealth::Failing {
            streak,
            last: Arc::new(e),
        },
        _ => SealHealth::Healthy,
    };
    report.send_replace(health);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: NonZeroU32 = NonZeroU32::MIN.saturating_add(1);

    #[test]
    fn a_first_failure_starts_a_streak_of_one_and_says_so() {
        assert_eq!(
            after_pass(None, true),
            (Some(NonZeroU32::MIN), HealthChange::StartedFailing)
        );
    }

    #[test]
    fn a_further_failure_lengthens_the_streak_quietly() {
        assert_eq!(
            after_pass(Some(NonZeroU32::MIN), true),
            (Some(TWO), HealthChange::Unchanged)
        );
    }

    #[test]
    fn a_success_after_failures_ends_the_streak_and_says_so() {
        assert_eq!(
            after_pass(Some(TWO), false),
            (None, HealthChange::Recovered)
        );
    }

    #[test]
    fn a_success_while_healthy_changes_nothing() {
        assert_eq!(after_pass(None, false), (None, HealthChange::Unchanged));
    }
}
