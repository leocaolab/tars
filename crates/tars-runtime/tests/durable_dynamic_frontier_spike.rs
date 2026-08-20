//! SPIKE (disposable evidence — brainstorm §12 / task #43): can the durable step
//! core grow the plan AT RUNTIME (dynamic frontier), the shape `auto` needs?
//!
//! A `triage` step discovers 3 issues and appends `fix-i` / `verify-i` steps to
//! the live job (`AnswerStore::append_steps`); the scheduler re-reads the plan
//! each pass and drives them. Two judges:
//!   1. `dynamic_frontier_grows_and_converges` — triage → 3×(fix→verify) all run
//!      once, job DONE. Proves runtime step insertion works on the existing core.
//!   2. `dynamic_frontier_crash_resume_no_recall_no_dup` — crash on `fix-2`,
//!      reopen, resume → completed steps skip (LLM not re-called), triage's
//!      re-run appends NO duplicates (idempotent id), converges to the same shape.
//!
//! Verdict this proves: #42's dynamic frontier is a SMALL clean addition on top
//! of the already-tested durable core (one store seam + re-read plan per pass),
//! not a rewrite.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tars_runtime::{
    AgentMessage, AnswerStore, DurableScheduler, LocalRuntime, Plan, PlanStep, Runtime,
    StepCondition, Worker, WorkerContext, WorkerError, WorkerOutput, WorkerRegistry,
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

fn ok(step: &PlanStep, prior: usize) -> WorkerOutput {
    WorkerOutput {
        message: AgentMessage::PartialResult {
            from_agent: AgentId::new(format!("worker:{}", step.worker_role)),
            step_id: Some(step.id.clone()),
            summary: format!("{} done (deps={})", step.id, prior),
            confidence: 1.0,
        },
        usage: Usage::default(),
        created: 0,
    }
}

/// The dynamic-frontier step: on run, GROW the plan with `fix-i`/`verify-i` for
/// each discovered issue. Idempotent on id, so a resumed re-run appends nothing.
struct TriageWorker {
    store: AnswerStore,
    job_id: String,
    issues: Vec<u32>,
    runs: Recorder,
}

#[async_trait]
impl Worker for TriageWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        *self.runs.lock().unwrap().entry(step.id.clone()).or_insert(0) += 1;
        let mut new_steps = Vec::new();
        for i in &self.issues {
            new_steps.push(mkstep(&format!("fix-{i}"), "fix", vec![step_id(step)]));
            new_steps.push(mkstep(&format!("verify-{i}"), "verify", vec![&format!("fix-{i}")]));
        }
        self.store
            .append_steps(&self.job_id, &new_steps)
            .map_err(|e| WorkerError::InvalidResult(format!("append_steps: {e}")))?;
        Ok(ok(step, prior.len()))
    }
}

fn step_id(s: &PlanStep) -> &str {
    &s.id
}

/// A counting fix/verify worker; can fail once on one step id (crash sim).
struct StepWorker {
    runs: Recorder,
    fail_on: Option<String>,
}

#[async_trait]
impl Worker for StepWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        *self.runs.lock().unwrap().entry(step.id.clone()).or_insert(0) += 1;
        if self.fail_on.as_deref() == Some(step.id.as_str()) {
            return Err(WorkerError::InvalidResult(format!("simulated crash at `{}`", step.id)));
        }
        Ok(ok(step, prior.len()))
    }
}

fn rt() -> Arc<dyn Runtime> {
    LocalRuntime::new(SqliteAgentEventLog::in_memory().unwrap())
}

fn count(r: &Recorder, s: &str) -> usize {
    r.lock().unwrap().get(s).copied().unwrap_or(0)
}

fn registry(triage: Arc<TriageWorker>, fixv: Arc<StepWorker>) -> WorkerRegistry {
    let mut reg = WorkerRegistry::new();
    reg.register("triage", triage);
    reg.register("fix", fixv.clone());
    reg.register("verify", fixv);
    reg
}

/// The seed plan: just `triage`. Everything else is discovered at runtime.
fn seed_plan() -> Plan {
    Plan {
        plan_id: "auto".into(),
        goal: "triage then fix+verify each issue".into(),
        steps: vec![mkstep("triage", "triage", vec![])],
    }
}

#[tokio::test]
async fn dynamic_frontier_grows_and_converges() {
    let store = AnswerStore::in_memory().unwrap();
    store.create_job("job", &seed_plan()).unwrap();
    let runs = Recorder::default();

    let triage = Arc::new(TriageWorker {
        store: store.clone(),
        job_id: "job".into(),
        issues: vec![1, 2, 3],
        runs: runs.clone(),
    });
    let fixv = Arc::new(StepWorker { runs: runs.clone(), fail_on: None });
    let sched = DurableScheduler::new(store.clone(), registry(triage, fixv), rt());

    sched.run_job("job").await.expect("dynamic job converges");

    // Plan grew: triage + 3 fix + 3 verify = 7 steps, all answered.
    assert_eq!(store.load_plan("job").unwrap().steps.len(), 7, "plan grew at runtime");
    assert_eq!(store.job_status("job").unwrap().as_deref(), Some("done"));
    assert_eq!(count(&runs, "triage"), 1);
    for i in 1..=3 {
        assert_eq!(count(&runs, &format!("fix-{i}")), 1, "fix-{i} ran once");
        assert_eq!(count(&runs, &format!("verify-{i}")), 1, "verify-{i} ran once");
        assert!(store.answer("job", &format!("verify-{i}")).unwrap().is_some());
    }
}

#[tokio::test]
async fn dynamic_frontier_crash_resume_no_recall_no_dup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auto.sqlite");
    let runs = Recorder::default();

    // Phase 1 — crash on fix-2 (after triage grew the plan + some fixes ran).
    {
        let store = AnswerStore::open(&path).unwrap();
        store.create_job("job", &seed_plan()).unwrap();
        let triage = Arc::new(TriageWorker {
            store: store.clone(),
            job_id: "job".into(),
            issues: vec![1, 2, 3],
            runs: runs.clone(),
        });
        let fixv = Arc::new(StepWorker { runs: runs.clone(), fail_on: Some("fix-2".into()) });
        let sched = DurableScheduler::new(store.clone(), registry(triage, fixv), rt());
        assert!(sched.run_job("job").await.is_err(), "fix-2 crash surfaces");
        // triage grew the plan + persisted before the crash.
        assert_eq!(store.load_plan("job").unwrap().steps.len(), 7);
        assert!(store.answer("job", "fix-2").unwrap().is_none(), "crashed step un-done");
    }

    // Phase 2 — reopen, resume with a healthy worker.
    {
        let store = AnswerStore::open(&path).unwrap();
        let triage = Arc::new(TriageWorker {
            store: store.clone(),
            job_id: "job".into(),
            issues: vec![1, 2, 3],
            runs: runs.clone(),
        });
        let fixv = Arc::new(StepWorker { runs: runs.clone(), fail_on: None });
        let sched = DurableScheduler::new(store.clone(), registry(triage, fixv), rt());
        sched.run_job("job").await.expect("resume converges");
        assert_eq!(store.job_status("job").unwrap().as_deref(), Some("done"));
    }

    // triage completed pre-crash → NOT re-run on resume → NO duplicate steps.
    assert_eq!(count(&runs, "triage"), 1, "triage not re-run; append stayed idempotent");
    assert_eq!(store_steps(&path), 7, "plan did NOT grow to 10 (no duplicate fix/verify)");
    // fix-2 was the crash window → re-ran once on resume; the rest ran once total.
    assert_eq!(count(&runs, "fix-2"), 2, "only the un-done crash step re-ran");
    assert_eq!(count(&runs, "fix-1"), 1, "completed fix-1 NOT re-called");
    assert_eq!(count(&runs, "verify-1"), 1);
}

fn store_steps(path: &std::path::Path) -> usize {
    AnswerStore::open(path).unwrap().load_plan("job").unwrap().steps.len()
}
