//! Integration tests for the durable runtime (M0 + M1).
//!
//! - `m0_answer_event_job_survive_close_and_reopen` — M0 verify (a):
//!   answer + result_event + job/plan survive a close/reopen of the file.
//! - `events_off_still_persists_and_resumes` — the CRITICAL-invariant
//!   regression: with the observability runtime events OFF (a no-op
//!   `NullRuntime`), the answer/job/result_events STILL persist and the
//!   job STILL resumes.
//! - `e2e2_crash_mid_dag_skips_completed_and_does_not_recall_llm` — §9
//!   E2E-2: 3-node author→reviewer→author plan; crash after node 1;
//!   reopen → node 1 skipped (present answer), nodes 2-3 run; the mock
//!   worker's per-step call count proves the completed step is NOT
//!   re-executed; final state == a no-crash run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tars_durable::{DurableScheduler, DurableStore, ResultEventKind, StepAnswer};
use tars_runtime::{
    AgentEvent, AgentMessage, LocalRuntime, Plan, PlanStep, Runtime, RuntimeError, StepCondition,
    Worker, WorkerContext, WorkerError, WorkerOutput, WorkerRegistry,
};
use tars_storage::SqliteEventStore;
use tars_types::{AgentId, TrajectoryId, Usage};

// ─── Mocks ──────────────────────────────────────────────────────────────

/// A no-network `Worker` that (1) counts how many times each step id was
/// invoked (proving no re-call on resume) and (2) can be told to fail on
/// one step id (simulating a crash mid-DAG). Its output is deterministic
/// in `(step.id, prior_results.len())`, so a resumed run and a no-crash
/// run produce identical answers.
#[derive(Clone)]
struct CountingWorker {
    runs: Arc<Mutex<HashMap<String, usize>>>,
    fail_on: Option<String>,
}

impl CountingWorker {
    fn new(runs: Arc<Mutex<HashMap<String, usize>>>, fail_on: Option<&str>) -> Arc<Self> {
        Arc::new(Self { runs, fail_on: fail_on.map(str::to_string) })
    }
}

#[async_trait]
impl Worker for CountingWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        prior_results: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        *self.runs.lock().unwrap().entry(step.id.clone()).or_insert(0) += 1;
        if self.fail_on.as_deref() == Some(step.id.as_str()) {
            return Err(WorkerError::InvalidResult(format!("simulated crash at `{}`", step.id)));
        }
        Ok(WorkerOutput {
            message: AgentMessage::PartialResult {
                from_agent: AgentId::new(format!("worker:{}", step.worker_role)),
                step_id: Some(step.id.clone()),
                summary: format!("{} done (deps={})", step.id, prior_results.len()),
                confidence: 1.0,
            },
            usage: Usage::default(),
            created: 0,
        })
    }
}

/// A `Runtime` whose every observability write is a NO-OP — the stand-in
/// for events fully OFF / absent (`StoreScope::Off`, `*_EVENTS_OFF`). If
/// the durable path depended on the observability sink for correctness,
/// resume under this runtime would break; it must not.
struct NullRuntime;

#[async_trait]
impl Runtime for NullRuntime {
    async fn create_trajectory(
        &self,
        _parent: Option<TrajectoryId>,
        _reason: &str,
    ) -> Result<TrajectoryId, RuntimeError> {
        Ok(TrajectoryId::new("null"))
    }
    async fn append(&self, _t: &TrajectoryId, _e: AgentEvent) -> Result<u64, RuntimeError> {
        Ok(0)
    }
    async fn replay(&self, _t: &TrajectoryId) -> Result<Vec<AgentEvent>, RuntimeError> {
        Ok(Vec::new())
    }
    async fn replay_since(
        &self,
        _t: &TrajectoryId,
        _since: u64,
    ) -> Result<Vec<AgentEvent>, RuntimeError> {
        Ok(Vec::new())
    }
    async fn list_trajectories(&self) -> Result<Vec<TrajectoryId>, RuntimeError> {
        Ok(Vec::new())
    }
}

// ─── Fixtures ───────────────────────────────────────────────────────────

/// author → reviewer → author_rev (a linear 3-node MAS pipeline, CUJ-2).
fn author_reviewer_plan() -> Plan {
    let step = |id: &str, role: &str, deps: Vec<&str>| PlanStep {
        id: id.into(),
        worker_role: role.into(),
        instruction: format!("do {id}"),
        depends_on: deps.into_iter().map(String::from).collect(),
        condition: StepCondition::Always,
    };
    Plan {
        plan_id: "mas".into(),
        goal: "author, review, revise".into(),
        steps: vec![
            step("author", "author", vec![]),
            step("reviewer", "reviewer", vec!["author"]),
            step("author_rev", "author", vec!["reviewer"]),
        ],
    }
}

fn events_on_runtime() -> Arc<dyn Runtime> {
    LocalRuntime::new(SqliteEventStore::in_memory().unwrap())
}

fn registry(worker: Arc<CountingWorker>) -> WorkerRegistry {
    WorkerRegistry::new().with_default(worker)
}

fn runs_map() -> Arc<Mutex<HashMap<String, usize>>> {
    Arc::new(Mutex::new(HashMap::new()))
}

fn count(runs: &Arc<Mutex<HashMap<String, usize>>>, step: &str) -> usize {
    runs.lock().unwrap().get(step).copied().unwrap_or(0)
}

// ─── M0 ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn m0_answer_event_job_survive_close_and_reopen() {
    // Mirrors tars-cache/sqlite.rs:493 (append_survives_close_and_reopen):
    // one Blackboard::commit writes {answer + result_event}; the job row +
    // plan are persisted at submit. All must survive a file reopen.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("durable.sqlite");
    let plan = author_reviewer_plan();

    {
        let store = DurableStore::open(&path).unwrap();
        store.create_job("job", &plan).unwrap();
        let msg = AgentMessage::PartialResult {
            from_agent: AgentId::new("worker:author"),
            step_id: Some("author".into()),
            summary: "author done".into(),
            confidence: 1.0,
        };
        let answer = StepAnswer::completed("job", "author", msg, Usage::default(), 0);
        // ONE transaction: answer + Completed event + job updated_at.
        store.commit_step(&answer, ResultEventKind::Completed, None).unwrap();
        // Drop → connection closes → WAL flushes on next open.
    }

    let store = DurableStore::open(&path).unwrap();
    // Answer survived, status projected from the timeline (law #5).
    let got = store.answer("job", "author").unwrap().expect("answer persisted");
    assert_eq!(got.status, "completed");
    assert_eq!(got.summary_is_author(), "author done");
    // result_event survived, monotonic seq.
    let events = store.result_events("job").unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[0].step_id, "author");
    assert_eq!(events[0].kind, ResultEventKind::Completed);
    // Job row + plan (status of record) survived.
    assert_eq!(store.job_status("job").unwrap().as_deref(), Some("running"));
    assert_eq!(store.load_plan("job").unwrap().steps.len(), 3);
}

#[tokio::test]
async fn m0_commit_is_idempotent_on_key_kind() {
    // Law #3: a re-committed transition is absorbed — one result_event,
    // not two — and consumes no seq.
    let store = DurableStore::in_memory().unwrap();
    let plan = author_reviewer_plan();
    store.create_job("job", &plan).unwrap();
    let msg = AgentMessage::PartialResult {
        from_agent: AgentId::new("w"),
        step_id: Some("author".into()),
        summary: "author done".into(),
        confidence: 1.0,
    };
    let answer = StepAnswer::completed("job", "author", msg, Usage::default(), 0);
    store.commit_step(&answer, ResultEventKind::Completed, None).unwrap();
    store.commit_step(&answer, ResultEventKind::Completed, None).unwrap();
    assert_eq!(store.result_events("job").unwrap().len(), 1);
}

// ─── M0 critical-invariant regression ───────────────────────────────────

#[tokio::test]
async fn events_off_still_persists_and_resumes() {
    // The durability store must be independent of the OFF-able
    // observability EventStore. Drive under a NullRuntime (events OFF)
    // and confirm the answer/job/result_events persist AND the job
    // resumes — with the completed step NEVER re-run.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("durable.sqlite");
    let plan = author_reviewer_plan();
    let runs = runs_map();

    // Phase 1 — events OFF, crash at `reviewer`.
    {
        let store = DurableStore::open(&path).unwrap();
        store.create_job("j", &plan).unwrap();
        let worker = CountingWorker::new(runs.clone(), Some("reviewer"));
        let rt: Arc<dyn Runtime> = Arc::new(NullRuntime);
        let sched = DurableScheduler::new(store.clone(), registry(worker), rt);
        let res = sched.run_job("j").await;
        assert!(res.is_err(), "reviewer worker fails → run_job surfaces the error");

        // Despite events OFF, the durable truth persisted:
        assert!(store.answer("j", "author").unwrap().is_some(), "answer persisted with events off");
        assert_eq!(store.job_status("j").unwrap().as_deref(), Some("running"), "job state persisted");
        assert_eq!(store.result_events("j").unwrap().len(), 1, "author's result_event persisted");
    }

    // Phase 2 — reopen the file, still events OFF, resume to completion.
    {
        let store = DurableStore::open(&path).unwrap();
        let worker = CountingWorker::new(runs.clone(), None);
        let rt: Arc<dyn Runtime> = Arc::new(NullRuntime);
        let sched = DurableScheduler::new(store.clone(), registry(worker), rt);
        sched.run_job("j").await.expect("resume converges with events off");
        assert_eq!(store.job_status("j").unwrap().as_deref(), Some("done"));
        assert!(store.answer("j", "author_rev").unwrap().is_some());
    }

    assert_eq!(count(&runs, "author"), 1, "completed step NOT re-executed on resume (events off)");
}

// ─── M1 — E2E-2 ─────────────────────────────────────────────────────────

#[tokio::test]
async fn e2e2_crash_mid_dag_skips_completed_and_does_not_recall_llm() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("durable.sqlite");
    let plan = author_reviewer_plan();
    let runs = runs_map();

    // ── Phase 1: crash after node 1 (worker fails on `reviewer`). ──
    {
        let store = DurableStore::open(&path).unwrap();
        store.create_job("job1", &plan).unwrap();
        let worker = CountingWorker::new(runs.clone(), Some("reviewer"));
        let sched = DurableScheduler::new(store.clone(), registry(worker), events_on_runtime());
        let res = sched.run_job("job1").await;
        assert!(res.is_err(), "the crash-simulating reviewer failure surfaces");
        // node 1 completed + checkpointed; node 3 never reached.
        assert!(store.answer("job1", "author").unwrap().is_some());
        assert!(store.answer("job1", "author_rev").unwrap().is_none());
    }
    assert_eq!(count(&runs, "author"), 1, "author ran once pre-crash");

    // ── Phase 2: reopen the file, resume with a healthy worker. ──
    let final_job1: HashMap<String, String>;
    {
        let store = DurableStore::open(&path).unwrap();
        let worker = CountingWorker::new(runs.clone(), None);
        let sched = DurableScheduler::new(store.clone(), registry(worker), events_on_runtime());
        sched.run_job("job1").await.expect("resume converges");
        assert_eq!(store.job_status("job1").unwrap().as_deref(), Some("done"));
        final_job1 = summaries(&store, "job1");
    }

    // NFR-1/5: the completed step (author) was NOT re-executed on resume.
    assert_eq!(count(&runs, "author"), 1, "completed step must not be re-run (LLM not re-called)");
    // The crash-window step (reviewer) re-ran once on resume; node 3 ran once.
    assert_eq!(count(&runs, "reviewer"), 2, "only the un-done crash-window step re-ran");
    assert_eq!(count(&runs, "author_rev"), 1);

    // ── final == a no-crash run of the same plan ──
    let runs_fresh = runs_map();
    let store2 = DurableStore::in_memory().unwrap();
    store2.create_job("job2", &plan).unwrap();
    let worker = CountingWorker::new(runs_fresh.clone(), None);
    let sched = DurableScheduler::new(store2.clone(), registry(worker), events_on_runtime());
    sched.run_job("job2").await.expect("no-crash run converges");
    let final_fresh = summaries(&store2, "job2");
    assert_eq!(final_job1, final_fresh, "resumed final state matches the no-crash run");
    // The no-crash run ran each step exactly once.
    assert_eq!(count(&runs_fresh, "author"), 1);
    assert_eq!(count(&runs_fresh, "reviewer"), 1);
    assert_eq!(count(&runs_fresh, "author_rev"), 1);
}

/// Collect `step_id → PartialResult.summary` for every completed step of
/// a job, so two runs can be compared for identical final state.
fn summaries(store: &DurableStore, job: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for step in ["author", "reviewer", "author_rev"] {
        if let Some(a) = store.answer(job, step).unwrap() {
            if let AgentMessage::PartialResult { summary, .. } = &a.message {
                out.insert(step.to_string(), summary.clone());
            }
        }
    }
    out
}

// Small helper used by the M0 reopen test to read a persisted summary.
trait AuthorSummary {
    fn summary_is_author(&self) -> String;
}
impl AuthorSummary for StepAnswer {
    fn summary_is_author(&self) -> String {
        match &self.message {
            AgentMessage::PartialResult { summary, .. } => summary.clone(),
            _ => String::new(),
        }
    }
}
