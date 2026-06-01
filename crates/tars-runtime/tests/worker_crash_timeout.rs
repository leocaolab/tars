//! P2 (worker-crash isolation) + P3 (per-step time budget).
//!
//!   1. `panic_is_isolated_not_task_abort`: a worker that `panic!`s is
//!      caught and surfaced as `WorkerError::Crashed` — `run_plan`
//!      returns a normal `Err`, the process does NOT unwind. Sibling
//!      steps in the same plan still complete.
//!   2. `panic_retries_under_infra_policy`: with an infra-retry budget,
//!      a worker that panics the first N times then succeeds makes the
//!      plan pass (Crashed classifies Transient).
//!   3. `step_time_budget_aborts_a_hung_worker`: a worker that sleeps
//!      past `step_time_budget` is aborted as TimedOut; with no retry
//!      budget the plan fails fast instead of hanging forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use tars_runtime::{
    AgentMessage, InfraRetryPolicy, LocalRuntime, Plan, PlanStep, RunPlanConfig, Runtime,
    StepCondition, Worker, WorkerContext, WorkerError, WorkerOutput, WorkerRegistry, run_plan,
};
use tars_storage::{EventStore, SqliteEventStore};
use tars_types::AgentId;

fn runtime() -> Arc<LocalRuntime> {
    let store: Arc<dyn EventStore> = SqliteEventStore::in_memory().expect("in-memory store");
    LocalRuntime::new(store)
}

fn ok_output(step: &PlanStep) -> WorkerOutput {
    WorkerOutput {
        message: AgentMessage::PartialResult {
            from_agent: AgentId::new("w"),
            step_id: Some(step.id.clone()),
            summary: "ok".into(),
            confidence: 1.0,
        },
        usage: Default::default(),
    }
}

fn step(id: &str, role: &str) -> PlanStep {
    PlanStep {
        id: id.into(),
        worker_role: role.into(),
        instruction: "x".into(),
        depends_on: vec![],
        condition: StepCondition::Always,
    }
}

// ── 1 + 2: panic isolation + retry ─────────────────────────────────────

/// Panics its first `panic_n` invocations, then succeeds.
struct PanicWorker {
    panic_n: u32,
    calls: Arc<AtomicU32>,
}

#[async_trait]
impl Worker for PanicWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.panic_n {
            panic!("boom on attempt {n}");
        }
        Ok(ok_output(step))
    }
}

/// Always-succeeds worker, for the sibling in the isolation test.
struct OkWorker;

#[async_trait]
impl Worker for OkWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        Ok(ok_output(step))
    }
}

#[tokio::test]
async fn panic_is_isolated_not_task_abort() {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "panic-iso").await.unwrap();

    // Two independent steps: one panics (no retry budget → surfaces),
    // one is fine. run_plan must return a normal Err, NOT unwind.
    let mut registry = WorkerRegistry::new();
    registry.register(
        "panic",
        Arc::new(PanicWorker {
            panic_n: 99,
            calls: Arc::new(AtomicU32::new(0)),
        }),
    );
    registry.register("ok", Arc::new(OkWorker));

    let plan = Plan {
        plan_id: "iso".into(),
        goal: "isolation".into(),
        steps: vec![step("bad", "panic"), step("good", "ok")],
    };

    let res = run_plan(
        rt.clone(),
        traj,
        plan,
        registry,
        None,
        RunPlanConfig::default(), // no infra retry
        CancellationToken::new(),
    )
    .await;

    // The crash came back as a value, not a process unwind. (If the
    // panic had propagated, this line never runs — the test harness
    // would report a panic, not a failed assert.)
    assert!(res.is_err(), "panicking worker surfaces as Err");
    let msg = format!("{}", res.unwrap_err());
    assert!(
        msg.contains("panicked") || msg.contains("boom"),
        "error names the crash: {msg}",
    );
}

#[tokio::test]
async fn panic_retries_under_infra_policy() {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "panic-retry").await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));

    let registry = WorkerRegistry::new().with_default(Arc::new(PanicWorker {
        panic_n: 2, // panic twice, succeed on the 3rd
        calls: calls.clone(),
    }));

    let outcome = run_plan(
        rt.clone(),
        traj,
        Plan {
            plan_id: "pr".into(),
            goal: "panic retry".into(),
            steps: vec![step("s", "default")],
        },
        registry,
        None,
        RunPlanConfig {
            infra_retry: InfraRetryPolicy {
                max_attempts: 3,
                backoffs: vec![Duration::from_millis(1)],
                ..Default::default()
            },
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await
    .expect("recovers within the crash-retry budget");

    assert!(outcome.steps[0].as_completed().is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 3, "2 panics + 1 success");
}

// ── 3: step time budget ────────────────────────────────────────────────

struct HangWorker;

#[async_trait]
impl Worker for HangWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        // Sleeps well past the budget the test sets.
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(ok_output(step))
    }
}

#[tokio::test]
async fn step_time_budget_aborts_a_hung_worker() {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "budget").await.unwrap();

    let registry = WorkerRegistry::new().with_default(Arc::new(HangWorker));

    // 50ms budget, no retry → the plan fails fast (well under the
    // worker's 30s sleep) instead of hanging.
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        run_plan(
            rt.clone(),
            traj,
            Plan {
                plan_id: "b".into(),
                goal: "budget".into(),
                steps: vec![step("hang", "default")],
            },
            registry,
            None,
            RunPlanConfig {
                step_time_budget: Some(Duration::from_millis(50)),
                ..Default::default()
            },
            CancellationToken::new(),
        ),
    )
    .await
    .expect("run_plan returned promptly — the budget fired, no hang");

    assert!(res.is_err(), "hung worker surfaces as Err");
    let msg = format!("{}", res.unwrap_err());
    assert!(msg.contains("timed out"), "error names the timeout: {msg}");
}
