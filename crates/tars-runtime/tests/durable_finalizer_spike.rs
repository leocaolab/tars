//! SPIKE (disposable evidence — brainstorm §9-F / task #40): structured
//! concurrency + cancel cascade + FINALIZER contract on the durable core.
//!
//! The #40 hazard: an agent acquires a git worktree (or an LLM stream); a sibling
//! fails; the batch aborts — and the worktree is LEFT ON DISK (drop cancels the
//! future but can't run async cleanup). The autonomous-run moat is: on cancel the
//! finalizer MUST run (ZIO `ensuring` / `acquireRelease`).
//!
//! Contract proven here: the scheduler shares one child cancel token across a
//! batch and, on the first error, CANCELS it and DRAINS the siblings — so a
//! cooperative worker (`select!` on `ctx.cancel`) runs its finalizer before the
//! batch returns, instead of leaking. Two roots run concurrently: `boom` fails
//! immediately; `slow` acquires a worktree and awaits cancel → releases it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tars_runtime::{
    AgentMessage, AnswerStore, DurableScheduler, LocalRuntime, Plan, PlanStep, Runtime,
    StepCondition, Worker, WorkerContext, WorkerError, WorkerOutput, WorkerRegistry,
};
use tars_storage::SqliteAgentEventLog;
use tars_types::{AgentId, Usage};

/// A stand-in for physical resources (git worktrees). `live` = currently held;
/// a leak is a non-empty `live` after the run. `ever` records that acquire ran.
#[derive(Clone, Default)]
struct Worktrees {
    live: Arc<Mutex<HashSet<String>>>,
    ever: Arc<Mutex<HashSet<String>>>,
}
impl Worktrees {
    fn acquire(&self, id: &str) {
        self.live.lock().unwrap().insert(id.to_string());
        self.ever.lock().unwrap().insert(id.to_string());
    }
    fn release(&self, id: &str) {
        self.live.lock().unwrap().remove(id);
    }
    fn live_count(&self) -> usize {
        self.live.lock().unwrap().len()
    }
    fn ever_had(&self, id: &str) -> bool {
        self.ever.lock().unwrap().contains(id)
    }
}

fn mkstep(id: &str, role: &str) -> PlanStep {
    PlanStep {
        id: id.into(),
        worker_role: role.into(),
        instruction: format!("do {id}"),
        depends_on: vec![],
        condition: StepCondition::Always,
    }
}

fn partial(step: &PlanStep) -> WorkerOutput {
    WorkerOutput {
        message: AgentMessage::PartialResult {
            from_agent: AgentId::new("w"),
            step_id: Some(step.id.clone()),
            summary: format!("{} done", step.id),
            confidence: 1.0,
        },
        usage: Usage::default(),
        created: 0,
    }
}

/// Acquires a worktree, then races real work against cancel. On EITHER exit it
/// releases — the `acquireRelease` discipline. Here "work" never completes on
/// its own, so only the cancel path fires (the abort case we're proving).
struct SlowWorker {
    wt: Worktrees,
}
#[async_trait]
impl Worker for SlowWorker {
    async fn run(
        &self,
        _p: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        self.wt.acquire(&step.id); // acquire the worktree
        let out = tokio::select! {
            // Real work (never finishes here) vs the batch cancel.
            _ = std::future::pending::<()>() => Ok(partial(step)),
            _ = ctx.cancel.cancelled() => {
                Err(WorkerError::InvalidResult(format!("`{}` cancelled", step.id)))
            }
        };
        self.wt.release(&step.id); // FINALIZER — runs on BOTH paths
        out
    }
}

/// Fails immediately — the sibling that triggers the batch abort.
struct BoomWorker;
#[async_trait]
impl Worker for BoomWorker {
    async fn run(
        &self,
        _p: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        Err(WorkerError::InvalidResult(format!("boom at `{}`", step.id)))
    }
}

fn rt() -> Arc<dyn Runtime> {
    LocalRuntime::new(SqliteAgentEventLog::in_memory().unwrap())
}

#[tokio::test]
async fn cancel_cascade_runs_finalizer_no_worktree_leak() {
    let wt = Worktrees::default();
    let store = AnswerStore::in_memory().unwrap();
    let plan = Plan {
        plan_id: "fin".into(),
        goal: "concurrent batch, one fails".into(),
        steps: vec![mkstep("slow", "slow"), mkstep("boom", "boom")],
    };
    store.create_job("job", &plan).unwrap();

    let mut reg = WorkerRegistry::new();
    reg.register("slow", Arc::new(SlowWorker { wt: wt.clone() }));
    reg.register("boom", Arc::new(BoomWorker));
    let sched = DurableScheduler::new(store.clone(), reg, rt());

    // boom fails → cascade cancel → slow observes it and releases.
    assert!(sched.run_job("job").await.is_err(), "the batch aborts on boom");

    // THE POINT: the cancelled sibling ran its finalizer — no leaked worktree.
    assert!(wt.ever_had("slow"), "slow did acquire a worktree");
    assert_eq!(wt.live_count(), 0, "finalizer ran on cancel — worktree released, not leaked");
}
