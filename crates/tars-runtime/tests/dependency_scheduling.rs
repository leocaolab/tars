//! Dependency-driven scheduling — the v0.7 win that the old
//! depth-batched level loop could NOT deliver.
//!
//!   1. `cross_level_overlap`: a tier-2 step (`fix-fast`, deps
//!      `scan-fast`) runs to completion WHILE a slow tier-1 sibling
//!      (`scan-slow`) is still in flight. Under depth-batching, `fix-*`
//!      couldn't start until every `scan-*` finished, so `fix-fast`
//!      would land AFTER `scan-slow`. Dependency-driven, it lands first.
//!   2. `max_concurrent_caps_in_flight`: with `max_concurrent: Some(1)`
//!      even fully-independent steps serialise (peak in-flight == 1);
//!      unbounded, they overlap (peak > 1).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use tars_runtime::{
    AgentMessage, LocalRuntime, Plan, PlanStep, RunPlanConfig, Runtime, StepCondition, Worker,
    WorkerContext, WorkerError, WorkerOutput, WorkerRegistry, run_plan,
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
            summary: "done".into(),
            confidence: 1.0,
        },
        usage: Default::default(),
    }
}

// ── Test 1: cross-level overlap ────────────────────────────────────────

/// Sleeps a per-step duration (keyed by step id prefix), then appends
/// its step id to a shared completion log. The log's order is the
/// observable proof of scheduling behaviour.
struct TimedWorker {
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Worker for TimedWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        // `scan-slow` dawdles; everything else is immediate.
        let delay = if step.id == "scan-slow" {
            Duration::from_millis(200)
        } else {
            Duration::ZERO
        };
        tokio::time::sleep(delay).await;
        self.log.lock().unwrap().push(step.id.clone());
        Ok(ok_output(step))
    }
}

#[tokio::test]
async fn cross_level_overlap_runs_tier2_before_slow_tier1_sibling() {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "overlap").await.unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));

    let plan = Plan {
        plan_id: "overlap".into(),
        goal: "prove cross-level scheduling".into(),
        steps: vec![
            PlanStep {
                id: "scan-slow".into(),
                worker_role: "w".into(),
                instruction: "slow root".into(),
                depends_on: vec![],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "scan-fast".into(),
                worker_role: "w".into(),
                instruction: "fast root".into(),
                depends_on: vec![],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "fix-fast".into(),
                worker_role: "w".into(),
                instruction: "tier-2, deps the fast root".into(),
                depends_on: vec!["scan-fast".into()],
                condition: StepCondition::Always,
            },
        ],
    };

    let registry = WorkerRegistry::new().with_default(Arc::new(TimedWorker { log: log.clone() }));

    run_plan(
        rt.clone(),
        traj,
        plan,
        registry,
        None,
        RunPlanConfig::default(), // unbounded concurrency
        CancellationToken::new(),
    )
    .await
    .expect("plan completes");

    let order = log.lock().unwrap().clone();
    let pos = |id: &str| order.iter().position(|s| s == id).expect("step ran");
    // The whole point: fix-fast finished before scan-slow — impossible
    // under the depth-batched level loop (it would gate fix-* behind
    // ALL scan-*).
    assert!(
        pos("fix-fast") < pos("scan-slow"),
        "tier-2 fix-fast must complete before slow tier-1 scan-slow; got order {order:?}",
    );
}

// ── Test 2: max_concurrent cap ─────────────────────────────────────────

/// Tracks peak simultaneous in-flight invocations via a live counter.
struct ConcurrencyProbe {
    in_flight: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
}

#[async_trait]
impl Worker for ConcurrencyProbe {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        // Hold the slot briefly so genuine overlap has a chance to show.
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ok_output(step))
    }
}

fn three_independent_steps() -> Plan {
    Plan {
        plan_id: "concurrency".into(),
        goal: "cap test".into(),
        steps: (0..3)
            .map(|i| PlanStep {
                id: format!("s{i}"),
                worker_role: "w".into(),
                instruction: "independent".into(),
                depends_on: vec![],
                condition: StepCondition::Always,
            })
            .collect(),
    }
}

async fn peak_under(max_concurrent: Option<usize>) -> u32 {
    let rt = runtime();
    let traj = rt.create_trajectory(None, "cap").await.unwrap();
    let peak = Arc::new(AtomicU32::new(0));
    let registry = WorkerRegistry::new().with_default(Arc::new(ConcurrencyProbe {
        in_flight: Arc::new(AtomicU32::new(0)),
        peak: peak.clone(),
    }));
    run_plan(
        rt.clone(),
        traj,
        three_independent_steps(),
        registry,
        None,
        RunPlanConfig {
            max_concurrent,
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await
    .expect("plan completes");
    peak.load(Ordering::SeqCst)
}

#[tokio::test]
async fn max_concurrent_caps_in_flight_workers() {
    // Cap of 1 → strict serialisation, peak can never exceed 1.
    assert_eq!(peak_under(Some(1)).await, 1, "cap=1 serialises");
    // Unbounded → all three independent steps overlap.
    assert!(
        peak_under(None).await > 1,
        "unbounded scheduling overlaps independent steps",
    );
}
