//! Stores the task queue and each task's trajectory, and decides nothing about what runs
//! next.
//!
//! A durable store knows which tasks are runnable, what each produced and what happened to it. It
//! keeps no dependency graph: a task becomes `Ready` because a write made it so (an
//! enqueue, a wake, a reclaim), never because the durable store derived it, and its sweeps
//! (expired leases, due parks, unreleased holds) are indexed queries, not graph walks.
//! A read or write that failed is an `Err`, never an empty or default value.
//!
//! Backends: [`MemDurableStore`], [`SqliteDurableStore`]. The port its sealed journals go through,
//! [`journal`], is defined here by the durable store that uses it; a store that writes the files
//! (Parquet, in `tars-melt`) implements it. Sealing finished journals into those files is the
//! store's own upkeep, started when it is opened ([`SqliteDurableStore::open`]); nobody driving
//! tasks over a [`DurableStore`] can ask for it or wait on it.
//!
//! # Laws
//!
//! Every backend keeps these. `tests/durable_store_laws.rs` checks each one the same way on every
//! backend, and a new backend passes that file before anything runs on it.
//!
//! 1. **Events are only appended.** A read returns every earlier event as it was, at its
//!    `seq`; a task's [`trajectory`](DurableStoreRead::trajectory) is its own events in `seq`
//!    order with payloads as written. Sealing moves a journal into a file, so
//!    [`since`](DurableStoreRead::since) and `trajectory` read the same events before and after.
//!    [`MemDurableStore`] is the exception: past [`MemDurableStore::TRAJECTORY_CAP`] events it
//!    drops the oldest.
//! 2. **The durable store writes the lifecycle.** Each write that moves a row appends its lifecycle
//!    event in the same transaction: an enqueue `Enqueued`, a claim `Running`, a commit
//!    `Done`, an ending failure `Failed` with its reason, a cancel `Failed` with
//!    [`Reason::Cancelled`](tars_types::Reason::Cancelled), a park `Parked` (and then
//!    `Woken` when it bounces), and a wake, a reply or a due deadline `Woken`. A retry and
//!    a reclaim write none, since they run the same run again. So a caller hands over
//!    only why (a result, a reason), never the event, and appends only its own `Delta`;
//!    and a trajectory's last lifecycle event names the row's status, except on a task a
//!    retry or a reclaim made `Ready`, whose last is the `Running` of the run it repeats.
//!    Every write lands whole, these events included, or not at all.
//! 3. **Writes are idempotent and an ended task stays ended.** Enqueuing an id already on
//!    the durable store changes nothing and appends nothing;
//!    [`record_effect`](DurableStore::record_effect) keeps the first result; a write that would
//!    move an ended task (an ending, a park, a wake, a reply) is
//!    [`DurableStoreError::Superseded`] and leaves no trace. An append, and the holds move it
//!    carries, is the one write still legal there.
//! 4. **A write lands only where it is admitted.** An ending, and an append to a task that
//!    has not ended, from a [`Writer`] that does not hold the task is
//!    [`DurableStoreError::LeaseLost`] and leaves no trace. A write to an id with no row, an
//!    append from any writer included, is [`DurableStoreError::NoSuchTask`], and a reply to a
//!    task that is not parked [`DurableStoreError::NotParked`].
//! 5. **A new run gets exactly one `Woken`.** Every write that makes a task `Ready` for a
//!    new run (a wake or a reply to a parked task, a due deadline, a bounced park) appends
//!    one `Woken`. A wake of a `Running` task defers it to that task's park; a wake of a
//!    `Ready` task, a retry and a reclaim append none.
//! 6. **An ending wakes its continuation.** A commit, and a failure that ends the task, wakes
//!    the task's continuation in the same write; a continuation that has ended is left as it
//!    is and the ending still lands. A retry wakes nobody.
//! 7. **A sealed journal is closed.** An append to a sealed task is
//!    [`DurableStoreError::JournalSealed`], and the holds move it carried does not land.
//! 8. **A sweep touches only what it names.** [`wake_due`](DurableStore::wake_due) wakes each
//!    parked task whose deadline has come, once, and clears the deadline; a reply or a wake
//!    clears it too. [`cancel_descendants`](DurableStore::cancel_descendants) follows
//!    `continuation` through every generation, ended ones included, leaves ended tasks as
//!    they are, reports running ones apart, and stops on a cycle.
//!    [`abandoned_holds`](DurableStore::abandoned_holds) lists, in id order and up to its
//!    limit, only ended tasks that still hold something and have no live lease.
//! 9. **Holds move with the journal.** [`append_delta`](FiberStore::append_delta) moves the
//!    count in the same write as the delta it counts, and the count never goes below zero.
//! 10. **A lease belongs to one claim.** A ready task is claimed by exactly one claimant,
//!     and every claim mints its own token. [`reclaim_expired`](DurableStore::reclaim_expired)
//!     takes only leases that have run out; [`reclaim_orphans`](DurableStore::reclaim_orphans)
//!     never takes a live claim's, and takes a named owner's whatever their expiry; both count
//!     the interruption against the journal position.
//!     [`renew_leases`](DurableStore::renew_leases) and
//!     [`release_lease`](DurableStore::release_lease) move only a lease their token still holds.
//!
//! [`MemDurableStore`]: crate::durable::MemDurableStore
//! [`SqliteDurableStore`]: crate::durable::SqliteDurableStore
//! [`MemDurableStore::TRAJECTORY_CAP`]: crate::durable::MemDurableStore::TRAJECTORY_CAP
mod cursor;
mod failure;
pub mod journal;
mod mem;
mod meta;
mod seal;
mod sealed;
mod sealing;
mod sqlite;

use async_trait::async_trait;
use serde_json::Value;

use tars_types::{
    AgentKey, ClaimedTask, DurableStoreId, Event, JobId, JournaledReason, Lease, LeaseToken,
    Millis, Status, Task, TaskId, TaskSpec, TenantId,
};

pub use cursor::{Cursor, CursorRangeError};
pub use failure::UntoldFailure;
pub use mem::MemDurableStore;
pub use meta::{recorded_tenant, retenant};
pub use seal::{SealedJournal, Unsealable};
pub use sealed::{journal_task_ids, read_journal_raw};
pub use sealing::{JournalSealPolicy, SealHealth, SealJournals, Sealing, StopSealingError};
pub use sqlite::{
    DurableStoreInitError, ExistingDurableStore, ReadOnlyDurableStore, SqliteDurableStore,
    Unclaimed,
};

/// Why a durable store read or write failed.
#[derive(Debug, thiserror::Error)]
pub enum DurableStoreError {
    #[error("durable store sqlite")]
    Sqlx(#[from] sqlx::Error),
    /// A JSON column (`intent`, `result`) did not encode or decode.
    #[error("durable store encode/decode")]
    Serde(#[from] serde_json::Error),
    /// An id column holds empty text, which no id the durable store writes can be: a corrupt
    /// or foreign row.
    #[error("corrupt row: `{column}` is empty")]
    EmptyId { column: &'static str },
    /// A stored status the durable store never writes, from a corrupt or foreign row.
    #[error("corrupt task `{id}`: unknown status `{status}`")]
    CorruptStatus { id: TaskId, status: String },
    /// A journal entry, live or sealed, whose `kind` is no event this build knows: a
    /// corrupt row, or one a newer build wrote. The stored text is carried as is.
    #[error("corrupt event {seq} of task `{task_id}`: kind {kind:?}")]
    CorruptEvent {
        seq: u64,
        task_id: TaskId,
        kind: String,
        source: serde_json::Error,
    },
    /// A journal row with a `seq` at or below zero, which the schema refuses.
    #[error("corrupt event of task `{task_id}`: seq {seq}")]
    CorruptSeq { task_id: TaskId, seq: i64 },
    /// A sealed row whose seq is above `i64::MAX`, which no durable store wrote.
    #[error("seq {seq} is beyond what a durable store can store")]
    SeqOutOfRange { seq: u64 },
    /// A row's interruption record is half-written or out of range — a corrupt /
    /// foreign row, since the durable store writes both columns together. The stored values are
    /// carried as they are.
    #[error("corrupt task `{id}`: interruption record at={at:?} times={times:?}")]
    CorruptInterruptions {
        id: TaskId,
        at: Option<i64>,
        times: Option<i64>,
    },
    /// A row whose held-resource count is negative, which the schema refuses.
    #[error("corrupt task `{id}`: holds {holds}")]
    CorruptHolds { id: TaskId, holds: i64 },
    /// A row holds a lease whose claim token is missing or unreadable — every claim
    /// writes one, so this is a corrupt / foreign row. The stored value is carried as is.
    #[error("corrupt task `{id}`: lease token {token:?}")]
    CorruptLeaseToken { id: TaskId, token: Option<String> },
    /// An operation named a task id the durable store has no row for.
    #[error("no such task `{0}`")]
    NoSuchTask(TaskId),
    /// [`DurableStore::resume`] found the task `Ready` or `Running`, so nothing on it is
    /// waiting for an answer.
    #[error("task `{id}` is `{}`, not parked — nothing there is waiting for an answer", Status::from(*status).as_str())]
    NotParked { id: TaskId, status: UnparkedStatus },
    /// [`DurableStore::resume`] was handed a reply that does not answer what the parked task
    /// waits for ([`answers`]): a reply of the wrong kind, from someone not asked, or to a
    /// task waiting only for its children. Nothing was written.
    #[error("task `{id}` waits for {wait:?}; refusing the reply {reply:?}")]
    WrongReply {
        id: TaskId,
        wait: Box<tars_types::Wait>,
        reply: Box<tars_types::AgentReply>,
    },
    /// A write that would have moved a task already in a terminal state was refused.
    ///
    /// The case this exists for: a child is cancelled because its parent died while a
    /// runner is still executing it. When that runner commits, the commit would flip the
    /// row back to `Done`, the cancellation would un-happen, and the task would go on to
    /// spawn children nobody is left to collect.
    ///
    /// An ending (`commit`, `fail`, `fail_terminal`, `park`) from the writer holding the
    /// task gets this on a terminal row, and so does a `wake` or `resume`;
    /// an ending from a writer that does not hold the task gets
    /// [`LeaseLost`](Self::LeaseLost) instead. A `park` from that same cancelled child
    /// would otherwise flip `failed` back to `parked` (or `ready`, with a wake pending)
    /// and a runner would re-claim and re-execute a finished task.
    #[error("task `{id}` is already `{}` — refusing to overwrite it", status.as_str())]
    Superseded { id: TaskId, status: Status },
    /// A write from a [`Writer`] that does not hold the task was refused: an ending
    /// (`commit`, `fail`, `fail_terminal`, `park`), or a trajectory append to a task that
    /// has not ended. The attempt's lease was reclaimed and the task is `status` now, held by another claim or by
    /// none; or the writer never claimed a task someone holds. Nothing was written: the
    /// attempt's result and its journal entries belong to no run any more.
    #[error("task `{id}` is `{}` and this writer does not hold it — refusing the write", status.as_str())]
    LeaseLost { id: TaskId, status: Status },
    /// An append to a task whose journal has been sealed into a file. Refused in the
    /// same write as the append, so whatever that write also changed (a `holds` move, a
    /// status flip) rolls back with it.
    #[error("task `{task_id}`'s journal is sealed — refusing to append to it")]
    JournalSealed { task_id: TaskId },
    /// [`SealJournals::seal_tasks`] left its whole batch as it was, because of this task.
    #[error("task `{task_id}` not sealed: {why}")]
    Unsealable { task_id: TaskId, why: Unsealable },
    /// The journal store staged a file without the task it was handed.
    #[error("staged journal file `{file}` holds no group for `{task_id}`")]
    NotStaged { task_id: TaskId, file: String },
    /// Writing or reading a sealed journal file failed.
    #[error("journal store")]
    Journal(#[from] crate::durable::journal::JournalError),
    /// A `journal_index` row whose row group no file can have, or whose seqs no journal
    /// can have. The stored numbers are carried as they are.
    #[error(
        "corrupt journal index for `{task_id}`: row group {row_group}, seqs {first_seq}..={last_seq}"
    )]
    CorruptJournalIndex {
        task_id: TaskId,
        row_group: i64,
        first_seq: i64,
        last_seq: i64,
    },
    /// A registered file name the durable store could not have minted.
    #[error(transparent)]
    CorruptJournalFile(#[from] crate::durable::journal::BadJournalFileName),
}

impl DurableStoreError {
    /// Returns whether another connection held the database when the durable store needed it, the
    /// one durable store failure known to pass on another try.
    pub fn is_lock_contention(&self) -> bool {
        match self {
            Self::Sqlx(e) => crate::is_lock_contention(e),
            _ => false,
        }
    }
}

/// What a wake does to a task, decided by the status it finds the task in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    /// Parked: the task becomes `Ready`, and the wake is its `Woken` event.
    Resume,
    /// Running: the wake waits in `wake_pending`, and the task's park bounces it back to
    /// `Ready` and writes the `Woken` then. Written now, a run that died after this wake
    /// would be retried under it and miss the reply its run was answering.
    Defer,
    /// Ready: the run already queued sees what the wake would report, so nothing is
    /// written.
    Absorb,
    /// Done or failed: the wake is refused.
    Ended,
}

/// Returns what a wake does to a task in `status`. Both backends write what this returns,
/// so they agree on which wakes leave a `Woken` event.
fn wake_of(status: Status) -> Wake {
    match status {
        Status::Parked => Wake::Resume,
        Status::Running => Wake::Defer,
        Status::Ready => Wake::Absorb,
        Status::Done | Status::Failed => Wake::Ended,
    }
}

/// What a failed attempt did to its task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailedAttempt {
    /// The retry budget had room: the task is `Ready` again for another attempt of the
    /// same run, its lease dropped.
    Requeued,
    /// The budget is spent: the task is `Failed`, with its `Failed` event, and keeps its
    /// lease for its holder to release.
    Terminal,
}

/// Returns what a failure does to a task that has now failed `attempts` times, against a
/// budget of `failure_limit` attempts.
fn failed_attempt(attempts: u32, failure_limit: u32) -> FailedAttempt {
    match attempts < failure_limit {
        true => FailedAttempt::Requeued,
        false => FailedAttempt::Terminal,
    }
}

/// Who a durable store write speaks for: an ending, or an append to the task's trajectory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writer {
    /// The attempt that claimed the task under this token.
    Claim(LeaseToken),
    /// A writer that never claimed the task: a fiber run directly with nothing
    /// supervising it.
    Unclaimed,
}

impl Writer {
    /// Returns the writer a step's own durable store writes speak for: the claim its supervising
    /// runner holds, or [`Unclaimed`](Self::Unclaimed) when nothing supervises it.
    pub fn of(ctx: &tars_types::StepContext) -> Self {
        match ctx.claim() {
            Some(token) => Self::Claim(token),
            None => Self::Unclaimed,
        }
    }
}

/// Returns whether a row whose lease is `lease` is leased to `writer`: a claim holds the row
/// leased under its token, and an unclaimed writer holds only a row nobody holds. Expiry is
/// not consulted: until a reclaim clears it, an expired lease is still the claim's.
fn leased_to(lease: Option<&Lease>, writer: Writer) -> bool {
    match (lease, writer) {
        (None, Writer::Unclaimed) => true,
        (Some(l), Writer::Claim(c)) => l.token == c,
        _ => false,
    }
}

/// Whether an ending may be written to a task, decided by the task's status, the lease
/// on its row and the writer the ending speaks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// The writer holds the task and the task has not ended.
    Admitted,
    /// The writer does not hold the task: its lease was reclaimed (and maybe claimed
    /// again), or it never claimed a task someone holds.
    LeaseLost,
    /// The writer holds the task but the task already ended: someone cancelled it while
    /// this attempt ran.
    Ended,
}

/// Returns whether an ending from `writer` may be written to a task in `status` whose
/// row holds `lease`.
///
/// The writer is compared before the status, so an attempt whose task was reclaimed and
/// then finished by another run is told it lost the lease, not that the task ended, and
/// does not wind up a task that is not its own.
fn admission(status: Status, lease: Option<&Lease>, writer: Writer) -> Admission {
    match (leased_to(lease, writer), status.is_terminal()) {
        (false, _) => Admission::LeaseLost,
        (true, true) => Admission::Ended,
        (true, false) => Admission::Admitted,
    }
}

/// Refuses an ending [`admission`] does not let through, with the error both backends
/// return for it.
fn admit(t: &Task, writer: Writer) -> Result<(), DurableStoreError> {
    match admission(t.status, t.lease.as_ref(), writer) {
        Admission::Admitted => Ok(()),
        Admission::LeaseLost => Err(lease_lost(t)),
        Admission::Ended => Err(DurableStoreError::Superseded {
            id: t.spec.id.clone(),
            status: t.status,
        }),
    }
}

/// Whether a trajectory append may be written to a task, decided by the task's status, the
/// lease on its row and the writer the append speaks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppendAdmission {
    Admitted,
    /// The task can still run and is not leased to the writer.
    LeaseLost,
}

/// Returns whether an append from `writer` may land on a task in `status` whose row holds
/// `lease`, so an attempt whose lease was taken over cannot write into the journal the next
/// run replays.
///
/// A terminal task runs no more, so nothing appended to it can interleave with a run.
/// What lands there records what was given back: the holder winding up, the sweep
/// settling abandoned resources. Refusing those would leave a resource no one can settle.
fn append_admission(status: Status, lease: Option<&Lease>, writer: Writer) -> AppendAdmission {
    match status.is_terminal() || leased_to(lease, writer) {
        true => AppendAdmission::Admitted,
        false => AppendAdmission::LeaseLost,
    }
}

/// Whether a run speaking for a writer holds its task, as the task's row says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holding {
    Held,
    /// The row is leased to someone else, or to nobody while the writer speaks for a claim;
    /// the task is `status` now.
    Lost {
        status: Status,
    },
    /// There is no row, and the writer speaks for a claim on one.
    NoRow,
}

/// Returns whether a run speaking for `writer` holds a task whose row is `row` (its status
/// and lease).
///
/// The status plays no part: a task that was reclaimed and then finished by another run is
/// that run's, and what this run took is that run's to give back. An append is judged
/// differently ([`append_admission`] lets anything land on an ended task), so neither
/// stands in for the other.
fn holding(row: Option<(Status, Option<&Lease>)>, writer: Writer) -> Holding {
    match row {
        Some((_, lease)) if leased_to(lease, writer) => Holding::Held,
        Some((status, _)) => Holding::Lost { status },
        None if writer == Writer::Unclaimed => Holding::Held,
        None => Holding::NoRow,
    }
}

/// Returns whether a task's held resources are abandoned: it has ended, still holds
/// something, and no run holds its lease. A lease is gone once `now` reaches its expiry.
///
/// An ended task can still be running: a child cancelled while it ran is `Failed` and keeps
/// the lease its runner renews. What it holds is that run's to give back, under the body that
/// is still using it, so only its run winding up, or the lease running out, hands it on.
pub(crate) fn abandoned_hold(
    status: Status,
    holds: u64,
    lease_expires_at: Option<Millis>,
    now: Millis,
) -> bool {
    holds > 0 && status.is_terminal() && lease_expires_at.is_none_or(|expires_at| now >= expires_at)
}

/// Refuses an append to an id with no row, whoever writes it, and one [`append_admission`]
/// does not let through, with the error both backends return for it.
pub(crate) fn admit_append(
    id: &TaskId,
    row: Option<&Task>,
    writer: Writer,
) -> Result<(), DurableStoreError> {
    let Some(t) = row else {
        return Err(DurableStoreError::NoSuchTask(id.clone()));
    };
    match append_admission(t.status, t.lease.as_ref(), writer) {
        AppendAdmission::Admitted => Ok(()),
        AppendAdmission::LeaseLost => Err(lease_lost(t)),
    }
}

/// Reads the id stored in `column`; the schema does not refuse empty text there, so an empty
/// one is a corrupt row.
pub(crate) fn stored_id<T: TryFrom<String>>(
    column: &'static str,
    text: String,
) -> Result<T, DurableStoreError> {
    T::try_from(text).map_err(|_empty| DurableStoreError::EmptyId { column })
}

fn lease_lost(t: &Task) -> DurableStoreError {
    DurableStoreError::LeaseLost {
        id: t.spec.id.clone(),
        status: t.status,
    }
}

/// What an admitted park does, decided by whether a wake arrived since the task was
/// claimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Park {
    /// The task waits as `Parked` with its question and deadline.
    Wait,
    /// A wake arrived while it ran: the task goes straight back to `Ready`, and the
    /// deferred wake is written as its `Woken` event after the `Parked` one.
    Bounce,
}

fn park_of(wake_pending: bool) -> Park {
    match wake_pending {
        true => Park::Bounce,
        false => Park::Wait,
    }
}

/// Returns whether `reply` answers what a task parked on `wait` waits for: an ask of its kind
/// ([`fits`](tars_types::fits)), from one of those asked. A task waiting for its children
/// asked nobody.
pub fn answers(wait: &tars_types::Wait, reply: &tars_types::AgentReply) -> bool {
    match wait {
        tars_types::Wait::Children => false,
        tars_types::Wait::Answer { ask, from, .. } => {
            tars_types::fits(ask, reply) && from.contains(reply.by())
        }
    }
}

/// Whether a task in some status is waiting for a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    Waiting,
    NotWaiting(UnparkedStatus),
    Ended,
}

fn reply_of(status: Status) -> Reply {
    match status {
        Status::Parked => Reply::Waiting,
        Status::Ready => Reply::NotWaiting(UnparkedStatus::Ready),
        Status::Running => Reply::NotWaiting(UnparkedStatus::Running),
        Status::Done | Status::Failed => Reply::Ended,
    }
}

/// The status of a task that has neither parked nor ended, which is what
/// [`DurableStoreError::NotParked`] finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnparkedStatus {
    Ready,
    Running,
}

impl From<UnparkedStatus> for Status {
    fn from(s: UnparkedStatus) -> Self {
        match s {
            UnparkedStatus::Ready => Status::Ready,
            UnparkedStatus::Running => Status::Running,
        }
    }
}

/// What [`DurableStore::cancel_descendants`] did.
///
/// Two lists rather than one count, because "3 cancelled" and "3 cancelled, 2 still
/// running" are different situations and the caller has to be able to tell them apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cancelled {
    /// Cancelled and definitely idle — they had not started, so nothing is executing
    /// them and nobody else will release what they held. The caller settles these.
    pub stopped: Vec<TaskId>,
    /// Cancelled while a runner was executing them. Their rows are terminal, so their
    /// eventual `commit` is refused, but they go on spending until then and their own
    /// runner winds them up. The caller must not release their resources: they may still
    /// be using them.
    pub running: Vec<TaskId>,
}

/// The reads of a durable store: everything a viewer needs.
///
/// Split from [`DurableStore`] so a durable store that must not be written to (one
/// no current runner has claimed, [`ReadOnlyDurableStore`]) can be read through the same calls
/// while the writes are not there to call.
#[async_trait]
pub trait DurableStoreRead: Send + Sync {
    async fn get(&self, id: &TaskId) -> Result<Option<Task>, DurableStoreError>;

    /// Returns every task row.
    async fn tasks(&self) -> Result<Vec<Task>, DurableStoreError>;

    /// Returns every task of `job` that has not ended. The default filters
    /// [`tasks`](DurableStoreRead::tasks); a backend that can answer from an index should.
    async fn open_tasks(&self, job: &JobId) -> Result<Vec<Task>, DurableStoreError> {
        Ok(self
            .tasks()
            .await?
            .into_iter()
            .filter(|t| &t.spec.job_id == job && !t.status.is_terminal())
            .collect())
    }

    /// Returns the events after `cursor`, in order; [`Cursor::START`] starts at the
    /// beginning. A backend may return one page, and the caller resumes from the last
    /// `seq`. Sealed journals are merged back in by `seq`: sealing moves rows, it never
    /// removes them from the stream.
    async fn since(&self, cursor: Cursor) -> Result<Vec<Event>, DurableStoreError>;

    /// Returns one task's trajectory in `seq` order, sealed or not; a resuming step
    /// rebuilds its state from its own `Delta` events here. [`MemDurableStore`] keeps a bounded
    /// number of events and may already have dropped a task's earliest ones.
    async fn trajectory(&self, task_id: &TaskId) -> Result<Vec<Event>, DurableStoreError>;

    /// Returns the last `Failed` event on `id`'s journal, sealed or not, or `None` when the
    /// journal carries none. The last, because a task retried and failed again should
    /// report the complaint that ended it.
    async fn failure(&self, id: &TaskId) -> Result<Option<JournaledReason>, DurableStoreError> {
        Ok(failure::last_failure(self.trajectory(id).await?))
    }

    /// Returns the `Failed` task `id`'s journaled reason, or why it cannot be told.
    async fn failure_reason(&self, id: &TaskId) -> Result<JournaledReason, UntoldFailure> {
        self.failure(id)
            .await
            .map_err(|source| UntoldFailure::Unreadable {
                id: id.clone(),
                source,
            })?
            .ok_or_else(|| UntoldFailure::Unrecorded { id: id.to_owned() })
    }
}

/// What a running fiber, or a step written by hand, may do to the durable store: spawn
/// children, read tasks, append to its own journal, ask whether it still holds its task, and
/// end it when nothing schedules it. Claiming, leasing, parking, reclaiming and sealing
/// belong to whoever schedules tasks, through [`DurableStore`].
#[async_trait]
pub trait FiberStore: DurableStoreRead {
    /// Enqueues many tasks all-or-nothing, as [`enqueue`](DurableStore::enqueue) does each.
    async fn enqueue_all(&self, specs: &[TaskSpec], now: Millis) -> Result<(), DurableStoreError>;

    /// Appends one `Delta` event carrying `payload` to the task's trajectory, stamped `at`
    /// (epoch millis), gives it the next `seq`, and moves the task's held-resource count by
    /// `holds_delta` in the same write (`+1` for a delta that takes a resource, `-1` for one
    /// that gives it back, `0` otherwise); the count never goes below zero.
    ///
    /// The delta is the truth and the count is an index of it, so the two land together or
    /// not at all. The count moves on an ended task too: that is where releases land, and
    /// a cancelled child still running may take a resource until it notices.
    ///
    /// An append to a sealed journal is [`DurableStoreError::JournalSealed`], and one to an
    /// id with no row [`DurableStoreError::NoSuchTask`]. On a task that has not ended, one
    /// from a writer that does not hold it is [`DurableStoreError::LeaseLost`], so an attempt
    /// whose lease was taken over cannot write into the journal the next run replays.
    async fn append_delta(
        &self,
        writer: Writer,
        task_id: &TaskId,
        payload: Value,
        holds_delta: i64,
        at: Millis,
    ) -> Result<(), DurableStoreError>;

    /// Writes `result`, flips the task `Done`, appends its `Done` event stamped `now` and
    /// wakes its [`continuation`](tars_types::TaskSpec::continuation), in one write; the lease
    /// stays for its holder to [`release`](DurableStore::release_lease).
    ///
    /// The wake goes in the same write so no task ends with its orchestrator never woken. A
    /// continuation that has itself ended has nobody left to wake: it is left as it is, and
    /// that is not this task's failure.
    ///
    /// This and every other ending ([`fail`](FiberStore::fail),
    /// [`fail_terminal`](DurableStore::fail_terminal), [`park`](DurableStore::park)) is
    /// written only while `writer` holds the row, so a runner whose lease was reclaimed
    /// cannot end a task another run now owns: it gets [`DurableStoreError::LeaseLost`] and
    /// nothing changes. A task already terminal under the same writer, `Done` included, is
    /// [`DurableStoreError::Superseded`].
    async fn commit(
        &self,
        id: &TaskId,
        writer: Writer,
        result: Value,
        now: Millis,
    ) -> Result<(), DurableStoreError>;

    /// Counts a failed attempt and makes the task `Ready` again if the count is below
    /// `failure_limit`, else `Failed` with a `Failed` event carrying `reason` and its
    /// continuation woken as [`commit`](FiberStore::commit) wakes it, and returns which. A
    /// crash is reclaimed through [`reclaim_expired`](DurableStore::reclaim_expired), not
    /// here, so it never spends the retry budget. Refused as
    /// [`commit`](FiberStore::commit) is.
    ///
    /// An attempt that is retried leaves no event, wakes nobody, and its `reason` is
    /// dropped: the retry is the same run, and a `Failed` event would say the task ended.
    async fn fail(
        &self,
        id: &TaskId,
        writer: Writer,
        failure_limit: u32,
        reason: JournaledReason,
        now: Millis,
    ) -> Result<FailedAttempt, DurableStoreError>;

    /// Returns whether a run speaking for `writer` holds task `task_id`, as its row says now
    /// ([`Holding`]).
    async fn holding(
        &self,
        task_id: &TaskId,
        writer: Writer,
    ) -> Result<Holding, DurableStoreError> {
        let row = self.get(task_id).await?;
        Ok(holding(
            row.as_ref().map(|t| (t.status, t.lease.as_ref())),
            writer,
        ))
    }
}

/// A task queue plus each task's trajectory. The durable store stores progress; what runs next
/// is decided above it.
#[async_trait]
pub trait DurableStore: FiberStore {
    // ── identity ─────────────────────────────────────────────────────────────

    /// Returns whose work this durable store holds: the tenant `store_meta` records, checked at
    /// open.
    fn tenant(&self) -> &TenantId;

    /// Returns `store_meta`'s `store_id`, generated by the first runner that opened the
    /// durable store and never changed after.
    fn id(&self) -> &DurableStoreId;

    // ── queue ────────────────────────────────────────────────────────────────

    /// Puts a task on the durable store as [`Ready`](tars_types::Status::Ready) with its
    /// `Enqueued` event stamped `now`. Re-enqueuing an existing id changes nothing and
    /// appends nothing, so a resubmit or a resume never duplicates or resurrects a task,
    /// and never writes into a journal that may already be sealed.
    async fn enqueue(&self, spec: &TaskSpec, now: Millis) -> Result<(), DurableStoreError>;

    /// Claims one ready task for `owner`, leasing it until `now + ttl`, and appends its
    /// `Running` event stamped `now`; `None` when nothing is ready. Runners racing for one
    /// task: exactly one wins it, the others claim another. The claim clears
    /// [`wake_pending`](tars_types::Task::wake_pending): the run it starts already sees
    /// whatever an earlier wake reported.
    async fn claim_ready(
        &self,
        owner: &str,
        now: Millis,
        ttl: Millis,
    ) -> Result<Option<ClaimedTask>, DurableStoreError>;

    /// Claims one ready task whose `agent_key` is not in `excluded`, as
    /// [`claim_ready`](DurableStore::claim_ready) does. The
    /// runtime's `QueueRunner` passes the keys at their in-flight cap,
    /// so a waiting task of a full key never blocks a ready task of a free one.
    ///
    /// The default ignores `excluded` and claims any ready task; `MemDurableStore` and
    /// `SqliteDurableStore` override it.
    async fn claim_ready_excluding(
        &self,
        owner: &str,
        now: Millis,
        ttl: Millis,
        excluded: &[AgentKey],
    ) -> Result<Option<ClaimedTask>, DurableStoreError> {
        let _ = excluded;
        self.claim_ready(owner, now, ttl).await
    }

    /// Fails a task for good (a permanent error: a bad rebuild, an unrepresentable
    /// result) without counting an attempt, appends its `Failed` event carrying `reason`
    /// and wakes its continuation as [`commit`](FiberStore::commit) does, in one write,
    /// leaving the lease for its holder to [`release`](DurableStore::release_lease).
    ///
    /// Why it failed is not on the task row: it is that `Failed` event, a
    /// [`JournaledReason`] that keeps the producer's classification and not just its
    /// sentence. Refused as [`commit`](FiberStore::commit) is.
    async fn fail_terminal(
        &self,
        id: &TaskId,
        writer: Writer,
        reason: JournaledReason,
        now: Millis,
    ) -> Result<(), DurableStoreError>;

    /// Parks a running task: `Parked`, lease dropped, what it waits for (`wait`, with its
    /// deadline when it waits for an answer) stored, and a `Parked` event appended, in one
    /// write. When a
    /// wake already arrived while it ran ([`wake_pending`](tars_types::Task::wake_pending)),
    /// it goes straight back to `Ready` instead, and that wake's `Woken` event follows
    /// the `Parked` one, so a wake that raced the park is not lost and the trajectory
    /// ends in the state the row is in. Refused as [`commit`](FiberStore::commit) is.
    async fn park(
        &self,
        id: &TaskId,
        writer: Writer,
        wait: &tars_types::Wait,
        now: Millis,
    ) -> Result<(), DurableStoreError>;

    /// Returns, in id order, up to `limit` tasks whose held resources are abandoned at `now`
    /// ([`abandoned_hold`]): ended, still holding something, and with no live lease.
    ///
    /// Releasing an OS resource and writing a row cannot be made atomic, so a runner that
    /// dies between "this task is terminal" and "here is its worktree back" loses the
    /// release, and no claim, reclaim or wake touches a terminal task again. A sweep
    /// retries from this list; a successful release journals its delta and decrements
    /// the count in one write, so the task drops out. A task whose lease is still
    /// live is left to the run holding it, which may still be using what it holds.
    async fn abandoned_holds(
        &self,
        limit: usize,
        now: Millis,
    ) -> Result<Vec<TaskId>, DurableStoreError>;

    /// Makes every parked task whose deadline has passed `Ready`, clears the deadline,
    /// appends a `Woken` event without a reply to each, and returns how many it woke.
    /// The event is what tells the next run that nobody answered, instead of leaving it
    /// to read an earlier park's reply.
    ///
    /// This is the third way a park ends, beside a child completing and a human replying;
    /// without it a question nobody answers parks a task forever. A parked task runs no
    /// code, so only a sweep can watch its clock. It spends no retry budget.
    async fn wake_due(&self, now: Millis) -> Result<usize, DurableStoreError>;

    /// Fails every descendant of `parent` that has not ended, and returns them split by
    /// whether a runner was executing them.
    ///
    /// When an orchestrator dies, the children it fanned out are still `Ready` rows that
    /// the next runner would spend real money finishing for nobody. The edge followed is
    /// [`continuation`](tars_types::TaskSpec::continuation), which for a spawned child is
    /// its parent, through every generation, terminal ones included.
    ///
    /// `Running` descendants are cancelled too. Nothing here can interrupt a live future,
    /// but [`commit`] and [`fail`] refuse a terminal row ([`DurableStoreError::Superseded`]), so
    /// when such a task finishes its result is discarded instead of un-doing the
    /// cancellation.
    ///
    /// `reason` is the canceller's wording, recorded on each descendant's `Failed` event
    /// as a [`Reason::Cancelled`](tars_types::Reason::Cancelled), an interrupt, so a
    /// supervisor never charges its own cancel against a child's failure budget.
    ///
    /// [`commit`]: FiberStore::commit
    /// [`fail`]: FiberStore::fail
    async fn cancel_descendants(
        &self,
        parent: &TaskId,
        reason: &str,
        now: Millis,
    ) -> Result<Cancelled, DurableStoreError>;

    /// Wakes a task, in one write with the `Woken` event it appends.
    ///
    /// A parked task becomes `Ready` with its question and deadline cleared. A `Running`
    /// one (the wake beat its park) gets `wake_pending` and no event yet: its coming
    /// [`park`](DurableStore::park) makes it `Ready` and writes the `Woken` then, and if the run
    /// dies first its retry is still the run the last wake caused. A `Ready` one is left
    /// as it is, with no event: the run it is waiting for sees everything the wake would
    /// report, and a `Woken` without a reply would hide a human reply that run has not
    /// read yet. A missing task is [`DurableStoreError::NoSuchTask`]; a terminal one is
    /// [`DurableStoreError::Superseded`].
    ///
    /// Every write that makes a task `Ready` for a new run (this one, a reply, a due
    /// deadline, a bounced park) appends a `Woken`, and no other write does, so the
    /// newest `Woken` belongs to the wake behind the task's next run. That is what
    /// a fiber relies on when it reads the reply its run was woken with. A retry
    /// after [`fail`](FiberStore::fail) and a reclaim also make a task `Ready`, but they run
    /// the same run again, under the wake that caused it, so they append none.
    async fn wake(&self, id: &TaskId, now: Millis) -> Result<(), DurableStoreError>;

    /// Stores `reply` on a parked task, makes it `Ready` and appends a `Woken` event carrying
    /// the reply, in one write; `FiberScope` reads the reply from that event. A missing task
    /// is [`DurableStoreError::NoSuchTask`], a `Ready` or `Running` one
    /// [`DurableStoreError::NotParked`], a terminal one [`DurableStoreError::Superseded`], and
    /// a reply that does not answer what the task waits for ([`answers`])
    /// [`DurableStoreError::WrongReply`], with nothing written.
    async fn resume(
        &self,
        id: &TaskId,
        reply: &tars_types::AgentReply,
        now: Millis,
    ) -> Result<(), DurableStoreError>;

    /// Makes every `Running` task whose lease has expired (a crashed worker) `Ready`
    /// again, without spending its retry budget, and returns how many. A live claim's
    /// lease expires in the future, so it is not touched.
    ///
    /// Every task it reclaims was interrupted, and that is counted in the same write:
    /// [`Task::interrupted`](tars_types::Task::interrupted) moves by
    /// [`InterruptionStreak::after`](tars_types::InterruptionStreak::after), against the number of
    /// `Delta` events on the task's trajectory. Not a charge against the retry budget —
    /// the record a runner reads before resuming the task into the same step again.
    async fn reclaim_expired(&self, now: Millis) -> Result<usize, DurableStoreError>;

    /// Makes `Ready` again every `Running` task whose lease is not in `live` and has
    /// expired or, when `previous` names an owner, is leased to that owner; this is the
    /// reclaim a runner does.
    ///
    /// `live` is the claims the calling runner is executing right now, and their tasks
    /// are kept whatever their lease says: a step that outlives its lease (a build, a
    /// test suite) must not be reclaimed by its own runner and run twice
    /// (`tests/fiber_long_effect.rs`). They are named by
    /// [`LeaseToken`], not task id, so an attempt that has ended
    /// neither shields nor exposes a later claim of the same task.
    ///
    /// `previous`, when given, also reclaims every `Running` row leased to that owner
    /// regardless of expiry — the work of a process that died under that id. Only the
    /// caller can know that is safe (no other live runner uses the id, and it is not
    /// itself executing anything), so the drive always passes `None`; see
    /// the runtime's `QueueRunner::reclaim_own_orphans`.
    ///
    /// Counts each interruption exactly as [`reclaim_expired`](DurableStore::reclaim_expired)
    /// does, in the same write as the flip.
    async fn reclaim_orphans(
        &self,
        previous: Option<&str>,
        live: &[LeaseToken],
        now: Millis,
    ) -> Result<usize, DurableStoreError>;

    /// Pushes out to `expires_at` the lease of each claim in `tokens` that still holds
    /// it, whatever the task's status, and returns how many it renewed. This heartbeat
    /// stops a peer from reclaiming a step that is merely slow, and a task someone else
    /// ended (a cancelled child) from being sealed while its runner still executes it. A
    /// token that was released, or whose task was claimed again, renews nothing; that is
    /// a smaller count, not an error.
    async fn renew_leases(
        &self,
        tokens: &[LeaseToken],
        expires_at: Millis,
    ) -> Result<usize, DurableStoreError>;

    /// Ends claim `token` on task `id` by clearing its lease, if that claim still holds
    /// it; otherwise it changes nothing.
    ///
    /// No write that ends a task clears its lease; its runner does, after winding the
    /// task up. Until then S(T) will not seal the task, so a task someone else ended
    /// while it ran is never sealed under its runner, whose next `acquire` would find
    /// its journal closed.
    async fn release_lease(&self, id: &TaskId, token: LeaseToken) -> Result<(), DurableStoreError>;

    /// Records an external effect's `result` under a stable `key`; the first write wins,
    /// since it is what the external system did. On resume a step re-derives the same
    /// key and [`effect_result`](DurableStore::effect_result) returns the recorded value
    /// instead of firing the irreversible write again.
    async fn record_effect(&self, key: &str, result: &Value) -> Result<(), DurableStoreError>;

    /// Returns the result recorded under `key`, or `None` if this effect never completed.
    async fn effect_result(&self, key: &str) -> Result<Option<Value>, DurableStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_resumes_only_a_parked_task() {
        assert_eq!(wake_of(Status::Parked), Wake::Resume);
        assert_eq!(wake_of(Status::Running), Wake::Defer);
        assert_eq!(wake_of(Status::Ready), Wake::Absorb);
        assert_eq!(wake_of(Status::Done), Wake::Ended);
        assert_eq!(wake_of(Status::Failed), Wake::Ended);
    }

    #[test]
    fn a_failure_requeues_until_the_attempts_reach_the_limit() {
        assert_eq!(failed_attempt(2, 3), FailedAttempt::Requeued);
        assert_eq!(failed_attempt(3, 3), FailedAttempt::Terminal);
        assert_eq!(failed_attempt(1, 1), FailedAttempt::Terminal);
    }

    #[test]
    fn a_park_bounces_only_when_a_wake_is_pending() {
        assert_eq!(park_of(false), Park::Wait);
        assert_eq!(park_of(true), Park::Bounce);
    }

    fn lease(token: LeaseToken) -> Lease {
        Lease {
            owner: "r".into(),
            expires_at: 10,
            token,
        }
    }

    fn token(n: u128) -> LeaseToken {
        LeaseToken::from(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn an_ending_from_the_claim_holding_the_task_is_admitted() {
        let held = lease(token(1));
        assert_eq!(
            admission(Status::Running, Some(&held), Writer::Claim(token(1))),
            Admission::Admitted
        );
    }

    #[test]
    fn an_ending_from_a_claim_the_row_no_longer_holds_is_lost_whatever_the_status() {
        let other = lease(token(2));
        for status in [
            Status::Ready,
            Status::Running,
            Status::Parked,
            Status::Done,
            Status::Failed,
        ] {
            assert_eq!(
                admission(status, None, Writer::Claim(token(1))),
                Admission::LeaseLost
            );
            assert_eq!(
                admission(status, Some(&other), Writer::Claim(token(1))),
                Admission::LeaseLost
            );
        }
    }

    #[test]
    fn an_unclaimed_ending_is_admitted_only_on_a_row_nobody_holds() {
        assert_eq!(
            admission(Status::Ready, None, Writer::Unclaimed),
            Admission::Admitted
        );
        let held = lease(token(1));
        assert_eq!(
            admission(Status::Running, Some(&held), Writer::Unclaimed),
            Admission::LeaseLost
        );
    }

    #[test]
    fn an_ending_on_an_ended_task_is_refused_even_from_its_own_claim() {
        let held = lease(token(1));
        for status in [Status::Done, Status::Failed] {
            assert_eq!(
                admission(status, Some(&held), Writer::Claim(token(1))),
                Admission::Ended
            );
            assert_eq!(admission(status, None, Writer::Unclaimed), Admission::Ended);
        }
    }

    #[test]
    fn an_append_from_a_claim_whose_lease_was_taken_over_is_lost_while_the_task_can_run() {
        let other = lease(token(2));
        for (status, lease_) in [
            (Status::Running, Some(&other)),
            (Status::Ready, None),
            (Status::Parked, None),
        ] {
            assert_eq!(
                append_admission(status, lease_, Writer::Claim(token(1))),
                AppendAdmission::LeaseLost
            );
        }
        let mine = lease(token(1));
        assert_eq!(
            append_admission(Status::Running, Some(&mine), Writer::Claim(token(1))),
            AppendAdmission::Admitted
        );
    }

    #[test]
    fn any_writer_appends_to_a_task_that_ended() {
        let other = lease(token(2));
        for status in [Status::Done, Status::Failed] {
            for lease_ in [None, Some(&other)] {
                for writer in [Writer::Claim(token(1)), Writer::Unclaimed] {
                    assert_eq!(
                        append_admission(status, lease_, writer),
                        AppendAdmission::Admitted
                    );
                }
            }
        }
    }

    #[test]
    fn an_unclaimed_append_lands_only_where_nobody_holds_a_task_that_can_run() {
        let held = lease(token(1));
        assert_eq!(
            append_admission(Status::Ready, None, Writer::Unclaimed),
            AppendAdmission::Admitted
        );
        assert_eq!(
            append_admission(Status::Running, Some(&held), Writer::Unclaimed),
            AppendAdmission::LeaseLost
        );
    }

    #[test]
    fn an_append_to_an_id_with_no_row_is_refused_whoever_writes_it() {
        for writer in [Writer::Unclaimed, Writer::Claim(token(1))] {
            assert!(matches!(
                admit_append(&TaskId::new("ghost"), None, writer),
                Err(DurableStoreError::NoSuchTask(id)) if id == TaskId::new("ghost")
            ));
        }
    }

    #[test]
    fn a_claim_whose_task_another_run_holds_does_not_hold_it_even_once_the_task_ended() {
        let other = lease(token(2));
        for status in [
            Status::Ready,
            Status::Running,
            Status::Parked,
            Status::Done,
            Status::Failed,
        ] {
            assert_eq!(
                holding(Some((status, Some(&other))), Writer::Claim(token(1))),
                Holding::Lost { status }
            );
            assert_eq!(
                holding(Some((status, None)), Writer::Claim(token(1))),
                Holding::Lost { status }
            );
        }
    }

    #[test]
    fn a_claim_holds_its_task_whatever_the_status() {
        let mine = lease(token(1));
        for status in [Status::Running, Status::Done, Status::Failed] {
            assert_eq!(
                holding(Some((status, Some(&mine))), Writer::Claim(token(1))),
                Holding::Held
            );
        }
    }

    #[test]
    fn an_unclaimed_run_holds_only_a_task_nobody_holds() {
        let held = lease(token(1));
        for status in [Status::Running, Status::Done] {
            assert_eq!(
                holding(Some((status, Some(&held))), Writer::Unclaimed),
                Holding::Lost { status }
            );
            assert_eq!(
                holding(Some((status, None)), Writer::Unclaimed),
                Holding::Held
            );
        }
        assert_eq!(holding(None, Writer::Unclaimed), Holding::Held);
        assert_eq!(holding(None, Writer::Claim(token(1))), Holding::NoRow);
    }

    #[test]
    fn an_ended_task_with_a_live_lease_is_left_to_its_run() {
        for status in [Status::Done, Status::Failed] {
            assert!(!abandoned_hold(status, 1, Some(101), 100));
            assert!(abandoned_hold(status, 1, Some(100), 100));
            assert!(abandoned_hold(status, 1, None, 100));
        }
    }

    #[test]
    fn a_task_that_can_still_run_or_owes_nothing_is_not_abandoned() {
        for status in [Status::Ready, Status::Running, Status::Parked] {
            assert!(!abandoned_hold(status, 1, None, 100));
        }
        assert!(!abandoned_hold(Status::Failed, 0, None, 100));
    }

    #[test]
    fn only_a_parked_task_is_waiting_for_a_reply() {
        assert_eq!(reply_of(Status::Parked), Reply::Waiting);
        assert_eq!(
            reply_of(Status::Ready),
            Reply::NotWaiting(UnparkedStatus::Ready)
        );
        assert_eq!(
            reply_of(Status::Running),
            Reply::NotWaiting(UnparkedStatus::Running)
        );
        assert_eq!(reply_of(Status::Done), Reply::Ended);
        assert_eq!(reply_of(Status::Failed), Reply::Ended);
    }
}
