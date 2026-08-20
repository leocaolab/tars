//! SPIKE (disposable evidence — brainstorm §12 / task #42 HITL): human-in-the-loop
//! is a SPECIAL CASE of durable resume — a step whose answer is filled by a HUMAN
//! instead of a worker. Prove it needs no new machinery beyond an "awaited role".
//!
//! Plan: review → approve(human) → merge. The scheduler PARKS at `approve` (a
//! role it never auto-runs), suspending the job (Ok, status stays `running`) —
//! NOT failing, NOT looping. A human commits `approve`'s answer OUT OF BAND
//! (across a store reopen — the "park for a day, resume in a fresh process"
//! scenario); resume unlocks `merge` and converges. review is NOT re-run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tars_runtime::{
    AgentMessage, AnswerStore, DurableScheduler, LocalRuntime, Plan, PlanStep, ResultEventKind,
    Runtime, StepAnswer, StepCondition, Worker, WorkerContext, WorkerError, WorkerOutput,
    WorkerRegistry,
};
use tars_storage::SqliteAgentEventLog;
use tars_types::{AgentId, Usage};

type Recorder = Arc<Mutex<HashMap<String, usize>>>;

fn mkstep(id: &str, role: &str, deps: Vec<&str>) -> PlanStep {
    PlanStep {
        id: id.into(),
        worker_role: role.into(),
        instruction: format!("do {id}"),
        depends_on: deps.into_iter().map(String::from).collect(),
        condition: StepCondition::Always,
    }
}

/// The `review`/`merge` worker — counts runs, deterministic output.
struct WorkWorker {
    runs: Recorder,
}

#[async_trait]
impl Worker for WorkWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        *self.runs.lock().unwrap().entry(step.id.clone()).or_insert(0) += 1;
        Ok(WorkerOutput {
            message: AgentMessage::PartialResult {
                from_agent: AgentId::new("worker:work"),
                step_id: Some(step.id.clone()),
                summary: format!("{} done (deps={})", step.id, prior.len()),
                confidence: 1.0,
            },
            usage: Usage::default(),
            created: 0,
        })
    }
}

fn rt() -> Arc<dyn Runtime> {
    LocalRuntime::new(SqliteAgentEventLog::in_memory().unwrap())
}

fn count(r: &Recorder, s: &str) -> usize {
    r.lock().unwrap().get(s).copied().unwrap_or(0)
}

fn registry(worker: Arc<WorkWorker>) -> WorkerRegistry {
    let mut reg = WorkerRegistry::new();
    reg.register("work", worker);
    // NB: no worker for role "human" — it is an AWAITED role (completed by a
    // person via commit_step), declared on the scheduler.
    reg
}

fn hitl_plan() -> Plan {
    Plan {
        plan_id: "hitl".into(),
        goal: "review, human approve, merge".into(),
        steps: vec![
            mkstep("review", "work", vec![]),
            mkstep("approve", "human", vec!["review"]),
            mkstep("merge", "work", vec!["approve"]),
        ],
    }
}

#[tokio::test]
async fn hitl_parks_then_human_wake_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hitl.sqlite");
    let runs = Recorder::default();

    // ── Phase 1: run → review executes, then PARK at the human step. ──
    {
        let store = AnswerStore::open(&path).unwrap();
        store.create_job("job", &hitl_plan()).unwrap();
        let worker = Arc::new(WorkWorker { runs: runs.clone() });
        let sched = DurableScheduler::new(store.clone(), registry(worker), rt())
            .with_awaited_roles(["human"]);

        // Suspends cleanly (Ok, NOT Err, NOT done) — parked awaiting the human.
        sched.run_job("job").await.expect("parks at human step (suspended, not failed)");
        assert_eq!(store.job_status("job").unwrap().as_deref(), Some("running"), "suspended, not done");
        assert!(store.answer("job", "review").unwrap().is_some(), "review ran before the park");
        assert!(store.answer("job", "approve").unwrap().is_none(), "human step un-answered");
        assert!(store.answer("job", "merge").unwrap().is_none(), "merge blocked on the human");
    }
    assert_eq!(count(&runs, "review"), 1);

    // ── Phase 2: a HUMAN approves OUT OF BAND — reopen the file (a fresh
    // process, possibly a day later) and commit the awaited step's answer. ──
    {
        let store = AnswerStore::open(&path).unwrap();
        let human = AgentMessage::PartialResult {
            from_agent: AgentId::new("human:reviewer"),
            step_id: Some("approve".into()),
            summary: "APPROVED by human".into(),
            confidence: 1.0,
        };
        let answer = StepAnswer::completed("job", "approve", human, Usage::default(), 0);
        store.commit_step(&answer, ResultEventKind::Completed, None).unwrap();
    }

    // ── Phase 3: resume → the human Wake unlocks `merge` → converge. ──
    {
        let store = AnswerStore::open(&path).unwrap();
        let worker = Arc::new(WorkWorker { runs: runs.clone() });
        let sched = DurableScheduler::new(store.clone(), registry(worker), rt())
            .with_awaited_roles(["human"]);
        sched.run_job("job").await.expect("resume after human wake converges");
        assert_eq!(store.job_status("job").unwrap().as_deref(), Some("done"));
        assert!(store.answer("job", "merge").unwrap().is_some(), "merge ran after approval");
    }

    // review was NOT re-run on resume; merge ran once.
    assert_eq!(count(&runs, "review"), 1, "completed step not re-run across the human park");
    assert_eq!(count(&runs, "merge"), 1);
}
