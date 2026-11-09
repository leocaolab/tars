//! Decides when a task's journal may be sealed: the rule S(T), a pure function both
//! durable stores call once to pick the rows and again inside the commit to prove nothing
//! changed while the file was being written.
//!
//! S(T): the task is terminal, owes nothing (`holds = 0`), its parent is absent or
//! terminal, no child of it is still open, and no runner holds a live lease on it.
//! Every decision path that reads a journal is then done with this one
//! (`docs/design/log-parquet.md` §3.1), so moving it out of the durable store changes no
//! decision.

use crate::durable::journal::{JournalFileName, JournalRow, StagedFile, TaskGroup, TrajectoryLog};
use tars_types::{Event, EventKind, Millis, Status, Task, TaskId};

use crate::durable::{Cursor, DurableStoreError, stored_id};

/// What the durable store read about one task, at one moment, to judge S(T).
pub(crate) struct SealFacts {
    pub(crate) task: Task,
    /// `None` when the task has no parent — or names one the durable store has no row for;
    /// [`check`] tells those apart through `task.spec.continuation`.
    pub(crate) parent: Option<Status>,
    pub(crate) open_children: u64,
    pub(crate) sealed: bool,
    /// The task's newest journal `seq`; `None` = it has no journal.
    pub(crate) high_water: Option<u64>,
}

/// Why a task's journal was not sealed. Nothing visible changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsealable {
    #[error("task is `{}`, not terminal", .0.as_str())]
    NotTerminal(Status),
    #[error("task still holds {0} resource(s)")]
    Holds(u64),
    #[error("parent `{parent}` is still `{}`", status.as_str())]
    ParentOpen { parent: TaskId, status: Status },
    #[error("parent `{0}` has no row")]
    ParentMissing(TaskId),
    #[error("{0} child task(s) still open")]
    ChildrenOpen(u64),
    #[error("a runner still holds its lease until {0}")]
    LeaseLive(Millis),
    #[error("already sealed")]
    AlreadySealed,
    #[error("no journal rows to seal")]
    EmptyJournal,
    /// A row was appended (or removed) between reading the journal and committing.
    #[error("journal moved from seq {staged} to {now:?} while staging")]
    JournalMoved { staged: u64, now: Option<u64> },
    /// The file's `staged` registration is gone — cleaned up as an orphan meanwhile.
    #[error("staged file `{}` is no longer registered as staged", .0.as_str())]
    StagedFileGone(JournalFileName),
}

/// Where a sealed task's journal now lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedJournal {
    pub task_id: TaskId,
    pub file: JournalFileName,
    pub row_group: u32,
}

/// A `journal_index` entry: which file holds a sealed task's journal, and the `seq`
/// span it covers — what a merged read of the global stream needs to know without
/// opening the file.
#[derive(Debug, Clone)]
pub(crate) struct IndexEntry {
    pub(crate) file: JournalFileName,
    pub(crate) row_group: u32,
    pub(crate) first_seq: u64,
    pub(crate) last_seq: u64,
}

impl IndexEntry {
    pub(crate) fn of(file: &JournalFileName, group: &TaskGroup) -> Self {
        Self {
            file: file.clone(),
            row_group: group.row_group,
            first_seq: group.first_seq,
            last_seq: group.last_seq,
        }
    }
}

/// A task S(T) picked, and how much its journal weighs in a file.
pub(crate) struct Candidate {
    pub(crate) task_id: TaskId,
    pub(crate) bytes: u64,
}

/// What the pre-check saw of one task: the newest `seq` the staged rows end at.
pub(crate) struct Pending {
    pub(crate) task_id: TaskId,
    pub(crate) high_water: u64,
}

impl Pending {
    pub(crate) fn of(task_id: &TaskId, rows: &[JournalRow]) -> Result<Self, DurableStoreError> {
        let high_water = rows
            .last()
            .map(|r| r.seq)
            .ok_or_else(|| refused(task_id, Unsealable::EmptyJournal))?;
        Ok(Self {
            task_id: task_id.clone(),
            high_water,
        })
    }
}

/// What a batch's pre-check hands to staging: the file's name, registered
/// `staged`, every task's journal rows, and what the commit must find unchanged.
pub(crate) struct SealPlan {
    pub(crate) name: JournalFileName,
    pub(crate) pending: Vec<Pending>,
    pub(crate) rows: Vec<JournalRow>,
}

impl SealPlan {
    /// Judges each task's facts with S(T) and takes its rows; any refusal refuses the
    /// batch.
    pub(crate) fn of(
        journals: Vec<(SealFacts, Vec<JournalRow>)>,
        now: Millis,
    ) -> Result<Self, DurableStoreError> {
        let checked = journals
            .into_iter()
            .map(|(facts, rows)| {
                let task_id = facts.task.spec.id.clone();
                check(&facts, now).map_err(|why| refused(&task_id, why))?;
                Ok((Pending::of(&task_id, &rows)?, rows))
            })
            .collect::<Result<Vec<_>, DurableStoreError>>()?;
        let (pending, rows): (Vec<Pending>, Vec<Vec<JournalRow>>) = checked.into_iter().unzip();
        Ok(Self {
            name: JournalFileName::fresh(),
            pending,
            rows: rows.concat(),
        })
    }
}

/// Re-checks a batch at commit: every task against the facts read inside the commit,
/// and the file still registered `staged`. Returns each task's index entry, in the
/// batch's order.
pub(crate) fn recheck_batch(
    pending: &[Pending],
    facts: Vec<SealFacts>,
    file_staged: bool,
    staged: &StagedFile,
    now: Millis,
) -> Result<Vec<(TaskId, IndexEntry)>, DurableStoreError> {
    pending
        .iter()
        .zip(facts)
        .map(|(p, f)| {
            let group = group_of(staged, &p.task_id)?;
            recheck(&f, now, p.high_water).map_err(|why| refused(&p.task_id, why))?;
            if !file_staged {
                let gone = Unsealable::StagedFileGone(staged.name.clone());
                return Err(refused(&p.task_id, gone));
            }
            Ok((p.task_id.clone(), IndexEntry::of(&staged.name, group)))
        })
        .collect()
}

/// Returns what the caller is told about each sealed task.
pub(crate) fn sealed_journals(entries: &[(TaskId, IndexEntry)]) -> Vec<SealedJournal> {
    entries
        .iter()
        .map(|(task_id, e)| SealedJournal {
            task_id: task_id.clone(),
            file: e.file.clone(),
            row_group: e.row_group,
        })
        .collect()
}

/// Deletes these files, whose registrations are already gone.
pub(crate) async fn remove_files(
    journal: &dyn TrajectoryLog,
    names: &[JournalFileName],
) -> Result<usize, DurableStoreError> {
    for name in names {
        journal.remove(name).await?;
    }
    Ok(names.len())
}

/// Returns one row's weight, as the journal store counts it when it cuts files.
pub(crate) fn row_bytes(row: &JournalRow) -> u64 {
    row.approx_bytes() as u64
}

/// Takes candidates in order while their journals together stay within `max_bytes`.
/// The first always goes in: a journal bigger than the cap still has to leave the
/// durable store some day, and alone is the smallest file it can make.
pub(crate) fn fit(candidates: Vec<Candidate>, max_bytes: u64) -> Vec<TaskId> {
    candidates
        .into_iter()
        .scan(0u64, |total, c| {
            *total += c.bytes;
            Some((*total, c.task_id))
        })
        .enumerate()
        .take_while(|(i, (total, _))| *i == 0 || *total <= max_bytes)
        .map(|(_, (_, task_id))| task_id)
        .collect()
}

/// Returns one page of the global stream after `cursor`: `live` rows merged by `seq`
/// with the sealed journals `sealed` names, at most `limit` events.
///
/// `live` and `sealed` must come from ONE snapshot, or a seal committing between the
/// two reads loses its rows from the page (or shows them twice). `sealed` is ordered
/// by `first_seq`, so once the page is full and ends before the next file begins, no
/// later file can change it and none is opened.
pub(crate) async fn merged_page(
    live: Vec<Event>,
    sealed: Vec<IndexEntry>,
    cursor: Cursor,
    limit: usize,
    journal: &dyn TrajectoryLog,
) -> Result<Vec<Event>, DurableStoreError> {
    let mut page = settle(live, limit);
    for entry in sealed {
        if settled_before(&page, limit, entry.first_seq) {
            break;
        }
        let rows = journal.read_group(&entry.file, entry.row_group).await?;
        let events = rows
            .into_iter()
            .filter(|r| r.seq > cursor.seq())
            .map(event_of)
            .collect::<Result<Vec<_>, _>>()?;
        page = settle(page.into_iter().chain(events).collect(), limit);
    }
    Ok(page)
}

fn settle(mut events: Vec<Event>, limit: usize) -> Vec<Event> {
    events.sort_by_key(|e| e.seq);
    events.truncate(limit);
    events
}

fn settled_before(page: &[Event], limit: usize, seq: u64) -> bool {
    page.len() >= limit && page.last().is_some_and(|e| e.seq < seq)
}

/// Judges S(T) over the facts at `now`.
pub(crate) fn check(f: &SealFacts, now: Millis) -> Result<(), Unsealable> {
    let t = &f.task;
    if !t.status.is_terminal() {
        return Err(Unsealable::NotTerminal(t.status));
    }
    if t.holds > 0 {
        return Err(Unsealable::Holds(t.holds));
    }
    if let Some(parent) = &t.spec.continuation {
        match f.parent {
            None => return Err(Unsealable::ParentMissing(parent.clone())),
            Some(status) if !status.is_terminal() => {
                return Err(Unsealable::ParentOpen {
                    parent: parent.clone(),
                    status,
                });
            }
            Some(_) => {}
        }
    }
    if f.open_children > 0 {
        return Err(Unsealable::ChildrenOpen(f.open_children));
    }
    if let Some(lease) = t.lease.as_ref().filter(|l| !l.is_expired(now)) {
        return Err(Unsealable::LeaseLive(lease.expires_at));
    }
    if f.sealed {
        return Err(Unsealable::AlreadySealed);
    }
    match f.high_water {
        Some(_) => Ok(()),
        None => Err(Unsealable::EmptyJournal),
    }
}

/// Re-checks at commit that S(T) still holds and the journal is the one staged.
pub(crate) fn recheck(f: &SealFacts, now: Millis, staged: u64) -> Result<(), Unsealable> {
    check(f, now)?;
    if f.high_water != Some(staged) {
        return Err(Unsealable::JournalMoved {
            staged,
            now: f.high_water,
        });
    }
    Ok(())
}

/// Wraps a refusal as the durable store reports it.
pub(crate) fn refused(task_id: &TaskId, why: Unsealable) -> DurableStoreError {
    DurableStoreError::Unsealable {
        task_id: task_id.clone(),
        why,
    }
}

/// Finds the staged group holding `task_id`'s journal.
pub(crate) fn group_of<'a>(
    staged: &'a StagedFile,
    task_id: &TaskId,
) -> Result<&'a TaskGroup, DurableStoreError> {
    staged
        .groups
        .iter()
        .find(|g| g.task_id == task_id.as_str())
        .ok_or_else(|| DurableStoreError::NotStaged {
            task_id: task_id.clone(),
            file: staged.name.as_str().to_string(),
        })
}

/// Turns a journal row, live or sealed, back into the event it was.
pub(crate) fn event_of(row: JournalRow) -> Result<Event, DurableStoreError> {
    let task_id = stored_id("task_id", row.task_id)?;
    match serde_json::from_str::<EventKind>(&row.payload) {
        Ok(kind) => Ok(Event {
            seq: row.seq,
            task_id,
            kind,
            at: row.at,
        }),
        Err(source) => Err(DurableStoreError::CorruptEvent {
            seq: row.seq,
            task_id,
            kind: row.payload,
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tars_types::{AgentKey, JobId, Lease, LeaseToken, TaskSpec};

    use super::*;

    fn facts(status: Status) -> SealFacts {
        let task = Task {
            spec: TaskSpec::new(
                TaskId::new("t"),
                JobId::new("j"),
                AgentKey::new("k"),
                json!(null),
            ),
            status,
            lease: None,
            attempt: 0,
            wait: None,
            holds: 0,
            interrupted: None,
            wake_pending: false,
            reply: None,
            result: None,
        };
        SealFacts {
            task,
            parent: None,
            open_children: 0,
            sealed: false,
            high_water: Some(7),
        }
    }

    #[test]
    fn a_terminal_task_owing_nothing_with_no_family_is_sealable() {
        assert_eq!(check(&facts(Status::Done), 0), Ok(()));
        assert_eq!(check(&facts(Status::Failed), 0), Ok(()));
    }

    #[test]
    fn each_clause_of_s_t_refuses_on_its_own() {
        assert_eq!(
            check(&facts(Status::Running), 0),
            Err(Unsealable::NotTerminal(Status::Running))
        );

        let mut f = facts(Status::Done);
        f.task.holds = 1;
        assert_eq!(check(&f, 0), Err(Unsealable::Holds(1)));

        let mut f = facts(Status::Done);
        f.task.spec.continuation = Some(TaskId::new("p"));
        assert_eq!(
            check(&f, 0),
            Err(Unsealable::ParentMissing(TaskId::new("p")))
        );
        f.parent = Some(Status::Parked);
        assert!(matches!(check(&f, 0), Err(Unsealable::ParentOpen { .. })));
        f.parent = Some(Status::Done);
        assert_eq!(check(&f, 0), Ok(()));

        let mut f = facts(Status::Done);
        f.open_children = 2;
        assert_eq!(check(&f, 0), Err(Unsealable::ChildrenOpen(2)));

        let mut f = facts(Status::Done);
        f.task.lease = Some(Lease {
            owner: "r".into(),
            expires_at: 100,
            token: LeaseToken::from(uuid::Uuid::nil()),
        });
        assert_eq!(check(&f, 99), Err(Unsealable::LeaseLive(100)));
        assert_eq!(check(&f, 100), Ok(()), "an expired lease holds nothing");

        let mut f = facts(Status::Done);
        f.sealed = true;
        assert_eq!(check(&f, 0), Err(Unsealable::AlreadySealed));

        let mut f = facts(Status::Done);
        f.high_water = None;
        assert_eq!(check(&f, 0), Err(Unsealable::EmptyJournal));
    }

    fn candidate(task_id: &'static str, bytes: u64) -> Candidate {
        Candidate {
            task_id: TaskId::new(task_id),
            bytes,
        }
    }

    #[test]
    fn a_batch_stops_before_the_byte_cap_but_always_takes_the_first() {
        let three = || vec![candidate("a", 40), candidate("b", 50), candidate("c", 20)];
        assert_eq!(fit(three(), 100), vec![TaskId::new("a"), TaskId::new("b")]);
        assert_eq!(
            fit(three(), 110),
            vec![TaskId::new("a"), TaskId::new("b"), TaskId::new("c")]
        );
        assert_eq!(
            fit(vec![candidate("big", 500), candidate("b", 1)], 100),
            vec![TaskId::new("big")]
        );
        assert!(fit(Vec::new(), 100).is_empty());
    }

    #[test]
    fn the_recheck_refuses_a_journal_that_moved_since_staging() {
        assert_eq!(recheck(&facts(Status::Done), 0, 7), Ok(()));
        assert_eq!(
            recheck(&facts(Status::Done), 0, 6),
            Err(Unsealable::JournalMoved {
                staged: 6,
                now: Some(7)
            })
        );
    }

    fn journal_row(payload: &str) -> JournalRow {
        JournalRow {
            task_id: "a".to_string(),
            seq: 7,
            at: 0,
            payload: payload.to_string(),
        }
    }

    #[test]
    fn a_sealed_row_with_an_unknown_kind_is_corrupt_with_its_text() {
        let known = serde_json::to_string(&EventKind::Running).unwrap();
        assert_eq!(
            event_of(journal_row(&known)).unwrap().kind,
            EventKind::Running
        );

        match event_of(journal_row("\"NoSuchKind\"")).unwrap_err() {
            DurableStoreError::CorruptEvent {
                seq, task_id, kind, ..
            } => assert_eq!(
                (seq, task_id.as_str(), kind.as_str()),
                (7, "a", "\"NoSuchKind\"")
            ),
            other => panic!("expected CorruptEvent, got {other:?}"),
        }
    }
}
