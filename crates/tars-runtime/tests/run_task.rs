//! End-to-end test for [`run_task`] — the multi-step Orchestrator →
//! Worker → Critic loop. Each test feeds a queue of canned LLM
//! responses (one per agent call) and asserts on the typed
//! [`TaskOutcome`] / [`RunTaskError`] plus the trajectory event log.
//!
//! No live LLM. The queueing mock plays back the exact JSON shape a
//! real model would emit when handed each agent's strict JSON schema.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::stream;

use tars_pipeline::{LlmService, Pipeline, ProviderService};
use tars_provider::{LlmEventStream, LlmProvider};
use tars_runtime::{
    run_task, AgentEvent, CriticAgent, LocalRuntime, OrchestratorAgent, Runtime, RunTaskConfig,
    RunTaskError, VerdictKind, WorkerAgent,
};
use tars_storage::{EventStore, SqliteEventStore};
use tars_types::{
    AgentId, Capabilities, ChatEvent, ChatRequest, Pricing, ProviderError, ProviderId,
    RequestContext, StopReason, Usage,
};
use tokio_util::sync::CancellationToken;

// ── Queueing test provider ─────────────────────────────────────────────
//
// The default MockProvider only holds one canned response; run_task
// fires off many LLM calls (Orchestrator + 3 per step) and each needs
// a different shape (Plan vs WorkerResult vs Verdict JSON). This local
// helper pops the next text off a FIFO per call and emits the
// standard 3-event stream. Errors loudly when the queue runs dry so
// "test fed the wrong number of responses" surfaces as a clear
// failure, not a hang.

struct QueuedProvider {
    id: ProviderId,
    capabilities: Capabilities,
    queue: Mutex<std::collections::VecDeque<String>>,
    history: Mutex<Vec<ChatRequest>>,
}

impl QueuedProvider {
    fn new(responses: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            id: ProviderId::new("queued_mock"),
            capabilities: Capabilities::text_only_baseline(Pricing::default()),
            queue: Mutex::new(responses.into_iter().collect()),
            history: Mutex::new(Vec::new()),
        })
    }

    fn history(&self) -> Vec<ChatRequest> {
        self.history.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for QueuedProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        self.history.lock().unwrap().push(req.clone());
        let next = self.queue.lock().unwrap().pop_front().ok_or_else(|| {
            ProviderError::Internal(
                "QueuedProvider: response queue empty (test fed too few responses)".into(),
            )
        })?;
        let model = req.model.label();
        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(model)),
            Ok(ChatEvent::Delta { text: next.clone() }),
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: next.len() as u64 / 4,
                    ..Default::default()
                },
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

fn build_llm(provider: Arc<QueuedProvider>) -> Arc<dyn LlmService> {
    let inner: Arc<dyn LlmService> = ProviderService::new(provider);
    Arc::new(Pipeline::builder_with_inner(inner).build())
}

async fn fresh_runtime() -> (Arc<LocalRuntime>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn EventStore> = SqliteEventStore::open(
        tars_storage::SqliteEventStoreConfig::new(dir.path().join("events.sqlite")),
    )
    .unwrap();
    (LocalRuntime::new(store), dir)
}

fn agents() -> (Arc<OrchestratorAgent>, Arc<WorkerAgent>, Arc<CriticAgent>) {
    (
        OrchestratorAgent::new(AgentId::new("orch"), "gpt-4o"),
        WorkerAgent::new(AgentId::new("worker"), "gpt-4o", "summarise"),
        CriticAgent::new(AgentId::new("critic"), "gpt-4o"),
    )
}

// ── Canned responses ───────────────────────────────────────────────────

fn one_step_plan() -> String {
    r#"{
        "plan_id": "p1",
        "goal": "summarise PR #42",
        "steps": [
            {"id":"s1","worker_role":"summarise","instruction":"Produce a short summary","depends_on":[]}
        ]
    }"#
    .to_string()
}

fn two_step_plan() -> String {
    r#"{
        "plan_id": "p2",
        "goal": "review and summarise",
        "steps": [
            {"id":"s1","worker_role":"summarise","instruction":"Step one","depends_on":[]},
            {"id":"s2","worker_role":"summarise","instruction":"Step two","depends_on":["s1"]}
        ]
    }"#
    .to_string()
}

fn worker_ok(summary: &str) -> String {
    serde_json::json!({"summary": summary, "confidence": 0.8}).to_string()
}

fn approve() -> String {
    r#"{"kind":"approve","reason":"","suggestions":[]}"#.to_string()
}

fn refine(suggestion: &str) -> String {
    serde_json::json!({
        "kind": "refine",
        "reason": "",
        "suggestions": [suggestion],
    })
    .to_string()
}

fn reject(reason: &str) -> String {
    serde_json::json!({
        "kind": "reject",
        "reason": reason,
        "suggestions": [],
    })
    .to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_one_step_approves_first_attempt() {
    let provider = QueuedProvider::new(vec![
        one_step_plan(),
        worker_ok("First-cut summary."),
        approve(),
    ]);
    let llm = build_llm(provider.clone());
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let outcome = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "summarise PR #42",
        RunTaskConfig::default(),
        CancellationToken::new(),
    )
    .await
    .expect("run_task should succeed");

    assert_eq!(outcome.plan.steps.len(), 1);
    assert_eq!(outcome.steps.len(), 1);
    assert_eq!(outcome.steps[0].step_id, "s1");
    assert_eq!(outcome.steps[0].refinement_attempts, 0);
    assert!(matches!(
        &outcome.steps[0].verdict,
        tars_runtime::AgentMessage::Verdict { verdict: VerdictKind::Approve, .. },
    ));

    // Trajectory: TrajectoryStarted + 3 step lifecycles (orch, worker, critic)
    // each = StepStarted + LlmCallCaptured + StepCompleted, then
    // TrajectoryCompleted = 1 + 3*3 + 1 = 11 events.
    let events = rt.replay(&outcome.trajectory_id).await.unwrap();
    assert_eq!(events.len(), 11, "events: {events:#?}");
    assert!(matches!(events[0], AgentEvent::TrajectoryStarted { .. }));
    assert!(matches!(events.last().unwrap(), AgentEvent::TrajectoryCompleted { .. }));

    // Three distinct agents appeared as StepStarted entries, in order.
    let agent_steps: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StepStarted { agent, .. } => Some(agent.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(agent_steps, vec!["orch", "worker", "critic"]);

    // Provider saw the 3 calls in the right order (Orchestrator first).
    assert_eq!(provider.history().len(), 3);
}

#[tokio::test]
async fn refine_then_approve_records_attempt_count_and_threads_suggestions() {
    let provider = QueuedProvider::new(vec![
        one_step_plan(),
        worker_ok("Vague first try."),
        refine("mention the security fix"),
        worker_ok("Improved summary including the security fix."),
        approve(),
    ]);
    let llm = build_llm(provider.clone());
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let outcome = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "summarise PR #42",
        RunTaskConfig::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.steps.len(), 1);
    assert_eq!(
        outcome.steps[0].refinement_attempts, 1,
        "one refine before approve"
    );

    // The second worker call should have received the Critic's
    // suggestion as a `refinements` payload.
    let history = provider.history();
    assert_eq!(history.len(), 5);
    let second_worker_text = history[3].messages[0].content()[0].as_text().unwrap();
    assert!(
        second_worker_text.contains("mention the security fix"),
        "second worker prompt should include the refinement suggestion; \
         got: {second_worker_text}",
    );
}

#[tokio::test]
async fn reject_verdict_aborts_task_and_marks_trajectory_abandoned() {
    let provider = QueuedProvider::new(vec![
        one_step_plan(),
        worker_ok("Off-base summary."),
        reject("the summary missed the entire point"),
    ]);
    let llm = build_llm(provider);
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let err = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "summarise PR #42",
        RunTaskConfig::default(),
        CancellationToken::new(),
    )
    .await
    .expect_err("should reject");

    let traj = err.trajectory_id().clone();
    match err {
        RunTaskError::Rejected { step_id, reason, .. } => {
            assert_eq!(step_id, "s1");
            assert!(reason.contains("missed the entire point"));
        }
        other => panic!("expected Rejected, got {other:?}"),
    }

    let events = rt.replay(&traj).await.unwrap();
    assert!(matches!(events.last().unwrap(), AgentEvent::TrajectoryAbandoned { .. }));
}

#[tokio::test]
async fn refine_exhausted_aborts_with_attempt_count() {
    // Config caps at 1 refinement → initial attempt + 1 refine + still
    // refine = exhausted.
    let provider = QueuedProvider::new(vec![
        one_step_plan(),
        worker_ok("first"),
        refine("be better"),
        worker_ok("second"),
        refine("be even better"),
    ]);
    let llm = build_llm(provider);
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let err = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "summarise PR #42",
        RunTaskConfig { max_refinements_per_step: 1 },
        CancellationToken::new(),
    )
    .await
    .expect_err("should exhaust");

    let traj = err.trajectory_id().clone();
    match err {
        RunTaskError::RefineExhausted { step_id, attempts, .. } => {
            assert_eq!(step_id, "s1");
            // Saw: attempt 0 (refine) + attempt 1 (refine, exhausted).
            // attempts field reports total attempts incl. the exhausting one.
            assert_eq!(attempts, 2);
        }
        other => panic!("expected RefineExhausted, got {other:?}"),
    }

    let events = rt.replay(&traj).await.unwrap();
    assert!(matches!(events.last().unwrap(), AgentEvent::TrajectoryAbandoned { .. }));
}

#[tokio::test]
async fn multi_step_plan_runs_each_step_in_order() {
    let provider = QueuedProvider::new(vec![
        two_step_plan(),
        worker_ok("step one done"),
        approve(),
        worker_ok("step two done"),
        approve(),
    ]);
    let llm = build_llm(provider.clone());
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let outcome = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "do both",
        RunTaskConfig::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.steps.len(), 2);
    assert_eq!(outcome.steps[0].step_id, "s1");
    assert_eq!(outcome.steps[1].step_id, "s2");
    for s in &outcome.steps {
        assert_eq!(s.refinement_attempts, 0);
    }

    // 1 orch + 2 * (worker + critic) = 5 LLM calls.
    assert_eq!(provider.history().len(), 5);

    // Trajectory: 1 + 5*3 + 1 = 17 events.
    let events = rt.replay(&outcome.trajectory_id).await.unwrap();
    assert_eq!(events.len(), 17);
}

#[tokio::test]
async fn malformed_plan_surfaces_as_orchestrator_error_and_abandons_trajectory() {
    let provider = QueuedProvider::new(vec!["definitely not JSON".to_string()]);
    let llm = build_llm(provider);
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let err = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "x",
        RunTaskConfig::default(),
        CancellationToken::new(),
    )
    .await
    .expect_err("should fail at planner");

    let traj = err.trajectory_id().clone();
    assert!(matches!(err, RunTaskError::Orchestrator { .. }));
    let events = rt.replay(&traj).await.unwrap();
    assert!(matches!(events.last().unwrap(), AgentEvent::TrajectoryAbandoned { .. }));
}

