//! Defines the durable store's records: a [`Task`] (one agent instance and its status) and the
//! values around it.
//!
//! A task is on the durable store only once it is runnable, and the only link between tasks the
//! durable store keeps is a task's `continuation`, the task to wake when it ends; a dependency
//! graph lives in an orchestrator's memory (tars-runtime's `pattern`). Nothing here does IO or
//! reads a clock: time is epoch milliseconds passed in from the shell.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{AgentKey, JobId, TaskId};

/// A runner instance's id — the `owner` of a [`Lease`].
pub type RunnerId = String;
/// Wall-clock epoch milliseconds, passed in from the shell. Unlike `Instant` it can be stored,
/// so a lease survives a restart.
pub type Millis = i64;

/// Why a clock reading is not a time a [`Millis`] can hold.
#[derive(Debug, thiserror::Error)]
pub enum ClockError {
    /// The clock reads before the Unix epoch, as a clock that is not set does (a machine
    /// without an RTC just after boot).
    #[error("the system clock reads before the Unix epoch")]
    BeforeEpoch(#[source] std::time::SystemTimeError),
    #[error("{ms} ms since the Unix epoch does not fit an i64")]
    Overflow { ms: u128 },
}

/// Returns `t` in milliseconds since the Unix epoch.
///
/// # Errors
///
/// Fails with [`ClockError`] when `t` is before the epoch or past what an `i64` holds, rather
/// than a `0` or a wrapped stamp.
pub fn unix_millis(t: std::time::SystemTime) -> Result<Millis, ClockError> {
    let ms = t
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(ClockError::BeforeEpoch)?
        .as_millis();
    Millis::try_from(ms).map_err(|_| ClockError::Overflow { ms })
}

/// A task's lifecycle status. A task is born [`Ready`](Status::Ready), because it is on the durable
/// store only once it is runnable. The durable store stores it as its [`as_str`](Status::as_str)
/// text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Unclaimed and runnable — a runner may claim it.
    Ready,
    /// Leased by a runner, its agent in flight.
    Running,
    /// Result committed — terminal, never recomputed.
    Done,
    /// Gave up — terminal.
    Failed,
    /// Suspended until a wake: a human reply, or a continuation child ending.
    Parked,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Parked => "parked",
        }
    }

    /// Parses a stored status; `None` for text the schema never writes (a corrupt or foreign
    /// row), which the caller reports as an error.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ready" => Self::Ready,
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "parked" => Self::Parked,
            _ => return None,
        })
    }

    /// Returns whether the status holds no more work (`Done` or `Failed`).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// A runner's claim on a task; one `owner` at a time holds a live lease. It expires, and a
/// crashed runner's task can be reclaimed, once `now` reaches `expires_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub owner: RunnerId,
    pub expires_at: Millis,
    /// Which claim this lease belongs to.
    pub token: LeaseToken,
}

/// One claim of one task: minted by the durable store when it leases the task, carried by the
/// attempt that claim started, and the only key that renews or gives back that lease.
///
/// It is neither the owner nor the task id, because the same runner can claim the same task
/// again while its previous attempt is still winding up (fail, re-armed, re-claimed at once).
/// Keyed by either, the old attempt ending would look like the new one ending and take the new
/// one's lease with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LeaseToken(Uuid);

impl LeaseToken {
    /// Returns the token as a durable store stores it. The durable store also mints it
    /// (`From<Uuid>`), since this module draws no randomness.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for LeaseToken {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

impl Lease {
    pub fn is_expired(&self, now: Millis) -> bool {
        now >= self.expires_at
    }
}

/// A task that died while running, again and again, in the same place: where, and how
/// many times in a row.
///
/// Resuming is the runtime's job — a reclaimed task replays its journal and carries on
/// from the step it was in. That same property turns a step which itself kills the
/// process (it outruns a supervisor's watchdog, it exhausts the host's memory) into an
/// endless loop: every restart walks straight back into it and dies there again. The
/// only thing that can see the loop is whoever resumes, so the count lives on the task
/// and is written by the reclaim that observes each interruption.
///
/// "The same place" is the task's journal position: how many
/// [`Delta`](EventKind::Delta) entries its trajectory held when it was interrupted.
/// Every completed step writes one, so an unchanged position means nothing finished
/// between two interruptions — the task died in the step that would have written the
/// next entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptionStreak {
    /// How many journal entries the task held when it was interrupted.
    pub at: u64,
    /// Consecutive interruptions at `at`. Never zero: a task that was never interrupted
    /// has no `InterruptionStreak` at all.
    pub times: u32,
}

impl InterruptionStreak {
    /// Returns the streak after one more interruption, observed with `at` journal entries.
    ///
    /// Progress is what breaks a streak: an interruption at a different position is the
    /// first of a new one, not a continuation of the old.
    pub fn after(previous: Option<InterruptionStreak>, at: u64) -> Self {
        match previous {
            Some(p) if p.at == at => Self {
                at,
                times: p.times.saturating_add(1),
            },
            _ => Self { at, times: 1 },
        }
    }
}

/// The input to enqueue a task. It has no result (that lands on the task's row when it ends)
/// and no dependency fields, since a task is enqueued only once it is runnable.
#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub id: TaskId,
    pub job_id: JobId,
    /// Registry key naming which agent runs this task.
    pub agent_key: AgentKey,
    /// The payload the agent's step pursues.
    pub intent: Value,
    /// The registry key of the agent that judges this task's work; `None` when the acting
    /// agent judges itself.
    pub judge: Option<AgentKey>,
    /// The task to wake when this one reaches a terminal state; `None` when nothing waits on it.
    pub continuation: Option<TaskId>,
}

impl TaskSpec {
    pub fn new(id: TaskId, job_id: JobId, agent_key: AgentKey, intent: Value) -> Self {
        Self {
            id,
            job_id,
            agent_key,
            intent,
            judge: None,
            continuation: None,
        }
    }

    pub fn judged_by(mut self, judge: AgentKey) -> Self {
        self.judge = Some(judge);
        self
    }

    pub fn waking(mut self, continuation: TaskId) -> Self {
        self.continuation = Some(continuation);
        self
    }
}

/// A whole persisted task row, as a backend loads it for reporting, a cold-restart frontier
/// rebuild, or a continuation's durable store read. The immutable identity is the embedded
/// [`spec`](Task::spec); the other fields are the task's mutable state.
#[derive(Clone, Debug)]
pub struct Task {
    pub spec: TaskSpec,
    pub status: Status,
    pub lease: Option<Lease>,
    pub attempt: u32,
    /// What a [`Parked`](Status::Parked) task waits for, `None` while it is not parked. A wait
    /// for a person carries when the task becomes claimable again on its own, so a question
    /// nobody answers does not park the task forever; that is on the task, not in the agent's
    /// journal, because a parked task holds no lease and runs no code that could notice the
    /// time, and the durable store sweeps for it as it sweeps for expired leases.
    pub wait: Option<crate::Wait>,
    /// Records a wake that arrived while the task was still `Running`, before it parked, so the
    /// park is re-armed and a racing continuation wake is not lost. It is cleared when the wake is
    /// consumed.
    pub wake_pending: bool,
    /// How many resources this task has taken through `acquire` and not yet released.
    ///
    /// It indexes the journal and is written in the same transaction as the `acquire:` /
    /// `release:` delta it counts, so "which terminal tasks still owe a release" is an indexed
    /// query rather than a decode of every trajectory.
    pub holds: u64,
    /// Where this task was last interrupted and how many times in a row, `None` when it never
    /// was; the reclaim that puts an interrupted task back to `Ready` writes it (see
    /// [`InterruptionStreak`]).
    pub interrupted: Option<InterruptionStreak>,
    /// A human reply committed for a parked task, stored on the row so it survives a restart.
    pub reply: Option<crate::AgentReply>,
    /// The committed result (a `Success`'s serialized `Refs`), present iff `Done`.
    pub result: Option<Value>,
}

impl Task {
    /// Decodes the `data` this task committed into `T`, for a reader of a root task's result (a
    /// CLI, a test, an HTTP handler) that has only the durable store row; a parent reads its
    /// children through tars-runtime's `fiber::join::data` instead.
    ///
    /// # Errors
    ///
    /// Returns `Reason::Defect` carrying what was there when the task committed no result, the
    /// result has no `data`, or the data does not decode into `T`: the data is what a task of
    /// ours stored.
    pub fn data<T: serde::de::DeserializeOwned>(&self) -> Result<T, crate::Reason> {
        let Some(result) = self.result.as_ref() else {
            return Err(crate::Reason::Defect {
                raw: Some(format!("{:?}", self.status)),
                detail: format!(
                    "task `{}` committed no result (it is {:?})",
                    self.spec.id, self.status
                ),
            });
        };
        let Some(data) = result.get("data").filter(|d| !d.is_null()) else {
            return Err(crate::Reason::Defect {
                raw: Some(result.to_string()),
                detail: format!("task `{}` committed no `data`", self.spec.id),
            });
        };
        serde_json::from_value(data.clone()).map_err(|e| crate::Reason::Defect {
            raw: Some(data.to_string()),
            detail: format!(
                "task `{}` data does not decode into {}: {e}",
                self.spec.id,
                std::any::type_name::<T>()
            ),
        })
    }
}

/// A task handed to a runner to execute: the part of a [`Task`] the runner needs to rebuild the
/// agent, run it and commit.
#[derive(Clone, Debug)]
pub struct ClaimedTask {
    pub spec: TaskSpec,
    /// The human reply for a resumed human-park, if any.
    pub reply: Option<crate::AgentReply>,
    /// The interruption streak the runner checks before resuming the task into that step again.
    pub interrupted: Option<InterruptionStreak>,
    /// The token the attempt renews its lease by and is known by while it runs.
    pub token: LeaseToken,
}

impl ClaimedTask {
    /// Returns the claim `token` made of `t`.
    pub fn new(t: &Task, token: LeaseToken) -> Self {
        Self {
            spec: t.spec.clone(),
            reply: t.reply.clone(),
            interrupted: t.interrupted,
            token,
        }
    }
}

/// One entry in a task's append-only stream of actions and lifecycle changes, the timeline the
/// UI renders. It is stored in a JSON column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// The task was put on the durable store (born `Ready`).
    Enqueued,
    /// A runner claimed it and started its agent.
    Running,
    /// Its result was committed — terminal.
    Done,
    /// It gave up — terminal. This is the only copy of why the task failed that outlives the
    /// process (the row keeps no reason), so it carries the producer's words together with the
    /// class and handler a caller branches on to decide whether to re-drive it or show it to a
    /// model.
    Failed { reason: crate::JournaledReason },
    /// It suspended for a wake, waiting for `wait`.
    Parked { wait: crate::Wait },
    /// A parked task was woken (a human reply, or a continuation-child completed).
    Woken { reply: Option<crate::AgentReply> },
    /// A payload the agent writes and the runtime carries without reading, such as one
    /// conversation turn or tool result. On resume the agent rebuilds its transcript from its
    /// own `Delta`s.
    Delta { payload: Value },
}

/// One entry in the trajectory stream. `seq` increases monotonically and is the `since` cursor;
/// `at` is epoch milliseconds supplied by the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    pub task_id: TaskId,
    pub kind: EventKind,
    pub at: Millis,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_zero_and_a_millisecond_before_it_is_refused() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(unix_millis(UNIX_EPOCH).unwrap(), 0);
        assert!(matches!(
            unix_millis(UNIX_EPOCH - Duration::from_millis(1)),
            Err(ClockError::BeforeEpoch(_))
        ));
    }

    #[test]
    fn a_time_past_what_an_i64_holds_is_refused_not_wrapped() {
        use std::time::{Duration, UNIX_EPOCH};
        let last = Duration::from_millis(i64::MAX as u64);
        let Some(past) = UNIX_EPOCH.checked_add(last + Duration::from_millis(1)) else {
            return;
        };
        assert_eq!(unix_millis(UNIX_EPOCH + last).unwrap(), i64::MAX);
        assert!(matches!(
            unix_millis(past),
            Err(ClockError::Overflow { .. })
        ));
    }

    #[test]
    fn status_round_trips_through_text() {
        for s in [
            Status::Ready,
            Status::Running,
            Status::Done,
            Status::Failed,
            Status::Parked,
        ] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        assert_eq!(Status::parse("pending"), None, "no Pending status exists");
        assert_eq!(Status::parse("garbage"), None);
    }

    #[test]
    fn lease_expiry_is_inclusive() {
        let l = Lease {
            owner: "r".into(),
            expires_at: 100,
            token: Uuid::nil().into(),
        };
        assert!(!l.is_expired(99));
        assert!(l.is_expired(100));
        assert!(l.is_expired(101));
    }

    #[test]
    fn an_interruption_at_the_same_position_extends_the_streak_and_progress_starts_a_new_one() {
        let first = InterruptionStreak::after(None, 4);
        assert_eq!(first, InterruptionStreak { at: 4, times: 1 });
        let again = InterruptionStreak::after(Some(first), 4);
        assert_eq!(again, InterruptionStreak { at: 4, times: 2 });
        assert_eq!(
            InterruptionStreak::after(Some(again), 5),
            InterruptionStreak { at: 5, times: 1 },
            "a step finished in between: a new streak"
        );
    }

    #[test]
    fn spec_defaults_to_no_continuation_and_waking_sets_it() {
        let s = TaskSpec::new(
            TaskId::new("a"),
            JobId::new("job"),
            AgentKey::new("agent"),
            Value::Null,
        );
        assert!(s.continuation.is_none());
        assert_eq!(
            s.waking(TaskId::new("orch")).continuation,
            Some(TaskId::new("orch"))
        );
    }

    #[test]
    fn event_kind_serializes_tagged() {
        let reason = crate::JournaledReason::from(&crate::Reason::Execution("boom".into()));
        let j = serde_json::to_string(&EventKind::Failed {
            reason: reason.clone(),
        })
        .unwrap();
        assert!(j.contains("\"kind\":\"failed\""));
        let back: EventKind = serde_json::from_str(&j).unwrap();
        assert_eq!(back, EventKind::Failed { reason });
    }

    #[test]
    fn failed_event_round_trips_its_classification() {
        let original = crate::Reason::defect("durable store unreachable");
        let ev = EventKind::Failed {
            reason: crate::JournaledReason::from(&original),
        };
        let back: EventKind = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        let EventKind::Failed { reason } = back else {
            panic!("not a Failed event")
        };
        assert_eq!(reason.class, original.error_class());
        assert_eq!(reason.handler, original.audience());
        assert_eq!(
            reason.text,
            original.to_string(),
            "the producer's own words"
        );
    }

    fn row(status: Status, result: Option<Value>) -> Task {
        Task {
            spec: TaskSpec::new(
                TaskId::new("t"),
                JobId::new("job"),
                AgentKey::new("agent"),
                Value::Null,
            ),
            status,
            lease: None,
            attempt: 0,
            wait: None,
            wake_pending: false,
            holds: 0,
            interrupted: None,
            reply: None,
            result,
        }
    }

    /// The row is our own durable store's, so a model could not fix any of these.
    #[test]
    fn a_task_row_whose_data_cannot_be_read_never_reaches_the_model() {
        let rows = [
            row(Status::Failed, None),
            row(Status::Done, Some(serde_json::json!({ "summary": "s" }))),
            row(
                Status::Done,
                Some(serde_json::json!({ "summary": "s", "data": "not a number" })),
            ),
        ];
        for t in rows {
            let err = t.data::<u32>().expect_err("nothing readable as a u32");
            assert!(!err.may_reach_model(), "{err:?}");
        }
    }
}
