//! `RunPlanConfig::infra_retry` — Plan-level retry for infra failures
//! (rate-limit / circuit-open / timeout), lifted out of Worker bodies.
//!
//! Proves the three contract points:
//!   1. a retryable (InfraClass::RateLimited / Transient) failure is
//!      re-run up to `max_attempts`, and a worker that recovers within
//!      budget makes the whole plan succeed;
//!   2. exhausting the budget surfaces the underlying error;
//!   3. a NotInfra failure is NOT retried (one shot, then surface) —
//!      so a deterministic worker bug never spins the budget.

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

/// Worker that fails its first `fail_n` invocations with `err_msg`,
/// then succeeds. Records the total invocation count so the test can
/// assert exactly how many attempts happened.
struct FlakyWorker {
    fail_n: u32,
    err_msg: String,
    calls: Arc<AtomicU32>,
}

#[async_trait]
impl Worker for FlakyWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_n {
            return Err(WorkerError::InvalidResult(self.err_msg.clone()));
        }
        Ok(WorkerOutput {
            message: AgentMessage::PartialResult {
                from_agent: AgentId::new("flaky"),
                step_id: Some(step.id.clone()),
                summary: "recovered".into(),
                confidence: 1.0,
            },
            usage: Default::default(),
        })
    }
}

fn one_step_plan() -> Plan {
    Plan {
        plan_id: "infra-retry-test".into(),
        goal: "exercise infra retry".into(),
        steps: vec![PlanStep {
            id: "flaky".into(),
            worker_role: "flaky".into(),
            instruction: "do the flaky thing".into(),
            depends_on: vec![],
            condition: StepCondition::Always,
        }],
    }
}

fn runtime() -> Arc<LocalRuntime> {
    let store: Arc<dyn EventStore> =
        SqliteEventStore::in_memory().expect("in-memory store");
    LocalRuntime::new(store)
}

fn policy(max_attempts: u32) -> InfraRetryPolicy {
    InfraRetryPolicy {
        max_attempts,
        // Tiny backoffs so the test is fast but still exercises the
        // sleep + cancel-aware select path.
        backoffs: vec![Duration::from_millis(1)],
        ..Default::default()
    }
}

#[tokio::test]
async fn rate_limit_failure_retries_then_succeeds_within_budget() {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "retry-success").await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));

    // Fails twice with a "circuit open" message (default classifier →
    // RateLimited), succeeds on the 3rd. Budget of 3 covers it.
    let registry = WorkerRegistry::new().with_default(Arc::new(FlakyWorker {
        fail_n: 2,
        err_msg: "tars provider: circuit open for gemini".into(),
        calls: calls.clone(),
    }));

    let outcome = run_plan(
        rt.clone(),
        traj,
        one_step_plan(),
        registry,
        None,
        RunPlanConfig {
            infra_retry: policy(3),
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await
    .expect("plan succeeds after retrying through the rate-limit");

    assert_eq!(outcome.steps.len(), 1);
    assert!(outcome.steps[0].as_completed().is_some(), "step completed");
    // 2 failures + 1 success = 3 invocations.
    assert_eq!(calls.load(Ordering::SeqCst), 3, "worker invoked 3 times");
}

#[tokio::test]
async fn budget_exhaustion_surfaces_the_error() {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "retry-exhaust").await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));

    // Fails 5 times; budget only allows 2 retries (3 total attempts) →
    // the error must surface.
    let registry = WorkerRegistry::new().with_default(Arc::new(FlakyWorker {
        fail_n: 5,
        err_msg: "429 too many requests".into(),
        calls: calls.clone(),
    }));

    let res = run_plan(
        rt.clone(),
        traj,
        one_step_plan(),
        registry,
        None,
        RunPlanConfig {
            infra_retry: policy(2),
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await;

    assert!(res.is_err(), "budget exhausted → plan fails");
    // 1 initial + 2 retries = 3 attempts, then give up.
    assert_eq!(calls.load(Ordering::SeqCst), 3, "exactly budget+1 attempts");
}

#[tokio::test]
async fn non_infra_failure_is_not_retried() {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "no-retry").await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));

    // A decode-shaped failure the default classifier maps to NotInfra:
    // retrying a deterministic bug would just waste the budget.
    let registry = WorkerRegistry::new().with_default(Arc::new(FlakyWorker {
        fail_n: 99,
        err_msg: "worker returned a malformed shape".into(),
        calls: calls.clone(),
    }));

    let res = run_plan(
        rt.clone(),
        traj,
        one_step_plan(),
        registry,
        None,
        RunPlanConfig {
            infra_retry: policy(3),
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await;

    assert!(res.is_err(), "non-infra failure surfaces");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "NotInfra error invoked the worker exactly once — no retry",
    );
}
