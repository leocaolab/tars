//! The laws a durable store keeps (the "Laws" section of `tars_storage::durable`), each checked the
//! same way on every backend. A new backend passes this file before anything runs on it.
//!
//! Each function checks one law, or one part of it, over a fresh durable store, and its cases
//! share that durable store. So no case leaves a task `Ready` behind: a claim promises no order, and
//! the next case's claim must find only the task that case enqueued.
use std::sync::Arc;

use serde_json::{Value, json};
use tars_storage::durable::{
    Cursor, DurableStore, DurableStoreError, FailedAttempt, MemDurableStore, SealJournals,
    SqliteDurableStore, UnparkedStatus, Writer,
};
use tars_types::{
    AgentAsk, AgentKey, AgentReply, Approver, Approvers, Event, EventKind, InterruptionStreak,
    JobId, JournaledReason, LeaseToken, Millis, Reason, Status, Task, TaskId, TaskSpec, TenantId,
    Wait,
};

mod common;
use common::holder;

/// Returns a park on `question`, put to `ana`, that times out at `until`.
fn asked(question: &str, until: Millis) -> Wait {
    Wait::Answer {
        ask: AgentAsk::Question {
            question: question.into(),
        },
        from: Approvers::one(ana()),
        until,
    }
}

fn ana() -> Approver {
    Approver::Person { id: "ana".into() }
}

/// Returns `ana`'s answer `value`.
fn answer(value: Value) -> AgentReply {
    AgentReply::Answer { by: ana(), value }
}

fn mem_store() -> Arc<MemDurableStore> {
    Arc::new(MemDurableStore::new(TenantId::new("test")))
}

async fn sqlite_store() -> Arc<SqliteDurableStore> {
    Arc::new(
        SqliteDurableStore::in_memory(TenantId::new("test"))
            .await
            .expect("an in-memory durable store opens"),
    )
}

/// Runs every law once on each backend, as `mem::<law>` and `sqlite::<law>`.
macro_rules! on_every_store {
    ($($law:ident),+ $(,)?) => {
        mod mem {
            $(
                #[tokio::test]
                async fn $law() {
                    super::$law(super::mem_store()).await
                }
            )+
        }
        mod sqlite {
            $(
                #[tokio::test]
                async fn $law() {
                    super::$law(super::sqlite_store().await).await
                }
            )+
        }
    };
}

on_every_store!(
    law_append_only,
    law_lifecycle_events,
    law_idempotent,
    law_admission,
    law_one_woken_per_new_run,
    law_an_ending_wakes_its_continuation,
    law_sealed_journal_refuses_appends,
    sweep_wake_due_wakes_only_parks_whose_deadline_came,
    sweep_wake_due_skips_a_park_a_reply_or_a_wake_ended,
    sweep_cancel_descendants_walks_the_subtree_and_splits_the_report,
    sweep_cancel_descendants_stops_on_a_continuation_cycle,
    sweep_abandoned_holds_leaves_a_task_that_can_still_run,
    sweep_abandoned_holds_waits_out_the_lease_and_honours_the_limit,
    holds_move_only_with_the_delta_they_count,
    holds_never_go_below_zero,
    lease_a_ready_task_is_claimed_once,
    lease_reclaim_expired_takes_only_leases_that_ran_out,
    lease_reclaim_orphans_takes_expired_and_named_owner_rows_but_never_live_ones,
    lease_renew_and_release_move_only_the_lease_their_token_holds,
    lease_a_re_claim_gets_a_lease_the_first_claim_cannot_touch,
    lease_a_reclaim_counts_the_interruption_against_the_journal_position,
    open_tasks_is_the_unfinished_work_of_one_job,
    a_task_row_keeps_its_intent_and_its_judge,
);

fn spec(id: &TaskId) -> TaskSpec {
    TaskSpec::new(id.clone(), JobId::new("j"), AgentKey::new("k"), json!(id))
}

fn boom() -> JournaledReason {
    JournaledReason::from(&Reason::Execution("boom".into()))
}

/// Enqueues `id` and claims it for runner `r`, leasing it until `now + ttl`.
async fn claimed(b: &Arc<dyn DurableStore>, id: &TaskId, now: i64, ttl: i64) -> LeaseToken {
    b.enqueue(&spec(id), now).await.unwrap();
    let c = b
        .claim_ready("r", now, ttl)
        .await
        .unwrap()
        .expect("the task just enqueued");
    assert_eq!(
        c.spec.id,
        id.clone(),
        "premise: it was the only claimable task"
    );
    c.token
}

/// Ends a `Ready` task nobody holds once its case is done with it, so the next case's
/// claim finds only its own task.
async fn retire(b: &Arc<dyn DurableStore>, id: &TaskId) {
    b.commit(id, Writer::Unclaimed, json!(null), 0)
        .await
        .unwrap();
}

async fn row(b: &Arc<dyn DurableStore>, id: &TaskId) -> Task {
    b.get(id)
        .await
        .unwrap()
        .expect("the task is on the durable store")
}

async fn kinds(b: &Arc<dyn DurableStore>, id: &TaskId) -> Vec<EventKind> {
    b.trajectory(id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

async fn woken(b: &Arc<dyn DurableStore>, id: &TaskId) -> usize {
    b.trajectory(id)
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Woken { .. }))
        .count()
}

/// Returns the cursor after the newest event, or the start on an empty durable store.
async fn newest(b: &Arc<dyn DurableStore>) -> Cursor {
    let seq = b
        .since(Cursor::START)
        .await
        .unwrap()
        .last()
        .map_or(0, |e| e.seq);
    Cursor::after(seq).unwrap()
}

fn delta(n: i64) -> Value {
    json!({ "z": n, "a": [n, "x"] })
}

/// `id` claimed, journalled three times, committed and its lease run out at 10: terminal,
/// owing nothing, sealable at 1000.
async fn finished(b: &Arc<dyn DurableStore>, id: &TaskId) -> Vec<Event> {
    let claim = Writer::Claim(claimed(b, id, 0, 10).await);
    for n in 0..3 {
        b.append_delta(claim, id, delta(n), 0, 100 + n)
            .await
            .unwrap();
    }
    b.commit(id, claim, json!({ "summary": "done" }), 0)
        .await
        .unwrap();
    b.trajectory(id).await.unwrap()
}

// ── 1. append-only; sealing only moves ──────────────────────────────────────

async fn law_append_only<S: DurableStore + SealJournals + 'static>(store: Arc<S>) {
    let b: Arc<dyn DurableStore> = store.clone();
    // A task's trajectory is its own events, in seq order, payloads as written.
    b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
    b.enqueue(&spec(&TaskId::new("b")), 0).await.unwrap();
    b.append_delta(
        Writer::Unclaimed,
        &TaskId::new("a"),
        json!({"turn": 1}),
        0,
        1,
    )
    .await
    .unwrap();
    b.append_delta(
        Writer::Unclaimed,
        &TaskId::new("b"),
        json!({"x": true}),
        0,
        2,
    )
    .await
    .unwrap();
    b.append_delta(
        Writer::Unclaimed,
        &TaskId::new("a"),
        json!({"turn": 2}),
        0,
        3,
    )
    .await
    .unwrap();
    let a = b.trajectory(&TaskId::new("a")).await.unwrap();
    assert_eq!(a.len(), 3, "only task a's own events");
    assert!(
        a.iter().all(|e| e.task_id == TaskId::new("a")),
        "scoped to task a"
    );
    assert!(a.windows(2).all(|w| w[0].seq < w[1].seq), "in seq order");
    assert_eq!(a[0].kind, EventKind::Enqueued);
    assert_eq!(
        a[1].kind,
        EventKind::Delta {
            payload: json!({"turn": 1})
        }
    );
    assert_eq!(
        a[2].kind,
        EventKind::Delta {
            payload: json!({"turn": 2})
        }
    );
    retire(&b, &TaskId::new("a")).await;
    retire(&b, &TaskId::new("b")).await;

    // `since` returns what came after its cursor.
    let start = newest(&b).await;
    b.enqueue(&spec(&TaskId::new("s")), 1).await.unwrap();
    b.claim_ready("r", 2, 1000).await.unwrap().unwrap();
    b.commit(
        &TaskId::new("s"),
        holder(&*b, &TaskId::new("s")).await,
        json!(null),
        3,
    )
    .await
    .unwrap();
    let all = b.since(start).await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].kind, EventKind::Enqueued);
    let tail = b.since(Cursor::after(all[0].seq).unwrap()).await.unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].kind, EventKind::Running);

    // An earlier read is a prefix of every later one.
    let earlier = b.since(Cursor::START).await.unwrap();
    b.append_delta(
        Writer::Unclaimed,
        &TaskId::new("a"),
        json!({"turn": 3}),
        0,
        4,
    )
    .await
    .unwrap();
    let later = b.since(Cursor::START).await.unwrap();
    assert_eq!(later[..earlier.len()], earlier[..], "nothing read changed");
    assert_eq!(later.len(), earlier.len() + 1);

    // Sealing moves a journal into a file; every read gives the same events.
    let before = finished(&b, &TaskId::new("t")).await;
    let stream = b.since(Cursor::START).await.unwrap();
    store
        .seal_tasks(&[TaskId::try_from("t".to_string()).unwrap()], 1_000)
        .await
        .unwrap();
    assert_eq!(
        b.trajectory(&TaskId::new("t")).await.unwrap(),
        before,
        "the journal reads back identical"
    );
    assert_eq!(
        b.since(Cursor::START).await.unwrap(),
        stream,
        "the stream reads the same, sealed or not"
    );

    // The furthest cursor there is reads nothing, on every backend.
    let furthest = Cursor::after(i64::MAX.unsigned_abs()).unwrap();
    assert!(b.since(furthest).await.unwrap().is_empty());
}

// ── 2. the durable store writes each lifecycle event with its row change ────────────

/// Returns the status a lifecycle event leaves its task in; a `Delta` is not one.
fn status_after(kind: &EventKind) -> Option<Status> {
    match kind {
        EventKind::Enqueued | EventKind::Woken { .. } => Some(Status::Ready),
        EventKind::Running => Some(Status::Running),
        EventKind::Done => Some(Status::Done),
        EventKind::Failed { .. } => Some(Status::Failed),
        EventKind::Parked { .. } => Some(Status::Parked),
        EventKind::Delta { .. } => None,
    }
}

async fn law_lifecycle_events(b: Arc<dyn DurableStore>) {
    // Enqueue, claim and commit.
    claimed(&b, &TaskId::new("done"), 0, 1000).await;
    b.commit(
        &TaskId::new("done"),
        holder(&*b, &TaskId::new("done")).await,
        json!({ "summary": "A" }),
        0,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("done")).await.status, Status::Done);
    assert_eq!(
        row(&b, &TaskId::new("done")).await.result.unwrap()["summary"],
        "A"
    );
    assert_eq!(
        kinds(&b, &TaskId::new("done")).await,
        vec![EventKind::Enqueued, EventKind::Running, EventKind::Done]
    );

    // A failure retried below the limit, then the one that ends the task.
    claimed(&b, &TaskId::new("failed"), 0, 1000).await;
    let first = b
        .fail(
            &TaskId::new("failed"),
            holder(&*b, &TaskId::new("failed")).await,
            2,
            boom(),
            0,
        )
        .await;
    assert_eq!(first.unwrap(), FailedAttempt::Requeued, "attempt 1 < 2");
    assert_eq!(row(&b, &TaskId::new("failed")).await.status, Status::Ready);
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    let second = b
        .fail(
            &TaskId::new("failed"),
            holder(&*b, &TaskId::new("failed")).await,
            2,
            boom(),
            0,
        )
        .await;
    assert_eq!(second.unwrap(), FailedAttempt::Terminal, "attempt 2 == 2");
    assert_eq!(row(&b, &TaskId::new("failed")).await.status, Status::Failed);
    assert_eq!(
        kinds(&b, &TaskId::new("failed")).await,
        vec![
            EventKind::Enqueued,
            EventKind::Running,
            EventKind::Running,
            EventKind::Failed { reason: boom() },
        ],
        "the retried attempt leaves no event; the one that ends the task leaves its reason"
    );

    // A permanent failure spends no attempt.
    let claim = Writer::Claim(claimed(&b, &TaskId::new("terminal"), 0, 1000).await);
    b.fail_terminal(&TaskId::new("terminal"), claim, boom(), 0)
        .await
        .unwrap();
    assert_eq!(row(&b, &TaskId::new("terminal")).await.attempt, 0);
    assert_eq!(
        kinds(&b, &TaskId::new("terminal")).await,
        vec![
            EventKind::Enqueued,
            EventKind::Running,
            EventKind::Failed { reason: boom() },
        ]
    );

    // A park, and the reply that makes the task `Ready` with the reply in one write.
    claimed(&b, &TaskId::new("parked"), 0, 1000).await;
    b.park(
        &TaskId::new("parked"),
        holder(&*b, &TaskId::new("parked")).await,
        &asked("approve?", Millis::MAX),
        0,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("parked")).await.status, Status::Parked);
    assert_eq!(
        row(&b, &TaskId::new("parked")).await.wait,
        Some(asked("approve?", Millis::MAX))
    );
    b.resume(&TaskId::new("parked"), &answer(json!("yes")), 0)
        .await
        .unwrap();
    let t = row(&b, &TaskId::new("parked")).await;
    assert_eq!(t.status, Status::Ready, "reply-set AND ready, one step");
    assert_eq!(t.reply, Some(answer(json!("yes"))));
    assert!(t.wait.is_none());
    assert_eq!(
        kinds(&b, &TaskId::new("parked")).await,
        vec![
            EventKind::Enqueued,
            EventKind::Running,
            EventKind::Parked {
                wait: asked("approve?", Millis::MAX)
            },
            EventKind::Woken {
                reply: Some(answer(json!("yes")))
            },
        ]
    );
    retire(&b, &TaskId::new("parked")).await;

    // A reclaim runs the same run again and writes nothing.
    claimed(&b, &TaskId::new("reclaimed"), 0, 10).await;
    assert_eq!(b.reclaim_expired(100).await.unwrap(), 1, "premise");
    assert_eq!(
        row(&b, &TaskId::new("reclaimed")).await.status,
        Status::Ready
    );
    assert_eq!(
        kinds(&b, &TaskId::new("reclaimed")).await,
        vec![EventKind::Enqueued, EventKind::Running]
    );
    retire(&b, &TaskId::new("reclaimed")).await;

    // An ending whose continuation's wake fails lands nothing, so the result is never
    // half-committed.
    b.enqueue(
        &spec(&TaskId::new("uncommitted")).waking(TaskId::new("ghost")),
        0,
    )
    .await
    .unwrap();
    let claim = Writer::Claim(b.claim_ready("r", 0, 1000).await.unwrap().unwrap().token);
    let r = b
        .commit(
            &TaskId::new("uncommitted"),
            claim,
            json!({ "summary": "C" }),
            0,
        )
        .await;
    assert!(matches!(r, Err(DurableStoreError::NoSuchTask(_))));
    let t = row(&b, &TaskId::new("uncommitted")).await;
    assert_eq!(t.status, Status::Running, "commit rolled back — not Done");
    assert!(t.result.is_none(), "no result written");
    assert_eq!(
        kinds(&b, &TaskId::new("uncommitted")).await,
        vec![EventKind::Enqueued, EventKind::Running],
        "no Done event appended"
    );

    // The same for a failure that spends the last attempt: the attempt rolls back too, so
    // a store-write fault never spends the retry budget.
    b.enqueue(
        &spec(&TaskId::new("unfailed")).waking(TaskId::new("ghost")),
        0,
    )
    .await
    .unwrap();
    let claim = Writer::Claim(b.claim_ready("r", 0, 1000).await.unwrap().unwrap().token);
    let r = b.fail(&TaskId::new("unfailed"), claim, 1, boom(), 0).await;
    assert!(matches!(r, Err(DurableStoreError::NoSuchTask(_))));
    let t = row(&b, &TaskId::new("unfailed")).await;
    assert_eq!(
        t.status,
        Status::Running,
        "restored to the pre-fail claimed state"
    );
    assert_eq!(
        t.attempt, 0,
        "the attempt increment rolled back too — budget not spent"
    );
    assert_eq!(
        kinds(&b, &TaskId::new("unfailed")).await,
        vec![EventKind::Enqueued, EventKind::Running],
        "no Failed event leaked"
    );
    b.cancel_descendants(&TaskId::new("ghost"), "done with it", 0)
        .await
        .unwrap();

    // A commit, its `Done` and its continuation's wake land together.
    b.enqueue(&spec(&TaskId::new("child")).waking(TaskId::new("orch")), 0)
        .await
        .unwrap();
    b.enqueue(&spec(&TaskId::new("orch")), 0).await.unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.park(
        &TaskId::new("orch"),
        holder(&*b, &TaskId::new("orch")).await,
        &Wait::Children,
        0,
    )
    .await
    .unwrap();
    let claim = holder(&*b, &TaskId::new("child")).await;
    b.commit(&TaskId::new("child"), claim, json!({ "summary": "C" }), 0)
        .await
        .unwrap();
    assert_eq!(row(&b, &TaskId::new("child")).await.status, Status::Done);
    assert_eq!(
        row(&b, &TaskId::new("child")).await.result.unwrap()["summary"],
        "C"
    );
    assert_eq!(
        row(&b, &TaskId::new("orch")).await.status,
        Status::Ready,
        "continuation woken"
    );
    assert!(
        b.since(Cursor::START)
            .await
            .unwrap()
            .iter()
            .any(|e| e.task_id == TaskId::new("child") && e.kind == EventKind::Done)
    );

    // A permanent failure, its `Failed` and its continuation's wake land together.
    b.enqueue(&spec(&TaskId::new("child2")).waking(TaskId::new("orch")), 0)
        .await
        .unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.park(
        &TaskId::new("orch"),
        holder(&*b, &TaskId::new("orch")).await,
        &Wait::Children,
        0,
    )
    .await
    .unwrap();
    let claim = holder(&*b, &TaskId::new("child2")).await;
    b.fail_terminal(&TaskId::new("child2"), claim, boom(), 0)
        .await
        .unwrap();
    assert_eq!(row(&b, &TaskId::new("child2")).await.status, Status::Failed);
    assert_eq!(
        row(&b, &TaskId::new("child2")).await.attempt,
        0,
        "fail_terminal did not spend the budget"
    );
    assert_eq!(
        row(&b, &TaskId::new("orch")).await.status,
        Status::Ready,
        "continuation woken"
    );
    assert!(
        b.since(Cursor::START)
            .await
            .unwrap()
            .iter()
            .any(|e| e.task_id == TaskId::new("child2")
                && e.kind == EventKind::Failed { reason: boom() })
    );

    // So every task's last lifecycle event names the status its row is in. None of them
    // was last made `Ready` by a retry or a reclaim, which leave the `Running` of the run
    // they repeat last.
    for t in b.tasks().await.unwrap() {
        let last = kinds(&b, &t.spec.id)
            .await
            .iter()
            .rev()
            .find_map(status_after);
        assert_eq!(last, Some(t.status), "task `{}`", t.spec.id);
    }
}

// ── 3. idempotent; an ended task stays ended ────────────────────────────────

const ENDINGS: [Status; 2] = [Status::Done, Status::Failed];

/// `id` claimed, then ended as `ending` — the row a late write from its runner meets.
async fn ended(b: &Arc<dyn DurableStore>, id: &TaskId, ending: Status) -> Task {
    let claim = Writer::Claim(claimed(b, id, 0, 10_000).await);
    match ending {
        Status::Done => b
            .commit(id, claim, json!({ "summary": "finished" }), 0)
            .await
            .unwrap(),
        Status::Failed => {
            let failed = b.fail(id, claim, 1, boom(), 0).await;
            assert_eq!(
                failed.unwrap(),
                FailedAttempt::Terminal,
                "premise: no retry"
            );
        }
        other => panic!("not an ending: {other:?}"),
    }
    row(b, id).await
}

/// Refused as superseded, naming this task and the state it had ended in.
fn assert_refused(r: Result<(), DurableStoreError>, id: &TaskId, ending: Status) {
    match r {
        Err(DurableStoreError::Superseded { id: got, status }) => {
            assert_eq!(got, id.clone());
            assert_eq!(status, ending);
        }
        other => panic!("expected Superseded({id}, {ending:?}), got {other:?}"),
    }
}

async fn assert_unchanged(b: &Arc<dyn DurableStore>, before: &Task) {
    let after = row(b, &before.spec.id).await;
    assert_eq!(after.status, before.status, "status");
    assert_eq!(after.wake_pending, before.wake_pending, "wake_pending");
    assert_eq!(after.wait, before.wait, "wait");
    assert_eq!(after.result, before.result, "result");
}

async fn law_idempotent<S: DurableStore + SealJournals + 'static>(store: Arc<S>) {
    let b: Arc<dyn DurableStore> = store.clone();
    for ending in ENDINGS {
        // A park.
        let id = TaskId::try_from(format!("park-{}", ending.as_str())).unwrap();
        let before = ended(&b, &id, ending).await;
        assert_refused(
            b.park(&id, holder(&*b, &id).await, &asked("?", 9), 0).await,
            &id,
            ending,
        );
        assert_unchanged(&b, &before).await;

        // A permanent failure.
        let id = TaskId::try_from(format!("ft-{}", ending.as_str())).unwrap();
        let before = ended(&b, &id, ending).await;
        let claim = holder(&*b, &id).await;
        assert_refused(b.fail_terminal(&id, claim, boom(), 0).await, &id, ending);
        assert_unchanged(&b, &before).await;

        // A wake, and no `Woken` from it.
        let id = TaskId::try_from(format!("wake-{}", ending.as_str())).unwrap();
        let before = ended(&b, &id, ending).await;
        let woken_before = woken(&b, &id).await;
        assert_refused(b.wake(&id, 5).await, &id, ending);
        assert_unchanged(&b, &before).await;

        assert_eq!(
            woken(
                &b,
                &TaskId::try_from(format!("wake-{}", ending.as_str())).unwrap()
            )
            .await,
            woken_before,
            "no `Woken` event"
        );
    }

    // A late commit or failure on a task that ended: the work is discarded, and the
    // failure does not re-arm it `Ready`.
    claimed(&b, &TaskId::new("t"), 0, 1000).await;
    b.fail(
        &TaskId::new("t"),
        holder(&*b, &TaskId::new("t")).await,
        1,
        boom(),
        0,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("t")).await.status, Status::Failed);
    let e = b
        .commit(
            &TaskId::new("t"),
            holder(&*b, &TaskId::new("t")).await,
            json!({ "summary": "done!" }),
            0,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, DurableStoreError::Superseded { ref id, .. } if id.as_str() == "t"),
        "got {e:?}"
    );
    assert!(
        row(&b, &TaskId::new("t")).await.result.is_none(),
        "work discarded"
    );
    let e = b
        .fail(
            &TaskId::new("t"),
            holder(&*b, &TaskId::new("t")).await,
            5,
            boom(),
            0,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, DurableStoreError::Superseded { ref id, .. } if id.as_str() == "t"),
        "got {e:?}"
    );
    assert_eq!(row(&b, &TaskId::new("t")).await.status, Status::Failed);

    // A wake raced the run, the task was then cancelled, and its code parks on the way
    // out: the park is refused, so `wake_pending` cannot re-run a cancelled task.
    b.enqueue(&spec(&TaskId::new("root")), 0).await.unwrap();
    b.enqueue(&spec(&TaskId::new("kid")).waking(TaskId::new("root")), 0)
        .await
        .unwrap();
    while let Some(c) = b.claim_ready("r", 0, 10_000).await.unwrap() {
        if c.spec.id == TaskId::new("kid") {
            break;
        }
    }
    b.wake(&TaskId::new("kid"), 1).await.unwrap();
    assert!(row(&b, &TaskId::new("kid")).await.wake_pending, "premise");
    let out = b
        .cancel_descendants(&TaskId::new("root"), "parent failed", 2)
        .await
        .unwrap();
    assert_eq!(out.running, vec![TaskId::new("kid")], "premise");
    let before = row(&b, &TaskId::new("kid")).await;
    assert_refused(
        b.park(
            &TaskId::new("kid"),
            holder(&*b, &TaskId::new("kid")).await,
            &Wait::Children,
            0,
        )
        .await,
        &TaskId::new("kid"),
        Status::Failed,
    );
    assert_unchanged(&b, &before).await;
    assert!(
        b.claim_ready("r2", 3, 10_000)
            .await
            .unwrap()
            .is_none_or(|c| c.spec.id != TaskId::new("kid")),
        "nothing may claim the cancelled task again"
    );

    // The one write left legal on an ended task: its releases land there.
    let before = ended(&b, &TaskId::new("h"), Status::Failed).await;
    for holds_delta in [1, 1, -1] {
        b.append_delta(
            Writer::Unclaimed,
            &TaskId::new("h"),
            delta(holds_delta),
            holds_delta,
            0,
        )
        .await
        .unwrap();
    }
    let after = row(&b, &TaskId::new("h")).await;
    assert_eq!(after.holds, before.holds + 1);
    assert_eq!(after.status, Status::Failed, "and it never moves status");

    // The first effect recorded under a key is the one kept.
    b.record_effect("charge", &json!({ "id": 1 }))
        .await
        .unwrap();
    b.record_effect("charge", &json!({ "id": 2 }))
        .await
        .unwrap();
    assert_eq!(
        b.effect_result("charge").await.unwrap(),
        Some(json!({ "id": 1 }))
    );
    assert_eq!(b.effect_result("never").await.unwrap(), None);

    // A resubmitted root, finished and sealed by then, changes nothing, appends nothing,
    // and does not hold up the new tasks beside it.
    let before = ended(&b, &TaskId::new("s"), Status::Done).await;
    let Writer::Claim(token) = holder(&*b, &TaskId::new("s")).await else {
        panic!("premise: `s` is held by its claim");
    };
    b.release_lease(&TaskId::new("s"), token).await.unwrap();
    store
        .seal_tasks(&[TaskId::try_from("s".to_string()).unwrap()], 1_000)
        .await
        .unwrap();
    let journal = b.trajectory(&TaskId::new("s")).await.unwrap();
    let specs = [
        TaskSpec::new(
            TaskId::new("s"),
            JobId::new("j"),
            AgentKey::new("k"),
            json!("again"),
        ),
        TaskSpec::new(
            TaskId::new("new"),
            JobId::new("j"),
            AgentKey::new("k"),
            json!("new"),
        ),
    ];
    b.enqueue_all(&specs, 2_000).await.unwrap();
    b.enqueue(&specs[0], 2_000).await.unwrap();
    let after = row(&b, &TaskId::new("s")).await;
    assert_eq!(after.status, before.status);
    assert_eq!(after.spec.intent, json!("s"), "the row is as it was");
    assert_eq!(b.trajectory(&TaskId::new("s")).await.unwrap(), journal);
    assert_eq!(
        kinds(&b, &TaskId::new("new")).await,
        vec![EventKind::Enqueued]
    );

    // Enqueuing an id already on the durable store changes nothing.
    b.enqueue(&spec(&TaskId::new("e")), 0).await.unwrap();
    b.enqueue(&spec(&TaskId::new("e")), 0).await.unwrap();
    let copies = b
        .tasks()
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.spec.id == TaskId::new("e"))
        .count();
    assert_eq!(copies, 1);
    assert_eq!(
        kinds(&b, &TaskId::new("e")).await,
        vec![EventKind::Enqueued]
    );
}

// ── 4. admission ────────────────────────────────────────────────────────────

/// The four endings, each written by `claim` on task `id`.
async fn endings(
    b: &Arc<dyn DurableStore>,
    id: &TaskId,
    claim: Writer,
) -> Vec<(&'static str, Result<(), DurableStoreError>)> {
    vec![
        (
            "commit",
            b.commit(id, claim, json!({ "summary": "stale" }), 0).await,
        ),
        ("fail", b.fail(id, claim, 1, boom(), 0).await.map(|_| ())),
        ("fail_terminal", b.fail_terminal(id, claim, boom(), 0).await),
        (
            "park",
            b.park(id, claim, &asked("stale?", Millis::MAX), 0).await,
        ),
    ]
}

fn assert_lease_lost(what: &str, r: &Result<(), DurableStoreError>, id: &TaskId, now: Status) {
    match r {
        Err(DurableStoreError::LeaseLost { id: got, status }) => {
            assert_eq!(got, id, "{what}");
            assert_eq!(*status, now, "{what}");
        }
        other => panic!("{what}: expected LeaseLost({id}, {now:?}), got {other:?}"),
    }
}

async fn row_text(b: &Arc<dyn DurableStore>, id: &TaskId) -> String {
    format!("{:?}", row(b, id).await)
}

/// Returns the payloads of task `id`'s `Delta` events, in order.
async fn deltas(b: &Arc<dyn DurableStore>, id: &TaskId) -> Vec<Value> {
    b.trajectory(id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Delta { payload } => Some(payload),
            _ => None,
        })
        .collect()
}

async fn law_admission(b: Arc<dyn DurableStore>) {
    // A write to an id with no row.
    let r = b
        .park(&TaskId::new("ghost"), Writer::Unclaimed, &Wait::Children, 0)
        .await;
    assert!(
        matches!(r, Err(DurableStoreError::NoSuchTask(ref id)) if id.as_str() == "ghost"),
        "{r:?}"
    );
    let r = b.wake(&TaskId::new("ghost"), 0).await;
    assert!(
        matches!(r, Err(DurableStoreError::NoSuchTask(ref id)) if id.as_str() == "ghost"),
        "{r:?}"
    );
    let r = b
        .fail_terminal(&TaskId::new("ghost"), Writer::Unclaimed, boom(), 0)
        .await;
    assert!(
        matches!(r, Err(DurableStoreError::NoSuchTask(ref id)) if id.as_str() == "ghost"),
        "{r:?}"
    );

    // A claim whose lease was reclaimed: its endings are refused and change nothing.
    let old = Writer::Claim(claimed(&b, &TaskId::new("t"), 0, 10).await);
    assert_eq!(b.reclaim_expired(100).await.unwrap(), 1, "premise");
    let before = row_text(&b, &TaskId::new("t")).await;
    let events = b.trajectory(&TaskId::new("t")).await.unwrap().len();
    for (what, r) in endings(&b, &TaskId::new("t"), old).await {
        assert_lease_lost(what, &r, &TaskId::new("t"), Status::Ready);
    }
    assert_eq!(
        row_text(&b, &TaskId::new("t")).await,
        before,
        "row untouched"
    );
    assert_eq!(
        b.trajectory(&TaskId::new("t")).await.unwrap().len(),
        events,
        "no event"
    );

    // Nor may it append, before anyone claims the task again.
    let stale = b
        .append_delta(old, &TaskId::new("t"), json!({ "by": "stale" }), 0, 102)
        .await;
    assert_lease_lost("append", &stale, &TaskId::new("t"), Status::Ready);
    assert!(deltas(&b, &TaskId::new("t")).await.is_empty());

    // It cannot end the run that took the task over.
    let new = b
        .claim_ready("r2", 101, 10_000)
        .await
        .unwrap()
        .expect("the reclaimed task")
        .token;
    for (what, r) in endings(&b, &TaskId::new("t"), old).await {
        assert_lease_lost(what, &r, &TaskId::new("t"), Status::Running);
    }
    b.commit(
        &TaskId::new("t"),
        Writer::Claim(new),
        json!({ "summary": "mine" }),
        0,
    )
    .await
    .unwrap();
    let t = row(&b, &TaskId::new("t")).await;
    assert_eq!(t.status, Status::Done);
    assert_eq!(t.result.unwrap()["summary"], "mine");

    // Once that run has finished, the stale claim is told it lost the lease, not that the
    // task ended, so it does not wind up a task that is not its own.
    b.release_lease(&TaskId::new("t"), new).await.unwrap();
    let r = b
        .commit(&TaskId::new("t"), old, json!({ "summary": "stale" }), 0)
        .await;
    assert_lease_lost("commit", &r, &TaskId::new("t"), Status::Done);

    // An expired lease nobody reclaimed is still its claim's.
    let claim = claimed(&b, &TaskId::new("expired"), 0, 10).await;
    b.park(
        &TaskId::new("expired"),
        Writer::Claim(claim),
        &asked("q?", Millis::MAX),
        500,
    )
    .await
    .unwrap();
    assert_eq!(
        row(&b, &TaskId::new("expired")).await.status,
        Status::Parked
    );

    // A writer with no claim ends only a task nobody holds.
    claimed(&b, &TaskId::new("held"), 0, 10_000).await;
    for (what, r) in endings(&b, &TaskId::new("held"), Writer::Unclaimed).await {
        assert_lease_lost(what, &r, &TaskId::new("held"), Status::Running);
    }
    b.enqueue(&spec(&TaskId::new("free")), 0).await.unwrap();
    b.commit(
        &TaskId::new("free"),
        Writer::Unclaimed,
        json!({ "summary": "direct" }),
        0,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("free")).await.status, Status::Done);

    // A reply lands only on a parked task, and the refusal names the task's state.
    b.enqueue(&spec(&TaskId::new("asked")), 0).await.unwrap();
    let r = b
        .resume(&TaskId::new("asked"), &answer(json!("yes")), 0)
        .await;
    assert!(
        matches!(r, Err(DurableStoreError::NotParked { ref id, status: UnparkedStatus::Ready }) if id.as_str() == "asked"),
        "{r:?}"
    );
    b.claim_ready("r", 0, 10_000)
        .await
        .unwrap()
        .expect("claimable");
    let r = b
        .resume(&TaskId::new("asked"), &answer(json!("yes")), 0)
        .await;
    assert!(
        matches!(
            r,
            Err(DurableStoreError::NotParked {
                status: UnparkedStatus::Running,
                ..
            })
        ),
        "{r:?}"
    );
    b.commit(
        &TaskId::new("asked"),
        holder(&*b, &TaskId::new("asked")).await,
        json!({ "summary": "done" }),
        0,
    )
    .await
    .unwrap();
    let r = b
        .resume(&TaskId::new("asked"), &answer(json!("yes")), 0)
        .await;
    assert!(
        matches!(
            r,
            Err(DurableStoreError::Superseded {
                status: Status::Done,
                ..
            })
        ),
        "{r:?}"
    );
    let r = b
        .resume(&TaskId::new("ghost"), &answer(json!("yes")), 0)
        .await;
    assert!(
        matches!(r, Err(DurableStoreError::NoSuchTask(ref id)) if id.as_str() == "ghost"),
        "{r:?}"
    );
    assert!(row(&b, &TaskId::new("asked")).await.reply.is_none());

    // A reply that does not answer what the task asks, or comes from someone not asked, is
    // refused and leaves the park as it was.
    let c = claimed(&b, &TaskId::new("picky"), 0, 1000).await;
    b.park(
        &TaskId::new("picky"),
        Writer::Claim(c),
        &asked("which branch?", Millis::MAX),
        0,
    )
    .await
    .unwrap();
    let before = row(&b, &TaskId::new("picky")).await;
    let approval = AgentReply::Approve { by: ana() };
    let stranger = AgentReply::Answer {
        by: Approver::Person { id: "bo".into() },
        value: json!("main"),
    };
    for reply in [approval, stranger] {
        let r = b.resume(&TaskId::new("picky"), &reply, 0).await;
        assert!(
            matches!(&r, Err(DurableStoreError::WrongReply { id, reply: got, .. }) if id.as_str() == "picky" && **got == reply),
            "{r:?}"
        );
        assert_unchanged(&b, &before).await;
    }
    retire(&b, &TaskId::new("picky")).await;
}

// ── 5. one `Woken` per new run ──────────────────────────────────────────────

/// Returns `id`'s `Parked` and `Woken` events, in order, by name.
async fn parks_and_wakes(b: &Arc<dyn DurableStore>, id: &TaskId) -> Vec<&'static str> {
    b.trajectory(id)
        .await
        .unwrap()
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Parked { .. } => Some("parked"),
            EventKind::Woken { .. } => Some("woken"),
            _ => None,
        })
        .collect()
}

async fn law_one_woken_per_new_run(b: Arc<dyn DurableStore>) {
    // A park with no wake pending ends the trajectory parked.
    claimed(&b, &TaskId::new("quiet"), 0, 10_000).await;
    b.park(
        &TaskId::new("quiet"),
        holder(&*b, &TaskId::new("quiet")).await,
        &asked("approve?", Millis::MAX),
        1,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("quiet")).await.status, Status::Parked);
    assert_eq!(parks_and_wakes(&b, &TaskId::new("quiet")).await, ["parked"]);

    // A wake during a run that was then reclaimed belongs to that run: the retry already
    // sees what it reported, so the retry's park does not bounce on it.
    claimed(&b, &TaskId::new("retried"), 0, 10).await;
    b.wake(&TaskId::new("retried"), 1).await.unwrap();
    b.reclaim_expired(100).await.unwrap();
    let retry = b
        .claim_ready("r", 101, 10_000)
        .await
        .unwrap()
        .expect("the reclaimed task")
        .token;
    assert!(
        !row(&b, &TaskId::new("retried")).await.wake_pending,
        "the retry already sees what the wake reported"
    );
    b.park(
        &TaskId::new("retried"),
        Writer::Claim(retry),
        &asked("q?", Millis::MAX),
        102,
    )
    .await
    .unwrap();
    assert_eq!(
        row(&b, &TaskId::new("retried")).await.status,
        Status::Parked
    );
    assert_eq!(
        woken(&b, &TaskId::new("retried")).await,
        0,
        "no wake for the retry to bounce on"
    );

    // A wake on a running task is deferred to its park, with no event yet; the park then
    // bounces to `Ready` and the trajectory ends woken, as the row is.
    claimed(&b, &TaskId::new("bounced"), 0, 10_000).await;
    b.wake(&TaskId::new("bounced"), 1).await.unwrap();
    let t = row(&b, &TaskId::new("bounced")).await;
    assert_eq!(t.status, Status::Running);
    assert!(t.wake_pending);
    assert_eq!(woken(&b, &TaskId::new("bounced")).await, 0);
    b.park(
        &TaskId::new("bounced"),
        holder(&*b, &TaskId::new("bounced")).await,
        &asked("approve?", Millis::MAX),
        2,
    )
    .await
    .unwrap();
    let t = row(&b, &TaskId::new("bounced")).await;
    assert_eq!(t.status, Status::Ready);
    assert!(!t.wake_pending);
    assert_eq!(
        parks_and_wakes(&b, &TaskId::new("bounced")).await,
        ["parked", "woken"]
    );
    retire(&b, &TaskId::new("bounced")).await;

    // A wake on a ready task changes nothing and writes no event.
    b.enqueue(&spec(&TaskId::new("ready")), 0).await.unwrap();
    b.wake(&TaskId::new("ready"), 1).await.unwrap();
    let t = row(&b, &TaskId::new("ready")).await;
    assert_eq!(t.status, Status::Ready);
    assert!(!t.wake_pending);
    assert_eq!(woken(&b, &TaskId::new("ready")).await, 0);
    retire(&b, &TaskId::new("ready")).await;

    // Parks due in the same sweep each get one `Woken`, in id order.
    for id in ["b", "c", "a"] {
        let c = claimed(&b, &id.parse::<TaskId>().unwrap(), 0, 10_000).await;
        b.park(
            &id.parse::<TaskId>().unwrap(),
            Writer::Claim(c),
            &asked("wake?", 5),
            1,
        )
        .await
        .unwrap();
    }
    let cursor = newest(&b).await;
    assert_eq!(b.wake_due(5).await.unwrap(), 3);
    let order: Vec<String> = b
        .since(cursor)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.task_id.into_inner())
        .collect();
    assert_eq!(order, ["a", "b", "c"]);
}

// ── 6. an ending wakes its continuation ─────────────────────────────────────

/// Enqueues `parent` and its `child`, claims both, and parks `parent` waiting for it.
async fn parked_parent_and_running_child(b: &Arc<dyn DurableStore>, parent: &str, child: &str) {
    b.enqueue(&spec(&parent.parse::<TaskId>().unwrap()), 0)
        .await
        .unwrap();
    b.enqueue(
        &spec(&child.parse::<TaskId>().unwrap()).waking(parent.parse::<TaskId>().unwrap()),
        0,
    )
    .await
    .unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.park(
        &parent.parse::<TaskId>().unwrap(),
        holder(&**b, &parent.parse::<TaskId>().unwrap()).await,
        &Wait::Children,
        0,
    )
    .await
    .unwrap();
}

async fn law_an_ending_wakes_its_continuation(b: Arc<dyn DurableStore>) {
    // A commit wakes its parked continuation, with the one `Woken` of the new run.
    parked_parent_and_running_child(&b, "p1", "c1").await;
    b.commit(
        &TaskId::new("c1"),
        holder(&*b, &TaskId::new("c1")).await,
        json!({ "summary": "C" }),
        1,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("p1")).await.status, Status::Ready);
    assert_eq!(woken(&b, &TaskId::new("p1")).await, 1);
    retire(&b, &TaskId::new("p1")).await;

    // A failure that ends the task wakes it; a retry is the same run and wakes nobody.
    parked_parent_and_running_child(&b, "p2", "c2").await;
    let retried = b
        .fail(
            &TaskId::new("c2"),
            holder(&*b, &TaskId::new("c2")).await,
            2,
            boom(),
            1,
        )
        .await;
    assert_eq!(retried.unwrap(), FailedAttempt::Requeued, "premise");
    assert_eq!(
        row(&b, &TaskId::new("p2")).await.status,
        Status::Parked,
        "a retry wakes nobody"
    );
    b.claim_ready("r", 1, 1000).await.unwrap().unwrap();
    let ended = b
        .fail(
            &TaskId::new("c2"),
            holder(&*b, &TaskId::new("c2")).await,
            2,
            boom(),
            2,
        )
        .await;
    assert_eq!(ended.unwrap(), FailedAttempt::Terminal, "premise");
    assert_eq!(row(&b, &TaskId::new("p2")).await.status, Status::Ready);
    assert_eq!(woken(&b, &TaskId::new("p2")).await, 1);
    retire(&b, &TaskId::new("p2")).await;

    // A permanent failure wakes it too.
    parked_parent_and_running_child(&b, "p3", "c3").await;
    b.fail_terminal(
        &TaskId::new("c3"),
        holder(&*b, &TaskId::new("c3")).await,
        boom(),
        1,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("p3")).await.status, Status::Ready);
    retire(&b, &TaskId::new("p3")).await;

    // A continuation still running keeps the wake for its park.
    b.enqueue(&spec(&TaskId::new("p4")), 0).await.unwrap();
    b.enqueue(&spec(&TaskId::new("c4")).waking(TaskId::new("p4")), 0)
        .await
        .unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.commit(
        &TaskId::new("c4"),
        holder(&*b, &TaskId::new("c4")).await,
        json!(null),
        1,
    )
    .await
    .unwrap();
    let p4 = row(&b, &TaskId::new("p4")).await;
    assert_eq!(p4.status, Status::Running);
    assert!(p4.wake_pending, "the wake waits for the park");
    b.commit(
        &TaskId::new("p4"),
        holder(&*b, &TaskId::new("p4")).await,
        json!(null),
        2,
    )
    .await
    .unwrap();

    // A continuation that has ended is left as it is, and the ending still lands.
    b.enqueue(&spec(&TaskId::new("p5")), 0).await.unwrap();
    b.enqueue(&spec(&TaskId::new("c5")).waking(TaskId::new("p5")), 0)
        .await
        .unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.claim_ready("r", 0, 1000).await.unwrap().unwrap();
    b.fail_terminal(
        &TaskId::new("p5"),
        holder(&*b, &TaskId::new("p5")).await,
        boom(),
        1,
    )
    .await
    .unwrap();
    let p5 = row(&b, &TaskId::new("p5")).await;
    b.commit(
        &TaskId::new("c5"),
        holder(&*b, &TaskId::new("c5")).await,
        json!({ "summary": "late" }),
        2,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("c5")).await.status, Status::Done);
    let after = row(&b, &TaskId::new("p5")).await;
    assert_eq!(after.status, p5.status);
    assert_eq!(after.wake_pending, p5.wake_pending);
    assert_eq!(woken(&b, &TaskId::new("p5")).await, 0);
}

// ── 7. a sealed journal refuses appends ─────────────────────────────────────

async fn law_sealed_journal_refuses_appends<S: DurableStore + SealJournals + 'static>(
    store: Arc<S>,
) {
    let b: Arc<dyn DurableStore> = store.clone();
    let before = finished(&b, &TaskId::new("t")).await;
    store
        .seal_tasks(&[TaskId::try_from("t".to_string()).unwrap()], 1_000)
        .await
        .unwrap();

    let plain = b
        .append_delta(Writer::Unclaimed, &TaskId::new("t"), delta(7), 0, 2_000)
        .await;
    assert!(
        matches!(&plain, Err(DurableStoreError::JournalSealed { task_id }) if task_id.as_str() == "t"),
        "{plain:?}"
    );

    let holding = b
        .append_delta(Writer::Unclaimed, &TaskId::new("t"), delta(8), 1, 2_001)
        .await;
    assert!(
        matches!(holding, Err(DurableStoreError::JournalSealed { .. })),
        "{holding:?}"
    );
    assert_eq!(
        row(&b, &TaskId::new("t")).await.holds,
        0,
        "the holds move rolled back with the refused append"
    );
    assert_eq!(b.trajectory(&TaskId::new("t")).await.unwrap(), before);
}

// ── 8. a sweep touches only what it names ───────────────────────────────────

async fn sweep_wake_due_wakes_only_parks_whose_deadline_came(b: Arc<dyn DurableStore>) {
    for (id, wake_at) in [
        ("past", Some(100)),
        ("future", Some(900)),
        ("forever", None),
    ] {
        let c = claimed(&b, &id.parse::<TaskId>().unwrap(), 0, 1000).await;
        let wait = match wake_at {
            Some(until) => asked("q", until),
            None => Wait::Children,
        };
        b.park(&id.parse::<TaskId>().unwrap(), Writer::Claim(c), &wait, 0)
            .await
            .unwrap();
    }

    assert_eq!(b.wake_due(50).await.unwrap(), 0, "nothing due yet");
    assert_eq!(
        b.wake_due(100).await.unwrap(),
        1,
        "a deadline reached is due"
    );
    assert_eq!(row(&b, &TaskId::new("past")).await.status, Status::Ready);
    assert_eq!(row(&b, &TaskId::new("future")).await.status, Status::Parked);
    assert_eq!(
        row(&b, &TaskId::new("forever")).await.status,
        Status::Parked
    );

    assert!(
        row(&b, &TaskId::new("past")).await.wait.is_none(),
        "the deadline is spent"
    );
    assert_eq!(b.wake_due(100).await.unwrap(), 0, "woken once, not again");

    assert_eq!(
        b.wake_due(i64::MAX).await.unwrap(),
        1,
        "only `future` remains"
    );
    assert_eq!(
        row(&b, &TaskId::new("forever")).await.status,
        Status::Parked,
        "a park with no deadline is never due"
    );
}

async fn sweep_wake_due_skips_a_park_a_reply_or_a_wake_ended(b: Arc<dyn DurableStore>) {
    let c = claimed(&b, &TaskId::new("answered"), 0, 1000).await;
    b.park(
        &TaskId::new("answered"),
        Writer::Claim(c),
        &asked("approve?", 500),
        0,
    )
    .await
    .unwrap();
    assert_eq!(
        row(&b, &TaskId::new("answered")).await.wait,
        Some(asked("approve?", 500))
    );
    b.resume(&TaskId::new("answered"), &answer(json!("yes")), 0)
        .await
        .unwrap();
    assert!(row(&b, &TaskId::new("answered")).await.wait.is_none());
    retire(&b, &TaskId::new("answered")).await;

    let c = claimed(&b, &TaskId::new("orch"), 0, 1000).await;
    b.park(
        &TaskId::new("orch"),
        Writer::Claim(c),
        &asked("wake?", 500),
        0,
    )
    .await
    .unwrap();
    b.wake(&TaskId::new("orch"), 0).await.unwrap();
    assert_eq!(row(&b, &TaskId::new("orch")).await.status, Status::Ready);
    assert!(row(&b, &TaskId::new("orch")).await.wait.is_none());

    assert_eq!(
        b.wake_due(i64::MAX).await.unwrap(),
        0,
        "both deadlines cancelled"
    );
}

async fn sweep_cancel_descendants_walks_the_subtree_and_splits_the_report(
    b: Arc<dyn DurableStore>,
) {
    claimed(&b, &TaskId::new("root"), 0, 10_000).await;
    b.enqueue(&spec(&TaskId::new("b")).waking(TaskId::new("root")), 0)
        .await
        .unwrap();
    let running = b.claim_ready("r", 0, 10_000).await.unwrap().unwrap();
    assert_eq!(
        running.spec.id,
        TaskId::new("b"),
        "premise: nothing else was claimable"
    );
    b.enqueue(&spec(&TaskId::new("done")).waking(TaskId::new("root")), 0)
        .await
        .unwrap();
    b.commit(
        &TaskId::new("done"),
        Writer::Unclaimed,
        json!({ "summary": "finished" }),
        0,
    )
    .await
    .unwrap();
    for (id, parent) in [("a", "root"), ("a1", "a"), ("gd", "done")] {
        b.enqueue(
            &spec(&id.parse::<TaskId>().unwrap()).waking(parent.parse::<TaskId>().unwrap()),
            0,
        )
        .await
        .unwrap();
    }

    let out = b
        .cancel_descendants(&TaskId::new("root"), "parent failed", 7)
        .await
        .unwrap();

    let mut stopped = out.stopped.clone();
    stopped.sort();
    assert_eq!(
        stopped,
        [TaskId::new("a"), TaskId::new("a1"), TaskId::new("gd")],
        "children, grandchildren, and the child of an ended task"
    );
    assert_eq!(
        out.running,
        [TaskId::new("b")],
        "a running child is reported apart"
    );
    assert_eq!(
        row(&b, &TaskId::new("b")).await.status,
        Status::Failed,
        "and cancelled too"
    );
    assert_eq!(
        row(&b, &TaskId::new("done")).await.status,
        Status::Done,
        "an ended task is left as it is"
    );
    for id in &stopped {
        let said = b.trajectory(id).await.unwrap().iter().any(
            |e| matches!(&e.kind, EventKind::Failed { reason } if reason.text == "parent failed"),
        );
        assert!(said, "`{id}` says why it was cancelled");
    }
}

/// A `continuation` cycle is malformed data, and a sweep that spins on it is no answer to
/// malformed data.
async fn sweep_cancel_descendants_stops_on_a_continuation_cycle(b: Arc<dyn DurableStore>) {
    b.enqueue(&spec(&TaskId::new("p")), 0).await.unwrap();
    b.enqueue(&spec(&TaskId::new("x")).waking(TaskId::new("y")), 0)
        .await
        .unwrap();
    b.enqueue(&spec(&TaskId::new("y")).waking(TaskId::new("x")), 0)
        .await
        .unwrap();
    b.enqueue(&spec(&TaskId::new("c")).waking(TaskId::new("p")), 0)
        .await
        .unwrap();

    let out = b
        .cancel_descendants(&TaskId::new("p"), "parent failed", 1)
        .await
        .unwrap();
    assert_eq!(out.stopped, [TaskId::new("c")]);

    let out = b
        .cancel_descendants(&TaskId::new("x"), "parent failed", 1)
        .await
        .unwrap();
    assert_eq!(out.stopped, [TaskId::new("y")], "walked once, did not loop");
}

async fn hold_one(b: &Arc<dyn DurableStore>, id: &str, claim: Writer) {
    b.append_delta(
        claim,
        &id.parse::<TaskId>().unwrap(),
        json!({ "acquire": 1 }),
        1,
        0,
    )
    .await
    .unwrap();
}

/// A parked task holding a worktree holds it on purpose: it is coming back for it.
async fn sweep_abandoned_holds_leaves_a_task_that_can_still_run(b: Arc<dyn DurableStore>) {
    let claim = Writer::Claim(claimed(&b, &TaskId::new("live"), 0, 1000).await);
    hold_one(&b, "live", claim).await;
    assert_eq!(row(&b, &TaskId::new("live")).await.holds, 1);
    assert!(
        b.abandoned_holds(10, 0).await.unwrap().is_empty(),
        "running and holding"
    );

    b.park(&TaskId::new("live"), claim, &asked("q", Millis::MAX), 0)
        .await
        .unwrap();
    assert!(
        b.abandoned_holds(10, 0).await.unwrap().is_empty(),
        "parked and holding"
    );

    b.fail(&TaskId::new("live"), Writer::Unclaimed, 1, boom(), 0)
        .await
        .unwrap();
    assert_eq!(
        b.abandoned_holds(10, 0).await.unwrap(),
        [TaskId::new("live")]
    );
}

async fn sweep_abandoned_holds_waits_out_the_lease_and_honours_the_limit(b: Arc<dyn DurableStore>) {
    for id in ["d1", "d2", "d3"] {
        let claim = Writer::Claim(claimed(&b, &id.parse::<TaskId>().unwrap(), 0, 1000).await);
        hold_one(&b, id, claim).await;
        b.fail(&id.parse::<TaskId>().unwrap(), claim, 1, boom(), 0)
            .await
            .unwrap();
    }
    let claim = Writer::Claim(claimed(&b, &TaskId::new("clean"), 0, 1000).await);
    b.fail(&TaskId::new("clean"), claim, 1, boom(), 0)
        .await
        .unwrap();

    assert!(
        b.abandoned_holds(10, 999).await.unwrap().is_empty(),
        "each holder's lease lives until 1000, and its run winds up what it holds"
    );
    assert_eq!(
        b.abandoned_holds(10, 1000).await.unwrap(),
        [TaskId::new("d1"), TaskId::new("d2"), TaskId::new("d3")],
        "in id order, and not the task that owes nothing"
    );
    assert_eq!(
        b.abandoned_holds(2, 1000).await.unwrap(),
        [TaskId::new("d1"), TaskId::new("d2")],
        "the limit caps one pass"
    );
}

// ── 9. holds move with the journal ──────────────────────────────────────────

async fn holds_move_only_with_the_delta_they_count(b: Arc<dyn DurableStore>) {
    let old = Writer::Claim(claimed(&b, &TaskId::new("h"), 0, 10).await);
    b.append_delta(old, &TaskId::new("h"), json!({ "acquire": 1 }), 1, 1)
        .await
        .unwrap();
    assert_eq!(
        row(&b, &TaskId::new("h")).await.holds,
        1,
        "the count moved with its delta"
    );

    assert_eq!(b.reclaim_expired(100).await.unwrap(), 1, "premise");
    let refused = b
        .append_delta(old, &TaskId::new("h"), json!({ "release": 1 }), -1, 101)
        .await;
    assert!(
        matches!(refused, Err(DurableStoreError::LeaseLost { .. })),
        "{refused:?}"
    );
    assert_eq!(
        row(&b, &TaskId::new("h")).await.holds,
        1,
        "a refused delta moves no count"
    );
    assert_eq!(
        deltas(&b, &TaskId::new("h")).await,
        vec![json!({ "acquire": 1 })]
    );
    retire(&b, &TaskId::new("h")).await;
}

async fn holds_never_go_below_zero(b: Arc<dyn DurableStore>) {
    b.enqueue(&spec(&TaskId::new("z")), 0).await.unwrap();
    b.append_delta(
        Writer::Unclaimed,
        &TaskId::new("z"),
        json!({ "release": 3 }),
        -3,
        0,
    )
    .await
    .unwrap();
    assert_eq!(row(&b, &TaskId::new("z")).await.holds, 0);
}

// ── 10. a lease belongs to one claim ────────────────────────────────────────

async fn lease_a_ready_task_is_claimed_once(b: Arc<dyn DurableStore>) {
    b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
    assert!(b.claim_ready("r1", 0, 100).await.unwrap().is_some());
    assert!(b.claim_ready("r2", 0, 100).await.unwrap().is_none());
    let t = row(&b, &TaskId::new("a")).await;
    assert_eq!(t.status, Status::Running);
    assert_eq!(t.lease.unwrap().owner, "r1");
}

async fn lease_reclaim_expired_takes_only_leases_that_ran_out(b: Arc<dyn DurableStore>) {
    claimed(&b, &TaskId::new("a"), 0, 100).await;
    assert_eq!(
        b.reclaim_expired(99).await.unwrap(),
        0,
        "a live lease is kept"
    );
    assert_eq!(
        b.reclaim_expired(100).await.unwrap(),
        1,
        "an expired one is taken"
    );
    assert_eq!(row(&b, &TaskId::new("a")).await.status, Status::Ready);
}

/// Enqueues `id` under `job` and claims it for `owner`, as [`claimed`] does for runner `r`.
async fn claimed_by(
    b: &Arc<dyn DurableStore>,
    id: &str,
    job: &str,
    owner: &str,
    now: i64,
    ttl: i64,
) -> LeaseToken {
    b.enqueue(
        &TaskSpec::new(
            id.parse::<TaskId>().unwrap(),
            job.parse::<JobId>().unwrap(),
            AgentKey::new("k"),
            json!(id),
        ),
        now,
    )
    .await
    .unwrap();
    let c = b
        .claim_ready(owner, now, ttl)
        .await
        .unwrap()
        .expect("the task just enqueued");
    assert_eq!(
        c.spec.id,
        id.parse::<TaskId>().unwrap(),
        "premise: it was the only claimable task"
    );
    c.token
}

async fn lease_reclaim_orphans_takes_expired_and_named_owner_rows_but_never_live_ones(
    b: Arc<dyn DurableStore>,
) {
    let live = claimed_by(&b, "live", "j", "me", 1_000, 10).await;
    claimed_by(&b, "expired", "j", "other", 1_000, 10).await;
    claimed_by(&b, "foreign", "j", "other", 1_000, 1_000_000).await;
    claimed_by(&b, "previous", "j", "me", 1_000, 1_000_000).await;

    let n = b.reclaim_orphans(None, &[live], 5_000).await.unwrap();
    assert_eq!(n, 1, "expiry alone reclaims only `expired`");
    assert_eq!(row(&b, &TaskId::new("expired")).await.status, Status::Ready);
    assert_eq!(
        row(&b, &TaskId::new("live")).await.status,
        Status::Running,
        "live is never reclaimed"
    );
    assert_eq!(
        row(&b, &TaskId::new("previous")).await.status,
        Status::Running,
        "without `previous`, our own id waits out its lease"
    );

    let n = b.reclaim_orphans(Some("me"), &[live], 5_000).await.unwrap();
    assert_eq!(n, 1, "naming the owner takes its unexpired row");
    assert_eq!(
        row(&b, &TaskId::new("previous")).await.status,
        Status::Ready
    );
    assert_eq!(
        row(&b, &TaskId::new("live")).await.status,
        Status::Running,
        "live stays live even when it is ours"
    );
    assert_eq!(
        row(&b, &TaskId::new("foreign")).await.status,
        Status::Running,
        "another owner's valid lease is untouched"
    );
}

async fn lease_expiry(b: &Arc<dyn DurableStore>, id: &TaskId) -> Option<i64> {
    row(b, id).await.lease.map(|l| l.expires_at)
}

async fn lease_renew_and_release_move_only_the_lease_their_token_holds(b: Arc<dyn DurableStore>) {
    let mine = claimed_by(&b, "mine", "j", "me", 1_000, 10).await;
    claimed_by(&b, "theirs", "j", "other", 1_000, 10).await;
    let done = claimed_by(&b, "done", "j", "me", 1_000, 10).await;
    b.commit(&TaskId::new("done"), Writer::Claim(done), json!(null), 0)
        .await
        .unwrap();

    let absent = LeaseToken::from(uuid::Uuid::new_v4());
    let n = b.renew_leases(&[mine, done, absent], 99_000).await.unwrap();
    assert_eq!(n, 2, "`mine`, and `done` until its runner releases it");
    assert_eq!(lease_expiry(&b, &TaskId::new("mine")).await, Some(99_000));
    assert_eq!(
        lease_expiry(&b, &TaskId::new("theirs")).await,
        Some(1_010),
        "someone else's lease is theirs to renew"
    );
    assert_eq!(lease_expiry(&b, &TaskId::new("done")).await, Some(99_000));
    assert_eq!(row(&b, &TaskId::new("done")).await.status, Status::Done);

    b.release_lease(&TaskId::new("done"), mine).await.unwrap();
    assert_eq!(
        lease_expiry(&b, &TaskId::new("done")).await,
        Some(99_000),
        "another claim's token releases nothing"
    );
    b.release_lease(&TaskId::new("done"), done).await.unwrap();
    assert_eq!(lease_expiry(&b, &TaskId::new("done")).await, None);
    assert_eq!(
        b.renew_leases(&[done], 199_000).await.unwrap(),
        0,
        "a released claim renews nothing"
    );
}

/// A task fails and the same runner claims it again: the first claim's token neither
/// renews the second claim's lease nor shields it from the sweep.
async fn lease_a_re_claim_gets_a_lease_the_first_claim_cannot_touch(b: Arc<dyn DurableStore>) {
    let first = claimed(&b, &TaskId::new("t"), 1_000, 10).await;
    b.fail(&TaskId::new("t"), Writer::Claim(first), 3, boom(), 0)
        .await
        .unwrap();
    let second = b.claim_ready("r", 2_000, 10).await.unwrap().unwrap().token;
    assert_ne!(first, second, "every claim mints its own token");

    assert_eq!(
        b.renew_leases(&[first], 99_000).await.unwrap(),
        0,
        "the ended claim renews nothing"
    );
    let lease = row(&b, &TaskId::new("t")).await.lease.unwrap();
    assert_eq!((lease.token, lease.expires_at), (second, 2_010));
    assert_eq!(
        b.reclaim_orphans(None, &[first], 5_000).await.unwrap(),
        1,
        "the ended claim does not shield the second one's expired lease"
    );

    let third = b.claim_ready("r", 6_000, 10).await.unwrap().unwrap().token;
    assert_eq!(b.renew_leases(&[third], 99_000).await.unwrap(), 1);
    assert_eq!(
        b.reclaim_orphans(None, &[third], 200_000).await.unwrap(),
        0,
        "a live claim is never reclaimed"
    );
}

/// The same journal position extends the streak; a journal entry written in between
/// starts a new one.
async fn lease_a_reclaim_counts_the_interruption_against_the_journal_position(
    b: Arc<dyn DurableStore>,
) {
    claimed_by(&b, "t", "j", "dead", 1_000, 10).await;
    assert_eq!(row(&b, &TaskId::new("t")).await.interrupted, None);

    assert_eq!(b.reclaim_expired(5_000).await.unwrap(), 1);
    assert_eq!(
        row(&b, &TaskId::new("t")).await.interrupted,
        Some(InterruptionStreak { at: 0, times: 1 }),
        "the expiry sweep counts"
    );

    b.claim_ready("dead", 6_000, 10).await.unwrap().unwrap();
    assert_eq!(
        b.reclaim_orphans(Some("dead"), &[], 6_000).await.unwrap(),
        1
    );
    assert_eq!(
        row(&b, &TaskId::new("t")).await.interrupted,
        Some(InterruptionStreak { at: 0, times: 2 }),
        "so does a runner's own-orphan reclaim, and nothing moved in between"
    );

    // The claim's own `Running` event is not a journal entry; the delta is.
    let claim = b
        .claim_ready("dead", 7_000, 10)
        .await
        .unwrap()
        .unwrap()
        .token;
    b.append_delta(
        Writer::Claim(claim),
        &TaskId::new("t"),
        json!({"step": 0}),
        0,
        7_000,
    )
    .await
    .unwrap();
    b.reclaim_orphans(None, &[], 9_000).await.unwrap();
    assert_eq!(
        row(&b, &TaskId::new("t")).await.interrupted,
        Some(InterruptionStreak { at: 1, times: 1 }),
        "progress starts a new streak"
    );
}

async fn open_tasks_is_the_unfinished_work_of_one_job(b: Arc<dyn DurableStore>) {
    claimed_by(&b, "running", "j", "me", 1_000, 1_000_000).await;
    let done = claimed_by(&b, "done", "j", "me", 1_000, 1_000_000).await;
    b.commit(&TaskId::new("done"), Writer::Claim(done), json!(null), 0)
        .await
        .unwrap();
    b.enqueue(
        &TaskSpec::new(
            TaskId::new("ready"),
            JobId::new("j"),
            AgentKey::new("k"),
            json!(0),
        ),
        0,
    )
    .await
    .unwrap();
    b.enqueue(
        &TaskSpec::new(
            TaskId::new("elsewhere"),
            JobId::new("other"),
            AgentKey::new("k"),
            json!(0),
        ),
        0,
    )
    .await
    .unwrap();

    let mut open: Vec<String> = b
        .open_tasks(&JobId::new("j"))
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.spec.id.into_inner())
        .collect();
    open.sort();
    assert_eq!(open, ["ready", "running"]);
}

async fn a_task_row_keeps_its_intent_and_its_judge(b: Arc<dyn DurableStore>) {
    let judged = TaskSpec::new(
        TaskId::new("judged"),
        JobId::new("j"),
        AgentKey::new("coder"),
        json!({ "round": 2 }),
    )
    .judged_by(AgentKey::new("reviewer"));
    let unjudged = TaskSpec::new(
        TaskId::new("unjudged"),
        JobId::new("j"),
        AgentKey::new("coder"),
        Value::Null,
    );
    b.enqueue(&judged, 0).await.unwrap();
    b.enqueue(&unjudged, 0).await.unwrap();

    let back = row(&b, &TaskId::new("judged")).await.spec;
    assert_eq!(back.intent, json!({ "round": 2 }));
    assert_eq!(back.judge.as_ref().map(AgentKey::as_str), Some("reviewer"));
    let back = row(&b, &TaskId::new("unjudged")).await.spec;
    assert_eq!(back.intent, Value::Null);
    assert_eq!(back.judge, None, "an unjudged task reads back self-judged");

    retire(&b, &TaskId::new("judged")).await;
    retire(&b, &TaskId::new("unjudged")).await;
}
