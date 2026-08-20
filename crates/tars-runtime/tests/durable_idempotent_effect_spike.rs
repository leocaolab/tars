//! SPIKE (disposable evidence — brainstorm §4-C2 / task #42 hard bone #2): the
//! ONE hard bone neither model (replay OR world-snapshot) can dodge — an
//! irreversible external write (`github.post_comment`) must be EXACTLY-ONCE
//! across crash/resume, even though the step is at-least-once.
//!
//! Truth: exactly-once EXECUTION is impossible. The fix is at-least-once step +
//! an idempotency KEY the external system dedupes on. The worker may run twice;
//! the side effect happens once.
//!
//! Model: a `post_comment` step keyed by its STABLE step id (the same role
//! `StepIdempotencyKey` plays in production). A mock GitHub dedupes by key and
//! counts REAL posts. Crash AFTER the post but BEFORE the step's answer commits
//! (the exact double-execution window); reopen; resume re-runs the step → posts
//! again with the same key → dedup → real posts stays 1.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tars_runtime::{
    AgentMessage, AnswerStore, DurableScheduler, LocalRuntime, Plan, PlanStep, Runtime,
    StepCondition, Worker, WorkerContext, WorkerError, WorkerOutput, WorkerRegistry,
};
use tars_storage::SqliteAgentEventLog;
use tars_types::{AgentId, Usage};

/// The external system (GitHub). Dedupes by idempotency key: a key it has
/// already seen returns the cached result WITHOUT a second real side effect.
#[derive(Clone, Default)]
struct MockGithub {
    inner: Arc<Mutex<GhState>>,
}

#[derive(Default)]
struct GhState {
    seen: HashMap<String, String>,
    real_posts: usize,
}

impl MockGithub {
    /// The idempotent write. `key` = a stable idempotency key (the step id).
    fn post_comment(&self, key: &str, _body: &str) -> String {
        let mut g = self.inner.lock().unwrap();
        if let Some(cached) = g.seen.get(key) {
            return cached.clone(); // already posted under this key → NO real side effect
        }
        g.real_posts += 1;
        let result = format!("comment-url-{}", g.real_posts);
        g.seen.insert(key.to_string(), result.clone());
        result
    }
    fn real_posts(&self) -> usize {
        self.inner.lock().unwrap().real_posts
    }
}

/// A worker that performs the irreversible external write, then (optionally,
/// once) crashes BEFORE returning Ok — i.e. after the post but before the
/// step's answer is committed: the exact at-least-once window.
struct PostWorker {
    gh: MockGithub,
    invocations: Arc<Mutex<usize>>,
    crash_after_post: Arc<AtomicBool>,
}

#[async_trait]
impl Worker for PostWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        _ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        *self.invocations.lock().unwrap() += 1;

        // Two-phase in spirit: the KEY is the stable step id (what
        // `StepIdempotencyKey` derives in production). The external system
        // dedupes on it, so a re-run is a no-op side-effect-wise.
        let key = step.id.clone();
        let _url = self.gh.post_comment(&key, &format!("body for {}", step.id));

        // Crash exactly once, AFTER the post — the answer never commits, so the
        // step is un-done and WILL re-run on resume.
        if self.crash_after_post.swap(false, Ordering::SeqCst) {
            return Err(WorkerError::InvalidResult("crash after post, before commit".into()));
        }

        Ok(WorkerOutput {
            message: AgentMessage::PartialResult {
                from_agent: AgentId::new("worker:post"),
                step_id: Some(step.id.clone()),
                summary: format!("posted {}", step.id),
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

fn post_plan() -> Plan {
    Plan {
        plan_id: "hitl".into(),
        goal: "post a comment, exactly once".into(),
        steps: vec![PlanStep {
            id: "post_comment".into(),
            worker_role: "post".into(),
            instruction: "post the review comment".into(),
            depends_on: vec![],
            condition: StepCondition::Always,
        }],
    }
}

#[tokio::test]
async fn irreversible_external_write_is_exactly_once_across_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("effect.sqlite");
    let gh = MockGithub::default();
    let invocations = Arc::new(Mutex::new(0usize));

    // Phase 1 — post succeeds, then the worker crashes before the answer commits.
    {
        let store = AnswerStore::open(&path).unwrap();
        store.create_job("job", &post_plan()).unwrap();
        let worker = Arc::new(PostWorker {
            gh: gh.clone(),
            invocations: invocations.clone(),
            crash_after_post: Arc::new(AtomicBool::new(true)),
        });
        let sched = DurableScheduler::new(store.clone(), registry(worker), rt());
        assert!(sched.run_job("job").await.is_err(), "crash-after-post surfaces");
        // The post DID happen once; but its answer did NOT commit.
        assert_eq!(gh.real_posts(), 1, "the external write happened once (pre-crash)");
        assert!(store.answer("job", "post_comment").unwrap().is_none(), "step un-done");
    }

    // Phase 2 — reopen, resume with a non-crashing worker: the step re-runs.
    {
        let store = AnswerStore::open(&path).unwrap();
        let worker = Arc::new(PostWorker {
            gh: gh.clone(),
            invocations: invocations.clone(),
            crash_after_post: Arc::new(AtomicBool::new(false)),
        });
        let sched = DurableScheduler::new(store.clone(), registry(worker), rt());
        sched.run_job("job").await.expect("resume converges");
        assert_eq!(store.job_status("job").unwrap().as_deref(), Some("done"));
    }

    // THE POINT: the worker ran TWICE (at-least-once), but the irreversible
    // external write happened EXACTLY ONCE — the idempotency key deduped the
    // resume's re-post. No double comment.
    assert_eq!(*invocations.lock().unwrap(), 2, "step is at-least-once (ran on both passes)");
    assert_eq!(gh.real_posts(), 1, "external write is EXACTLY-ONCE across crash/resume");
}

fn registry(worker: Arc<PostWorker>) -> WorkerRegistry {
    let mut reg = WorkerRegistry::new();
    reg.register("post", worker);
    reg
}
