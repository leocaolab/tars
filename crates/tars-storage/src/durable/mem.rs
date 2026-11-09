//! Keeps a [`DurableStore`] in memory, under one mutex over the task map and the trajectory; it
//! does not survive a restart (use [`SqliteDurableStore`](super::SqliteDurableStore) for that).
//!
//! The lock is a [`tokio::sync::Mutex`], and it is FIFO-fair: releasing it hands the permit to the first queued waiter,
//! even one nobody will poll again. A caller that holds a suspended `MemDurableStore` call (a
//! boxed future parked mid-acquire) and then awaits a different `MemDurableStore` call without
//! polling the first deadlocks. Every call here is short and waits on nothing outside,
//! so avoiding that is the caller's polling discipline; tars-runtime's `QueueRunner::drive`
//! states it.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use crate::durable::journal::{
    JournalFileName, JournalRow, MemTrajectoryLog, StagedFile, TrajectoryLog,
};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

use crate::durable::seal::{
    Candidate, IndexEntry, Pending, SealFacts, SealPlan, SealedJournal, check, event_of, fit,
    merged_page, recheck_batch, remove_files, row_bytes, sealed_journals,
};
use crate::durable::sealing::SealJournals;
use crate::durable::{
    Cursor, DurableStore, DurableStoreError, DurableStoreRead, FailedAttempt, FiberStore, Park,
    Reply, Wake, Writer, admit, admit_append, answers, failed_attempt, park_of, reply_of, wake_of,
};
use tars_types::{
    AgentKey, ClaimedTask, DurableStoreId, Event, EventKind, InterruptionStreak, JournaledReason,
    Lease, LeaseToken, Millis, Status, Task, TaskId, TaskSpec, TenantId,
};
use tars_types::{AgentReply, Wait};

#[derive(Default)]
struct MemState {
    tasks: HashMap<TaskId, Task>,
    trajectory: VecDeque<Event>,
    seq: u64,
    effects: HashMap<String, Value>,
    /// The `journal_file` registry: a file is visible only once `Live`.
    journal_files: HashMap<JournalFileName, FileState>,
    /// Holds an entry exactly when the task's journal is sealed.
    journal_index: HashMap<TaskId, IndexEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    Staged { registered_at: Millis },
    Live,
}

impl MemState {
    /// Appends one trajectory event with the next `seq`, dropping the oldest past
    /// [`MemDurableStore::TRAJECTORY_CAP`].
    fn push_event(&mut self, task_id: TaskId, kind: EventKind, at: Millis) {
        self.seq += 1;
        let seq = self.seq;
        if self.trajectory.len() >= MemDurableStore::TRAJECTORY_CAP {
            self.trajectory.pop_front();
        }
        self.trajectory.push_back(Event {
            seq,
            task_id,
            kind,
            at,
        });
    }

    /// Counts the journal entries (`Delta` events) `id` holds: its position, for
    /// [`InterruptionStreak`]. Counted over the kept log, so after [`MemDurableStore::TRAJECTORY_CAP`]
    /// drops some the count only reads smaller, which starts a new streak rather than a false
    /// one.
    fn journal_len(&self, id: &TaskId) -> u64 {
        self.trajectory
            .iter()
            .filter(|e| e.task_id == *id && matches!(e.kind, EventKind::Delta { .. }))
            .count() as u64
    }

    /// Reads what S(T) is judged on, as the state stands.
    fn seal_facts(&self, task_id: &TaskId) -> Result<SealFacts, DurableStoreError> {
        let task = self
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| DurableStoreError::NoSuchTask(task_id.clone()))?;
        let parent = task
            .spec
            .continuation
            .as_ref()
            .and_then(|p| self.tasks.get(p))
            .map(|p| p.status);
        let open_children = self
            .tasks
            .values()
            .filter(|c| c.spec.continuation.as_ref() == Some(task_id) && !c.status.is_terminal())
            .count() as u64;
        let high_water = self
            .trajectory
            .iter()
            .filter(|e| e.task_id == *task_id)
            .map(|e| e.seq)
            .max();
        Ok(SealFacts {
            task,
            parent,
            open_children,
            sealed: self.journal_index.contains_key(task_id),
            high_water,
        })
    }

    /// Returns one task's live journal, `kind` encoded as the SQLite durable store stores it.
    fn journal_rows(&self, task_id: &TaskId) -> Result<Vec<JournalRow>, DurableStoreError> {
        self.trajectory
            .iter()
            .filter(|e| e.task_id == *task_id)
            .map(|e| {
                Ok(JournalRow {
                    task_id: e.task_id.to_string(),
                    seq: e.seq,
                    at: e.at,
                    payload: serde_json::to_string(&e.kind)?,
                })
            })
            .collect()
    }

    /// Pre-checks a batch and registers its file `staged`, under the one lock.
    fn plan_seal(
        &mut self,
        task_ids: &[TaskId],
        now: Millis,
    ) -> Result<SealPlan, DurableStoreError> {
        let journals = task_ids
            .iter()
            .map(|id| Ok((self.seal_facts(id)?, self.journal_rows(id)?)))
            .collect::<Result<Vec<_>, DurableStoreError>>()?;
        let plan = SealPlan::of(journals, now)?;
        self.journal_files
            .insert(plan.name.clone(), FileState::Staged { registered_at: now });
        Ok(plan)
    }

    /// Commits a seal under the one lock: every check first, then every change.
    fn commit_seal(
        &mut self,
        pending: &[Pending],
        now: Millis,
        staged: &StagedFile,
    ) -> Result<Vec<SealedJournal>, DurableStoreError> {
        let facts = pending
            .iter()
            .map(|p| self.seal_facts(&p.task_id))
            .collect::<Result<Vec<_>, _>>()?;
        let file_staged = matches!(
            self.journal_files.get(&staged.name),
            Some(FileState::Staged { .. })
        );
        let entries = recheck_batch(pending, facts, file_staged, staged, now)?;

        self.journal_files
            .insert(staged.name.clone(), FileState::Live);
        let sealed: BTreeSet<&TaskId> = entries.iter().map(|(id, _)| id).collect();
        self.trajectory.retain(|e| !sealed.contains(&e.task_id));
        let journals = sealed_journals(&entries);
        self.journal_index.extend(entries);
        Ok(journals)
    }

    /// Returns the tasks with live journal rows that S(T) would seal now, each with its
    /// weight.
    fn seal_candidates(
        &self,
        max_tasks: usize,
        now: Millis,
    ) -> Result<Vec<Candidate>, DurableStoreError> {
        let with_rows: BTreeSet<&TaskId> = self.trajectory.iter().map(|e| &e.task_id).collect();
        with_rows
            .into_iter()
            .filter(|id| {
                self.seal_facts(id)
                    .is_ok_and(|facts| check(&facts, now).is_ok())
            })
            .take(max_tasks)
            .map(|id| {
                let bytes = self.journal_rows(id)?.iter().map(row_bytes).sum();
                Ok(Candidate {
                    task_id: (*id).clone(),
                    bytes,
                })
            })
            .collect()
    }

    /// Drops every `staged` registration made at or before `registered_by` and returns
    /// the names dropped.
    fn forget_staged(&mut self, registered_by: Millis) -> Vec<JournalFileName> {
        let names: Vec<JournalFileName> = self
            .journal_files
            .iter()
            .filter(|(_, state)| {
                matches!(state, FileState::Staged { registered_at } if *registered_at <= registered_by)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            self.journal_files.remove(name);
        }
        names
    }

    fn refuse_if_sealed(&self, task_id: &TaskId) -> Result<(), DurableStoreError> {
        refuse_if_sealed(&self.journal_index, task_id)
    }

    /// Puts a new task on the durable store with its `Enqueued` event; an existing id is left as
    /// it was and gets no event.
    fn enqueue(&mut self, spec: &TaskSpec, now: Millis) -> Result<(), DurableStoreError> {
        if self.tasks.contains_key(&spec.id) {
            return Ok(());
        }
        self.refuse_if_sealed(&spec.id)?;
        self.tasks.insert(spec.id.clone(), task_from(spec));
        self.push_event(spec.id.clone(), EventKind::Enqueued, now);
        Ok(())
    }

    /// Counts a failed attempt on task `id` and returns what it did, with the `Failed`
    /// event when it ended the task.
    /// Writes an ending to task `id` with its events and, when the row has ended, wakes the
    /// task's continuation, all or nothing. A continuation that has ended is left as it is.
    fn end<R>(
        &mut self,
        id: &TaskId,
        now: Millis,
        f: impl FnOnce(&mut Task) -> Result<(Vec<EventKind>, R), DurableStoreError>,
    ) -> Result<R, DurableStoreError> {
        let mut t = self
            .tasks
            .get(id)
            .cloned()
            .ok_or_else(|| DurableStoreError::NoSuchTask(id.clone()))?;
        let (events, out) = f(&mut t)?;
        if !events.is_empty() {
            self.refuse_if_sealed(id)?;
        }
        let woken = match (t.status.is_terminal(), &t.spec.continuation) {
            (true, Some(c)) => self.continuation_wake(c)?,
            _ => None,
        };
        self.tasks.insert(id.clone(), t);
        for kind in events {
            self.push_event(id.clone(), kind, now);
        }
        if let Some((c, events)) = woken {
            let cid = c.spec.id.clone();
            self.tasks.insert(cid.clone(), c);
            for kind in events {
                self.push_event(cid.clone(), kind, now);
            }
        }
        Ok(out)
    }

    /// Returns continuation `id`'s row as a wake leaves it and the events the wake writes,
    /// or `None` when it has ended and there is nobody left to wake. Nothing is written.
    fn continuation_wake(
        &self,
        id: &TaskId,
    ) -> Result<Option<(Task, Vec<EventKind>)>, DurableStoreError> {
        let mut c = self
            .tasks
            .get(id)
            .cloned()
            .ok_or_else(|| DurableStoreError::NoSuchTask(id.clone()))?;
        if wake_of(c.status) == Wake::Ended {
            return Ok(None);
        }
        let events = wake_row(&mut c)?;
        if !events.is_empty() {
            self.refuse_if_sealed(id)?;
        }
        Ok(Some((c, events)))
    }

    fn update_with_events(
        &mut self,
        id: &TaskId,
        now: Millis,
        f: impl FnOnce(&mut Task) -> Result<Vec<EventKind>, DurableStoreError>,
    ) -> Result<(), DurableStoreError> {
        let events = update_row_with_events(&mut self.tasks, &self.journal_index, id, f)?;
        for kind in events {
            self.push_event(id.clone(), kind, now);
        }
        Ok(())
    }

    /// Puts an interrupted `Running` task back to `Ready`, counting the interruption
    /// against the position it was at, under the same lock as the flip.
    fn reclaim(&mut self, id: &TaskId) {
        let at = self.journal_len(id);
        if let Some(t) = self.tasks.get_mut(id) {
            t.status = Status::Ready;
            t.lease = None;
            t.interrupted = Some(InterruptionStreak::after(t.interrupted, at));
        }
    }
}

fn refuse_if_sealed(
    index: &HashMap<TaskId, IndexEntry>,
    task_id: &TaskId,
) -> Result<(), DurableStoreError> {
    match index.contains_key(task_id) {
        true => Err(DurableStoreError::JournalSealed {
            task_id: task_id.clone(),
        }),
        false => Ok(()),
    }
}

/// A [`DurableStore`] in memory.
///
/// It is built only for a named tenant, with no tenant-less constructor and no
/// `Default`, because a runner on the durable store does that tenant's work:
///
/// ```compile_fail
/// let _ = tars_storage::durable::MemDurableStore::new();
/// ```
///
/// ```compile_fail
/// let _ = tars_storage::durable::MemDurableStore::default();
/// ```
pub struct MemDurableStore {
    state: Mutex<MemState>,
    journal: Arc<dyn TrajectoryLog>,
    tenant: TenantId,
    id: DurableStoreId,
}

impl MemDurableStore {
    /// Caps the trajectory events a durable store keeps, so a long-lived one does not grow without
    /// bound. The oldest go first and `seq` stays monotonic; a task's earliest events can
    /// be lost, its `Failed` event among them, which [`SqliteDurableStore`] does not do.
    ///
    /// [`SqliteDurableStore`]: super::SqliteDurableStore
    pub const TRAJECTORY_CAP: usize = 100_000;

    /// Makes an empty durable store of `tenant`'s work under a fresh id, its sealed journals in
    /// memory with it.
    pub fn new(tenant: TenantId) -> Self {
        Self {
            state: Mutex::default(),
            journal: Arc::new(MemTrajectoryLog::new()),
            tenant,
            id: DurableStoreId::from(Uuid::new_v4()),
        }
    }

    /// Returns this durable store with its sealed journals kept in `journal`.
    pub fn with_journal(self, journal: Arc<dyn TrajectoryLog>) -> Self {
        Self { journal, ..self }
    }

    /// Locks the state. A `tokio` mutex does not poison, so a panic while holding it
    /// leaves the state reachable.
    async fn state(&self) -> MutexGuard<'_, MemState> {
        self.state.lock().await
    }
}

fn task_from(spec: &TaskSpec) -> Task {
    Task {
        spec: spec.clone(),
        status: Status::Ready,
        lease: None,
        attempt: 0,
        wait: None,
        holds: 0,
        interrupted: None,
        wake_pending: false,
        reply: None,
        result: None,
    }
}

fn superseded(t: &Task) -> DurableStoreError {
    DurableStoreError::Superseded {
        id: t.spec.id.clone(),
        status: t.status,
    }
}

/// Runs `f` on a copy of task `id`'s row and lands the copy only if `f` succeeds, so a
/// refused write leaves the row as it was even inside a transaction that goes on.
fn update_row(
    tasks: &mut HashMap<TaskId, Task>,
    id: &TaskId,
    f: impl FnOnce(&mut Task) -> Result<(), DurableStoreError>,
) -> Result<(), DurableStoreError> {
    let mut t = tasks
        .get(id)
        .cloned()
        .ok_or_else(|| DurableStoreError::NoSuchTask(id.clone()))?;
    f(&mut t)?;
    tasks.insert(id.clone(), t);
    Ok(())
}

/// Runs `f`, a change to task `id`'s row that returns the events going with it, as
/// [`update_row`] does, and returns those events for the caller to append. An event bound
/// for a sealed journal refuses the whole write, row change included.
fn update_row_with_events(
    tasks: &mut HashMap<TaskId, Task>,
    sealed: &HashMap<TaskId, IndexEntry>,
    id: &TaskId,
    f: impl FnOnce(&mut Task) -> Result<Vec<EventKind>, DurableStoreError>,
) -> Result<Vec<EventKind>, DurableStoreError> {
    let mut events = Vec::new();
    update_row(tasks, id, |t| {
        events = f(t)?;
        match events.is_empty() {
            true => Ok(()),
            false => refuse_if_sealed(sealed, id),
        }
    })?;
    Ok(events)
}

/// Makes `t` `Ready` for a new run and returns the `Woken` event that records why,
/// carrying the human `reply` when one woke it. Every write that queues a new run (a
/// wake, a reply, a due deadline, a bounced park) goes through here, so none of them
/// leaves the event out. It clears what only a park or a run had: the question, the
/// deadline, a pending wake and the lease.
#[must_use]
fn wake_into_ready(t: &mut Task, reply: Option<AgentReply>) -> EventKind {
    t.status = Status::Ready;
    t.wait = None;
    t.wake_pending = false;
    t.lease = None;
    if let Some(r) = &reply {
        t.reply = Some(r.clone());
    }
    EventKind::Woken { reply }
}

fn commit_row(
    t: &mut Task,
    writer: Writer,
    result: Value,
) -> Result<Vec<EventKind>, DurableStoreError> {
    admit(t, writer)?;
    t.result = Some(result);
    t.status = Status::Done;
    Ok(vec![EventKind::Done])
}

fn fail_row(
    t: &mut Task,
    writer: Writer,
    failure_limit: u32,
) -> Result<FailedAttempt, DurableStoreError> {
    admit(t, writer)?;
    t.attempt += 1;
    let failed = failed_attempt(t.attempt, failure_limit);
    // A `ready` row holds no lease; a failed one keeps it for its holder to release.
    match failed {
        FailedAttempt::Requeued => {
            t.status = Status::Ready;
            t.lease = None;
        }
        FailedAttempt::Terminal => t.status = Status::Failed,
    }
    Ok(failed)
}

/// Returns the events a failed attempt writes: a retry writes none, since it is the same
/// run again.
fn failure_events(failed: FailedAttempt, reason: JournaledReason) -> Vec<EventKind> {
    match failed {
        FailedAttempt::Requeued => Vec::new(),
        FailedAttempt::Terminal => vec![EventKind::Failed { reason }],
    }
}

fn fail_terminal_row(
    t: &mut Task,
    writer: Writer,
    reason: JournaledReason,
) -> Result<Vec<EventKind>, DurableStoreError> {
    admit(t, writer)?;
    t.status = Status::Failed;
    Ok(vec![EventKind::Failed { reason }])
}

fn park_row(
    t: &mut Task,
    writer: Writer,
    wait: &Wait,
) -> Result<Vec<EventKind>, DurableStoreError> {
    admit(t, writer)?;
    let parked = EventKind::Parked { wait: wait.clone() };
    match park_of(t.wake_pending) {
        Park::Wait => {
            t.lease = None;
            t.status = Status::Parked;
            t.wait = Some(wait.clone());
            Ok(vec![parked])
        }
        Park::Bounce => Ok(vec![parked, wake_into_ready(t, None)]),
    }
}

fn wake_row(t: &mut Task) -> Result<Vec<EventKind>, DurableStoreError> {
    match wake_of(t.status) {
        Wake::Resume => Ok(vec![wake_into_ready(t, None)]),
        Wake::Defer => {
            t.wake_pending = true;
            Ok(Vec::new())
        }
        Wake::Absorb => Ok(Vec::new()),
        Wake::Ended => Err(superseded(t)),
    }
}

fn reply_row(t: &mut Task, reply: &AgentReply) -> Result<Vec<EventKind>, DurableStoreError> {
    match reply_of(t.status) {
        Reply::Waiting => match &t.wait {
            Some(wait) if answers(wait, reply) => Ok(vec![wake_into_ready(t, Some(reply.clone()))]),
            Some(wait) => Err(DurableStoreError::WrongReply {
                id: t.spec.id.clone(),
                wait: Box::new(wait.clone()),
                reply: Box::new(reply.clone()),
            }),
            None => Err(DurableStoreError::CorruptStatus {
                id: t.spec.id.clone(),
                status: "parked with no wait".into(),
            }),
        },
        Reply::NotWaiting(status) => Err(DurableStoreError::NotParked {
            id: t.spec.id.clone(),
            status,
        }),
        Reply::Ended => Err(superseded(t)),
    }
}

/// Leases `t` to `owner` under a fresh claim and returns that claim. A wake that came
/// before the claim is spent here: the run it starts sees what that wake reported.
fn claim_row(t: &mut Task, owner: &str, expires_at: Millis) -> ClaimedTask {
    let token = LeaseToken::from(Uuid::new_v4());
    t.status = Status::Running;
    t.wake_pending = false;
    t.lease = Some(Lease {
        owner: owner.to_string(),
        expires_at,
        token,
    });
    ClaimedTask::new(t, token)
}

fn row<'a>(
    tasks: &'a mut HashMap<TaskId, Task>,
    id: &TaskId,
) -> Result<&'a mut Task, DurableStoreError> {
    tasks
        .get_mut(id)
        .ok_or_else(|| DurableStoreError::NoSuchTask(id.clone()))
}

#[async_trait]
impl DurableStoreRead for MemDurableStore {
    async fn get(&self, id: &TaskId) -> Result<Option<Task>, DurableStoreError> {
        Ok(self.state().await.tasks.get(id).cloned())
    }

    async fn tasks(&self) -> Result<Vec<Task>, DurableStoreError> {
        Ok(self.state().await.tasks.values().cloned().collect())
    }

    async fn since(&self, cursor: Cursor) -> Result<Vec<Event>, DurableStoreError> {
        let (live, sealed) = {
            let st = self.state().await;
            let live: Vec<Event> = st
                .trajectory
                .iter()
                .filter(|e| e.seq > cursor.seq())
                .cloned()
                .collect();
            let mut sealed: Vec<IndexEntry> = st
                .journal_index
                .values()
                .filter(|e| e.last_seq > cursor.seq())
                .cloned()
                .collect();
            sealed.sort_by_key(|e| e.first_seq);
            (live, sealed)
        };
        // Unpaged, like the live log it merges with.
        merged_page(live, sealed, cursor, usize::MAX, &*self.journal).await
    }

    async fn trajectory(&self, task_id: &TaskId) -> Result<Vec<Event>, DurableStoreError> {
        let sealed = {
            let st = self.state().await;
            match st.journal_index.get(task_id) {
                Some(sealed) => sealed.clone(),
                None => {
                    return Ok(st
                        .trajectory
                        .iter()
                        .filter(|e| e.task_id == *task_id)
                        .cloned()
                        .collect());
                }
            }
        };
        let rows = self
            .journal
            .read_group(&sealed.file, sealed.row_group)
            .await?;
        rows.into_iter().map(event_of).collect()
    }
}

#[async_trait]
impl FiberStore for MemDurableStore {
    async fn commit(
        &self,
        id: &TaskId,
        writer: Writer,
        result: Value,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        self.state()
            .await
            .end(id, now, |t| Ok((commit_row(t, writer, result)?, ())))
    }

    async fn fail(
        &self,
        id: &TaskId,
        writer: Writer,
        failure_limit: u32,
        reason: JournaledReason,
        now: Millis,
    ) -> Result<FailedAttempt, DurableStoreError> {
        self.state().await.end(id, now, |t| {
            let failed = fail_row(t, writer, failure_limit)?;
            Ok((failure_events(failed, reason), failed))
        })
    }

    async fn enqueue_all(&self, specs: &[TaskSpec], now: Millis) -> Result<(), DurableStoreError> {
        // All-or-nothing: every seal check comes before the first write.
        let mut st = self.state().await;
        for spec in specs.iter().filter(|s| !st.tasks.contains_key(&s.id)) {
            st.refuse_if_sealed(&spec.id)?;
        }
        for spec in specs {
            st.enqueue(spec, now)?;
        }
        Ok(())
    }

    async fn append_delta(
        &self,
        writer: Writer,
        task_id: &TaskId,
        payload: Value,
        holds_delta: i64,
        at: Millis,
    ) -> Result<(), DurableStoreError> {
        let mut st = self.state().await;
        admit_append(task_id, st.tasks.get(task_id), writer)?;
        st.refuse_if_sealed(task_id)?;
        let t = row(&mut st.tasks, task_id)?;
        // A stray release must not leave the count negative.
        t.holds = t.holds.saturating_add_signed(holds_delta);
        st.push_event(task_id.clone(), EventKind::Delta { payload }, at);
        Ok(())
    }
}

#[async_trait]
impl DurableStore for MemDurableStore {
    fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    fn id(&self) -> &DurableStoreId {
        &self.id
    }

    async fn enqueue(&self, spec: &TaskSpec, now: Millis) -> Result<(), DurableStoreError> {
        self.state().await.enqueue(spec, now)
    }

    async fn claim_ready_excluding(
        &self,
        owner: &str,
        now: Millis,
        ttl: Millis,
        excluded: &[AgentKey],
    ) -> Result<Option<ClaimedTask>, DurableStoreError> {
        let mut st = self.state().await;
        // The lock makes the find and the flip one step. Which ready task is found is
        // hash order.
        let Some(id) = st
            .tasks
            .values()
            .find(|t| t.status == Status::Ready && !excluded.contains(&t.spec.agent_key))
            .map(|t| t.spec.id.clone())
        else {
            return Ok(None);
        };
        let mut claimed = None;
        st.update_with_events(&id, now, |t| {
            claimed = Some(claim_row(t, owner, now + ttl));
            Ok(vec![EventKind::Running])
        })?;
        Ok(claimed)
    }

    async fn fail_terminal(
        &self,
        id: &TaskId,
        writer: Writer,
        reason: JournaledReason,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        self.state()
            .await
            .end(id, now, |t| Ok((fail_terminal_row(t, writer, reason)?, ())))
    }

    async fn park(
        &self,
        id: &TaskId,
        writer: Writer,
        wait: &Wait,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        self.state()
            .await
            .update_with_events(id, now, |t| park_row(t, writer, wait))
    }

    async fn abandoned_holds(
        &self,
        limit: usize,
        now: Millis,
    ) -> Result<Vec<TaskId>, DurableStoreError> {
        let st = self.state().await;
        let mut out: Vec<TaskId> = st
            .tasks
            .values()
            .filter(|t| {
                crate::durable::abandoned_hold(
                    t.status,
                    t.holds,
                    t.lease.as_ref().map(|l| l.expires_at),
                    now,
                )
            })
            .map(|t| t.spec.id.clone())
            .collect();
        // Stable order: a HashMap walk would make the batch (and any test of it)
        // depend on hash order.
        out.sort();
        out.truncate(limit);
        Ok(out)
    }

    async fn wake_due(&self, now: Millis) -> Result<usize, DurableStoreError> {
        let mut st = self.state().await;
        // In id order, so one sweep's events land in the same order on every durable store; the
        // batch lands whole or not at all, so every seal check comes before the first
        // write.
        let due: BTreeSet<TaskId> = st
            .tasks
            .values()
            .filter(|t| {
                t.status == Status::Parked
                    && t.wait
                        .as_ref()
                        .and_then(Wait::until)
                        .is_some_and(|at| at <= now)
            })
            .map(|t| t.spec.id.clone())
            .collect();
        for id in &due {
            st.refuse_if_sealed(id)?;
        }
        for id in &due {
            // Nobody answered: the run this queues reads a `Woken` without a reply.
            st.update_with_events(id, now, |t| Ok(vec![wake_into_ready(t, None)]))?;
        }
        Ok(due.len())
    }

    async fn cancel_descendants(
        &self,
        parent: &TaskId,
        reason: &str,
        now: Millis,
    ) -> Result<crate::durable::Cancelled, DurableStoreError> {
        let mut st = self.state().await;
        let mut out = crate::durable::Cancelled::default();
        // `frontier` holds parents whose children have not been looked at yet; `seen`
        // stops a `continuation` cycle from spinning here forever.
        let mut frontier = vec![parent.clone()];
        let mut seen: std::collections::HashSet<TaskId> =
            std::collections::HashSet::from([parent.clone()]);
        while let Some(p) = frontier.pop() {
            let kids: Vec<TaskId> = st
                .tasks
                .values()
                .filter(|t| t.spec.continuation.as_ref() == Some(&p))
                .map(|t| t.spec.id.clone())
                .collect();
            for id in kids {
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Some(t) = st.tasks.get_mut(&id) else {
                    continue;
                };
                match t.status {
                    Status::Ready | Status::Parked => {
                        t.status = Status::Failed;
                        t.lease = None;
                        t.wait = None;
                        out.stopped.push(id.clone());
                        frontier.push(id.clone());
                    }
                    Status::Running => {
                        // It cannot be interrupted, but `commit`/`fail` refuse a
                        // terminal row, so its work is discarded instead of un-doing
                        // this cancellation. Its lease stays with the runner executing it.
                        t.status = Status::Failed;
                        out.running.push(id.clone());
                        frontier.push(id.clone());
                    }
                    // Already terminal — nothing to cancel, but its children might not be.
                    Status::Done | Status::Failed => frontier.push(id.clone()),
                }
            }
        }
        for id in out.stopped.iter().chain(&out.running) {
            st.push_event(
                id.clone(),
                EventKind::Failed {
                    reason: JournaledReason::cancelled(reason),
                },
                now,
            );
        }
        Ok(out)
    }

    async fn wake(&self, id: &TaskId, now: Millis) -> Result<(), DurableStoreError> {
        self.state().await.update_with_events(id, now, wake_row)
    }

    async fn resume(
        &self,
        id: &TaskId,
        reply: &AgentReply,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        self.state()
            .await
            .update_with_events(id, now, |t| reply_row(t, reply))
    }

    async fn reclaim_expired(&self, now: Millis) -> Result<usize, DurableStoreError> {
        let mut st = self.state().await;
        let crashed: Vec<TaskId> = st
            .tasks
            .values()
            .filter(|t| {
                t.status == Status::Running && t.lease.as_ref().is_none_or(|l| l.is_expired(now))
            })
            .map(|t| t.spec.id.clone())
            .collect();
        for id in &crashed {
            st.reclaim(id);
        }
        Ok(crashed.len())
    }

    async fn reclaim_orphans(
        &self,
        previous: Option<&str>,
        live: &[LeaseToken],
        now: Millis,
    ) -> Result<usize, DurableStoreError> {
        let mut st = self.state().await;
        let orphans: Vec<TaskId> = st
            .tasks
            .values()
            .filter(|t| t.status == Status::Running)
            .filter(|t| match &t.lease {
                None => true,
                Some(l) if live.contains(&l.token) => false,
                Some(l) => l.is_expired(now) || previous == Some(l.owner.as_str()),
            })
            .map(|t| t.spec.id.clone())
            .collect();
        for id in &orphans {
            st.reclaim(id);
        }
        Ok(orphans.len())
    }

    async fn renew_leases(
        &self,
        tokens: &[LeaseToken],
        expires_at: Millis,
    ) -> Result<usize, DurableStoreError> {
        let mut st = self.state().await;
        let held = st
            .tasks
            .values_mut()
            .filter_map(|t| t.lease.as_mut())
            .filter(|l| tokens.contains(&l.token));
        let mut renewed = 0;
        for lease in held {
            lease.expires_at = expires_at;
            renewed += 1;
        }
        Ok(renewed)
    }

    async fn release_lease(&self, id: &TaskId, token: LeaseToken) -> Result<(), DurableStoreError> {
        let mut st = self.state().await;
        if let Some(t) = st
            .tasks
            .get_mut(id)
            .filter(|t| t.lease.as_ref().is_some_and(|l| l.token == token))
        {
            t.lease = None;
        }
        Ok(())
    }

    async fn record_effect(&self, key: &str, result: &Value) -> Result<(), DurableStoreError> {
        self.state()
            .await
            .effects
            .entry(key.to_string())
            .or_insert_with(|| result.clone());
        Ok(())
    }

    async fn effect_result(&self, key: &str) -> Result<Option<Value>, DurableStoreError> {
        Ok(self.state().await.effects.get(key).cloned())
    }
}

#[async_trait]
impl SealJournals for MemDurableStore {
    async fn seal_candidates(
        &self,
        max_tasks: usize,
        max_bytes: u64,
        now: Millis,
    ) -> Result<Vec<TaskId>, DurableStoreError> {
        let candidates = self.state().await.seal_candidates(max_tasks, now)?;
        Ok(fit(candidates, max_bytes))
    }

    async fn seal_tasks(
        &self,
        task_ids: &[TaskId],
        now: Millis,
    ) -> Result<Vec<SealedJournal>, DurableStoreError> {
        if task_ids.is_empty() {
            return Ok(Vec::new());
        }
        let SealPlan {
            name,
            pending,
            rows,
        } = self.state().await.plan_seal(task_ids, now)?;

        // Lock released: other durable store writes go on while the file is written, exactly
        // the window the commit's re-check exists for.
        let staged = self.journal.stage(&name, rows).await?;

        self.state().await.commit_seal(&pending, now, &staged)
    }

    async fn remove_staged(&self, registered_by: Millis) -> Result<usize, DurableStoreError> {
        let names = self.state().await.forget_staged(registered_by);
        remove_files(&*self.journal, &names).await
    }

    async fn remove_unregistered(&self) -> Result<usize, DurableStoreError> {
        let stored = self.journal.stored().await?;
        let orphans: Vec<JournalFileName> = {
            let st = self.state().await;
            stored
                .into_iter()
                .filter(|name| !st.journal_files.contains_key(name))
                .collect()
        };
        remove_files(&*self.journal, &orphans).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::Unsealable;
    use tars_types::JobId;

    fn sealed_index(id: &TaskId) -> HashMap<TaskId, IndexEntry> {
        HashMap::from([(
            id.clone(),
            IndexEntry {
                file: JournalFileName::fresh(),
                row_group: 0,
                first_seq: 1,
                last_seq: 1,
            },
        )])
    }

    fn parked(id: &TaskId) -> HashMap<TaskId, Task> {
        let mut t = task_from(&spec(id));
        t.status = Status::Parked;
        HashMap::from([(id.clone(), t)])
    }

    #[test]
    fn a_write_whose_event_is_bound_for_a_sealed_journal_changes_nothing() {
        let mut tasks = parked(&TaskId::new("t"));
        let r = update_row_with_events(
            &mut tasks,
            &sealed_index(&TaskId::new("t")),
            &TaskId::new("t"),
            wake_row,
        );
        assert!(
            matches!(r, Err(DurableStoreError::JournalSealed { ref task_id }) if task_id.as_str() == "t"),
            "{r:?}"
        );
        assert_eq!(tasks[&TaskId::new("t")].status, Status::Parked);
    }

    #[test]
    fn a_write_to_an_unsealed_journal_lands_with_its_event() {
        let mut tasks = parked(&TaskId::new("t"));
        let events =
            update_row_with_events(&mut tasks, &HashMap::new(), &TaskId::new("t"), wake_row)
                .unwrap();
        assert_eq!(events, vec![EventKind::Woken { reply: None }]);
        assert_eq!(tasks[&TaskId::new("t")].status, Status::Ready);
    }

    #[test]
    fn a_write_with_no_event_is_not_held_up_by_a_seal() {
        let mut tasks = parked(&TaskId::new("t"));
        tasks.get_mut(&TaskId::new("t")).unwrap().status = Status::Running;
        let events = update_row_with_events(
            &mut tasks,
            &sealed_index(&TaskId::new("t")),
            &TaskId::new("t"),
            wake_row,
        )
        .unwrap();
        assert!(events.is_empty());
        assert!(tasks[&TaskId::new("t")].wake_pending);
    }
    use serde_json::json;

    fn spec(id: &TaskId) -> TaskSpec {
        TaskSpec::new(
            id.clone(),
            JobId::new("job"),
            AgentKey::new("agent"),
            json!(id),
        )
    }

    #[tokio::test]
    async fn trajectory_is_bounded_oldest_dropped() {
        // A long-lived MemDurableStore must not grow the trajectory without limit: past the
        // cap, the oldest events are dropped (never OOM).
        let b = MemDurableStore::new(tars_types::TenantId::new("test"));
        b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
        let extra = 10usize;
        for _ in 0..(MemDurableStore::TRAJECTORY_CAP + extra) {
            b.append_delta(Writer::Unclaimed, &TaskId::new("a"), json!(null), 0, 0)
                .await
                .unwrap();
        }
        let all = b.since(Cursor::START).await.unwrap();
        assert_eq!(
            all.len(),
            MemDurableStore::TRAJECTORY_CAP,
            "trajectory is capped at TRAJECTORY_CAP"
        );
        assert_eq!(
            all.first().unwrap().seq,
            extra as u64 + 2,
            "the `Enqueued` and the oldest deltas dropped, seq monotonic"
        );
    }

    /// The cap can drop every event of a task, so a terminal task with no journal rows is
    /// reachable here and S(T) must refuse it.
    #[tokio::test]
    async fn a_task_whose_whole_journal_the_cap_dropped_is_not_sealed() {
        let b = MemDurableStore::new(tars_types::TenantId::new("test"));
        b.enqueue(&spec(&TaskId::new("quiet")), 0).await.unwrap();
        b.commit(&TaskId::new("quiet"), Writer::Unclaimed, json!(null), 0)
            .await
            .unwrap();
        assert_eq!(
            b.seal_candidates(10, u64::MAX, 0).await.unwrap(),
            [TaskId::new("quiet")],
            "premise: with its journal it is a candidate"
        );

        b.enqueue(&spec(&TaskId::new("filler")), 0).await.unwrap();
        for n in 0..MemDurableStore::TRAJECTORY_CAP {
            b.append_delta(Writer::Unclaimed, &TaskId::new("filler"), json!(n), 0, 0)
                .await
                .unwrap();
        }
        assert!(
            b.trajectory(&TaskId::new("quiet"))
                .await
                .unwrap()
                .is_empty(),
            "premise"
        );

        assert!(b.seal_candidates(10, u64::MAX, 0).await.unwrap().is_empty());
        match b.seal_tasks(&[TaskId::new("quiet")], 0).await {
            Err(DurableStoreError::Unsealable { task_id, why }) => {
                assert_eq!((task_id.as_str(), why), ("quiet", Unsealable::EmptyJournal))
            }
            other => panic!("expected EmptyJournal, got {other:?}"),
        }
    }
}
