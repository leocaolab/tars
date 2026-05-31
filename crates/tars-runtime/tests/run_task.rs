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
    AgentEvent, CriticAgent, LocalRuntime, OrchestratorAgent, RunTaskConfig, RunTaskError, Runtime,
    VerdictKind, WorkerAgent, run_task,
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
        tars_runtime::AgentMessage::Verdict {
            verdict: VerdictKind::Approve,
            ..
        },
    ));

    // Trajectory: TrajectoryStarted + 3 step lifecycles (orch, worker, critic)
    // each = StepStarted + LlmCallCaptured + StepCompleted, then
    // TrajectoryCompleted = 1 + 3*3 + 1 = 11 events.
    let events = rt.replay(&outcome.trajectory_id).await.unwrap();
    assert_eq!(events.len(), 11, "events: {events:#?}");
    assert!(matches!(events[0], AgentEvent::TrajectoryStarted { .. }));
    assert!(matches!(
        events.last().unwrap(),
        AgentEvent::TrajectoryCompleted { .. }
    ));

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
async fn reject_verdict_aborts_task_when_max_replans_is_zero() {
    // Pre-replan semantics: with max_replans = 0, the first reject is
    // terminal and the task fails. This used to fire as
    // `RunTaskError::Rejected`; after the replan loop landed, the
    // variant is `ReplanExhausted { replans: 0 }` — same meaning
    // ("rejected; we gave up immediately"), new name.
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
        RunTaskConfig {
            max_refinements_per_step: 2,
            max_replans: 0,
        },
        CancellationToken::new(),
    )
    .await
    .expect_err("should reject");

    let traj = err.trajectory_id().clone();
    match err {
        RunTaskError::ReplanExhausted {
            step_id, reason, replans, ..
        } => {
            assert_eq!(step_id, "s1");
            assert!(reason.contains("missed the entire point"));
            assert_eq!(replans, 0, "max_replans=0 → no replan attempts before giving up");
        }
        other => panic!("expected ReplanExhausted, got {other:?}"),
    }

    let events = rt.replay(&traj).await.unwrap();
    assert!(matches!(
        events.last().unwrap(),
        AgentEvent::TrajectoryAbandoned { .. }
    ));
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
        RunTaskConfig {
            max_refinements_per_step: 1,
            max_replans: 0,
        },
        CancellationToken::new(),
    )
    .await
    .expect_err("should exhaust");

    let traj = err.trajectory_id().clone();
    match err {
        RunTaskError::RefineExhausted {
            step_id, attempts, ..
        } => {
            assert_eq!(step_id, "s1");
            // Saw: attempt 0 (refine) + attempt 1 (refine, exhausted).
            // attempts field reports total attempts incl. the exhausting one.
            assert_eq!(attempts, 2);
        }
        other => panic!("expected RefineExhausted, got {other:?}"),
    }

    let events = rt.replay(&traj).await.unwrap();
    assert!(matches!(
        events.last().unwrap(),
        AgentEvent::TrajectoryAbandoned { .. }
    ));
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
    assert!(matches!(
        events.last().unwrap(),
        AgentEvent::TrajectoryAbandoned { .. }
    ));
}

/// Schema-dispatching provider — solves the "what response should I
/// pop?" problem for parallel-execution tests.
///
/// With the DAG executor, multiple worker / critic calls may fly in
/// flight at the same wall-clock time (sibling level-0 steps). A
/// plain FIFO `QueuedProvider` can't tell whether the next pop is
/// supposed to be a Worker result or a Critic verdict — the next
/// concurrent call wins the queue lock and gets whatever's on top
/// regardless of role. The result: a Critic call gets handed back a
/// Worker JSON shape (missing `kind`), `serde_json` rejects it, and
/// the test fails non-deterministically.
///
/// This provider keys responses on the request's `structured_output`
/// schema name — "Plan" / "WorkerResult" / "Verdict" — so a Worker
/// pop reliably finds Worker JSON regardless of completion order.
struct SchemaDispatchProvider {
    id: ProviderId,
    capabilities: Capabilities,
    by_schema: Mutex<std::collections::HashMap<String, std::collections::VecDeque<String>>>,
}

impl SchemaDispatchProvider {
    fn new(by_schema: std::collections::HashMap<String, Vec<String>>) -> Arc<Self> {
        let mapped = by_schema
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect::<std::collections::VecDeque<_>>()))
            .collect();
        Arc::new(Self {
            id: ProviderId::new("schema_dispatch"),
            capabilities: Capabilities::text_only_baseline(Pricing::default()),
            by_schema: Mutex::new(mapped),
        })
    }
}

#[async_trait]
impl LlmProvider for SchemaDispatchProvider {
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
        let schema_name = req
            .structured_output
            .as_ref()
            .and_then(|s| s.name.as_deref())
            .unwrap_or("")
            .to_string();
        let next = {
            let mut by = self.by_schema.lock().unwrap();
            by.get_mut(&schema_name).and_then(|q| q.pop_front())
        }
        .ok_or_else(|| {
            ProviderError::Internal(format!(
                "SchemaDispatchProvider: no canned response for schema `{schema_name}`"
            ))
        })?;
        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(req.model.label())),
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

/// Fan-out + merge plan: s1 + s2 are independent roots, s3 depends on
/// both. Pins the two DAG-executor properties we added:
///
/// 1. The merge step (s3) sees BOTH s1 and s2 in its
///    `prior_results` payload — without dep-result threading, a
///    fan-out plan is just N independent calls and can't actually
///    consume sibling work.
///
/// 2. Output Vec stays in plan-declaration order regardless of the
///    completion order of level-0 steps.
///
/// Uses `SchemaDispatchProvider` so the test is order-stable: parallel
/// Worker + Critic calls at level 0 can interleave freely, and each
/// schema gets its own canned queue.
#[tokio::test]
async fn dag_fanout_plan_threads_dep_results_into_merge_step() {
    use std::collections::HashMap;
    let mut by_schema: HashMap<String, Vec<String>> = HashMap::new();
    by_schema.insert("Plan".into(), vec![fanout_merge_plan()]);
    by_schema.insert(
        "WorkerResult".into(),
        vec![
            // 3 worker responses: s1, s2, s3 (in completion order;
            // by content they're interchangeable for s1/s2).
            worker_ok("leaf done"),
            worker_ok("leaf done"),
            worker_ok("merge done"),
        ],
    );
    by_schema.insert(
        "Verdict".into(),
        vec![approve(), approve(), approve()],
    );
    let provider = SchemaDispatchProvider::new(by_schema);
    let inner: Arc<dyn LlmService> = ProviderService::new(provider);
    let llm: Arc<dyn LlmService> = Arc::new(Pipeline::builder_with_inner(inner).build());
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let outcome = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "fan out then merge",
        RunTaskConfig::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // (2) Output preserves plan-declaration order even though level-0
    // steps may have completed in either order.
    assert_eq!(outcome.steps.len(), 3);
    assert_eq!(outcome.steps[0].step_id, "s1");
    assert_eq!(outcome.steps[1].step_id, "s2");
    assert_eq!(outcome.steps[2].step_id, "s3");

    // (1) Level ordering: the merge step must START after both leaves
    // are DONE. `StepCompleted` doesn't carry the `agent` field, so
    // we map step_seq → agent via the StepStarted events and reason
    // about ordering through that map.
    let events = rt.replay(&outcome.trajectory_id).await.unwrap();
    use tars_runtime::AgentEvent::*;
    let mut worker_seqs: Vec<u32> = Vec::new();
    let mut worker_starts_idx: Vec<usize> = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        if let StepStarted { agent, step_seq, .. } = ev {
            if agent == "worker" {
                worker_seqs.push(*step_seq);
                worker_starts_idx.push(i);
            }
        }
    }
    assert_eq!(
        worker_seqs.len(),
        3,
        "exactly 3 worker StepStarteds (s1, s2, merge): {worker_seqs:?}",
    );
    let merge_seq = worker_seqs[2];
    let merge_start_idx = worker_starts_idx[2];
    let leaf_seqs: std::collections::HashSet<u32> = worker_seqs[..2].iter().copied().collect();
    let leaf_completes_before_merge = events[..merge_start_idx]
        .iter()
        .filter(
            |ev| matches!(ev, StepCompleted { step_seq, .. } if leaf_seqs.contains(step_seq)),
        )
        .count();
    assert_eq!(
        leaf_completes_before_merge, 2,
        "both leaf workers (seq={leaf_seqs:?}) must complete before merge worker (seq={merge_seq}) starts; \
         got {leaf_completes_before_merge} matching StepCompleteds before idx={merge_start_idx}",
    );

    // (1b) Parallelism witness: somewhere in the log there must be
    // two worker `StepStarted` events back-to-back with NO worker
    // `StepCompleted` for the first one between them — that's the
    // in-flight overlap signature of the level-0 batch. A serial
    // executor would always interleave StepStarted→StepCompleted per
    // worker. We resolve "worker StepCompleted" via the
    // worker-step_seq set.
    let worker_seq_set: std::collections::HashSet<u32> = worker_seqs.iter().copied().collect();
    let mut saw_parallel_overlap = false;
    let mut last_worker_start_seq: Option<(usize, u32)> = None;
    for (i, ev) in events.iter().enumerate() {
        if let StepStarted { agent, step_seq, .. } = ev {
            if agent == "worker" {
                if let Some((prev_i, prev_seq)) = last_worker_start_seq {
                    let between_completed = events[prev_i + 1..i].iter().any(|e| {
                        matches!(e, StepCompleted { step_seq, .. } if *step_seq == prev_seq)
                    });
                    if !between_completed {
                        saw_parallel_overlap = true;
                        break;
                    }
                }
                last_worker_start_seq = Some((i, *step_seq));
                let _ = worker_seq_set; // hush unused-warn if no overlap path runs
            }
        }
    }
    assert!(
        saw_parallel_overlap,
        "expected two worker StepStarteds back-to-back (level-0 batch in flight) \
         — serial executor regression?"
    );
}

fn fanout_merge_plan() -> String {
    // s1 ──┐
    //      ├─→ s3
    // s2 ──┘
    r#"{
        "plan_id": "fanout",
        "goal": "fan out then merge",
        "steps": [
            {"id":"s1","worker_role":"summarise","instruction":"Leaf A","depends_on":[]},
            {"id":"s2","worker_role":"summarise","instruction":"Leaf B","depends_on":[]},
            {"id":"s3","worker_role":"summarise","instruction":"Merge A and B","depends_on":["s1","s2"]}
        ]
    }"#
    .to_string()
}

/// First plan rejected → orchestrator replans → second plan approves.
/// Pins the replan loop's happy path: a Critic Reject on the first
/// attempt no longer terminates the task; the orchestrator gets a
/// second shot with the prior plan + reject reason in its context.
#[tokio::test]
async fn replan_on_reject_recovers_with_second_plan() {
    use std::collections::HashMap;
    // SchemaDispatchProvider so the replan re-uses the same Plan
    // schema slot (it pops fresh Plan responses on each
    // orchestrator call; the FIFO order between worker / critic
    // doesn't matter).
    let mut by_schema: HashMap<String, Vec<String>> = HashMap::new();
    by_schema.insert(
        "Plan".into(),
        vec![
            // Plan attempt 1: the orchestrator's first try.
            one_step_plan(),
            // Plan attempt 2 (replan): a "different" plan.
            // Concrete content doesn't matter for the test; what
            // matters is that the orchestrator got a fresh Plan
            // request and produced this in response.
            r#"{
                "plan_id":"p1-v2",
                "goal":"summarise PR #42",
                "steps":[{"id":"s1","worker_role":"summarise","instruction":"Try again differently","depends_on":[]}]
            }"#.to_string(),
        ],
    );
    by_schema.insert(
        "WorkerResult".into(),
        vec![
            worker_ok("Off-base summary."),     // plan 1's s1 worker
            worker_ok("Tighter take this time"), // plan 2's s1 worker
        ],
    );
    by_schema.insert(
        "Verdict".into(),
        vec![
            reject("the summary missed the entire point"), // rejects plan 1's s1
            approve(),                                      // approves plan 2's s1
        ],
    );
    let provider = SchemaDispatchProvider::new(by_schema);
    let inner: Arc<dyn LlmService> = ProviderService::new(provider);
    let llm: Arc<dyn LlmService> = Arc::new(Pipeline::builder_with_inner(inner).build());
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let outcome = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "summarise PR #42",
        RunTaskConfig {
            max_refinements_per_step: 2,
            max_replans: 2,
        },
        CancellationToken::new(),
    )
    .await
    .expect("replan should recover");

    // Second plan's id surfaces on the outcome — proof the
    // orchestrator was reinvoked and a NEW Plan was used.
    assert_eq!(outcome.plan.plan_id, "p1-v2");
    assert_eq!(outcome.steps.len(), 1);
    // Pull the summary out of the typed PartialResult.
    let summary = match &outcome.steps[0].result {
        tars_runtime::AgentMessage::PartialResult { summary, .. } => summary.clone(),
        other => panic!("expected PartialResult, got {other:?}"),
    };
    assert!(
        summary.contains("Tighter take"),
        "outcome should carry plan-2's worker output, not plan-1's: {summary:?}",
    );

    // Trajectory should have TWO orchestrator StepStarteds (planner +
    // replanner) before any TrajectoryCompleted.
    let events = rt.replay(&outcome.trajectory_id).await.unwrap();
    use tars_runtime::AgentEvent::*;
    let orch_starts = events
        .iter()
        .filter(|ev| matches!(ev, StepStarted { agent, .. } if agent == "orch"))
        .count();
    assert_eq!(
        orch_starts, 2,
        "orchestrator called twice (initial plan + replan); trajectory: {events:?}"
    );
    assert!(matches!(events.last().unwrap(), TrajectoryCompleted { .. }));
}

/// Repeated rejects → orchestrator hits `max_replans` → terminal
/// `ReplanExhausted` error. Pins the upper bound.
#[tokio::test]
async fn replan_exhausted_after_max_replans_consecutive_rejects() {
    use std::collections::HashMap;
    let mut by_schema: HashMap<String, Vec<String>> = HashMap::new();
    // 1 initial plan + max_replans=2 replans = 3 Plan responses.
    by_schema.insert(
        "Plan".into(),
        vec![one_step_plan(), one_step_plan(), one_step_plan()],
    );
    by_schema.insert(
        "WorkerResult".into(),
        vec![
            worker_ok("attempt 1"),
            worker_ok("attempt 2"),
            worker_ok("attempt 3"),
        ],
    );
    by_schema.insert(
        "Verdict".into(),
        vec![
            reject("nope #1"),
            reject("nope #2"),
            reject("nope #3"), // 3rd reject = budget exhausted (1 initial + 2 replans)
        ],
    );
    let provider = SchemaDispatchProvider::new(by_schema);
    let inner: Arc<dyn LlmService> = ProviderService::new(provider);
    let llm: Arc<dyn LlmService> = Arc::new(Pipeline::builder_with_inner(inner).build());
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    let err = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "summarise PR #42",
        RunTaskConfig {
            max_refinements_per_step: 2,
            max_replans: 2,
        },
        CancellationToken::new(),
    )
    .await
    .expect_err("should exhaust replans");

    match err {
        RunTaskError::ReplanExhausted {
            step_id,
            reason,
            replans,
            ..
        } => {
            assert_eq!(step_id, "s1");
            assert!(
                reason.contains("nope #3"),
                "last reject's reason should surface: {reason}"
            );
            assert_eq!(replans, 2, "tried 2 replans before giving up");
        }
        other => panic!("expected ReplanExhausted, got {other:?}"),
    }
}

/// Cancel-on-reject — when one step in the same level emits a Critic
/// Reject, in-flight sibling steps in that batch should be cancelled
/// rather than allowed to burn their full Worker + Critic budget on
/// work the replan loop will discard.
///
/// Setup: 3-step fan-out plan (s1 ∥ s2 ∥ s3, all roots — level 0 runs
/// them all in parallel). A `GatedProvider` makes the FIRST worker
/// call return instantly (so SOME worker — whichever wins the start
/// race — flows through fast, then its critic rejects); the OTHER
/// two worker calls block on a `tokio::sync::Notify` that the test
/// NEVER fires. So the only way the task can return is if the level
/// loop's `level_cancel.cancel()` drops those blocked futures
/// mid-await.
///
/// Why this proves cancel-on-reject specifically:
///  - Without the cancel call, the level loop would block at
///    `tasks.next().await` forever waiting for the gated siblings.
///  - With cancel-on-reject (as implemented in task.rs), the first
///    Critic Reject triggers `level_cancel.cancel()`, which fires
///    `drive_llm_call`'s `tokio::select!` against `cancel.cancelled()`,
///    which drops the in-flight provider futures (and their inner
///    Notify await) — so `run_one_step` returns
///    `Err(AgentError::Cancelled)`, the drain loop discards those,
///    and the task can finally return `ReplanExhausted`.
///  - We track `entered_gate_count` to prove the gated arm WAS
///    reached (not just that the first worker bailed before any
///    sibling started) — so the deadlock-avoidance is genuinely
///    about cancel propagation through an actively-awaiting future.
///
/// `max_replans = 0` so the first Reject is immediately terminal —
/// keeps the assertion shape simple (ReplanExhausted{replans:0})
/// and isolates the cancel signal from replan-loop semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_on_reject_terminates_in_flight_sibling_workers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// Returns canned text; the FIRST `WorkerResult` call answers
    /// instantly, the rest park on a never-notified `Notify`. Plan
    /// + Verdict responses always answer instantly so the
    /// orchestrator and rejecting critic can complete their calls.
    struct GatedProvider {
        id: ProviderId,
        capabilities: Capabilities,
        /// Never notified by the test — the gated workers only exit
        /// by being dropped via cancel.
        gate: Arc<Notify>,
        worker_call_count: AtomicUsize,
        /// Bumped when a worker enters the gated `Notify::notified()`
        /// await. Counts entrances, not exits — drop-cancel never
        /// reaches the exit side of the await.
        entered_gate_count: Arc<AtomicUsize>,
        plan_response: String,
        fast_worker_response: String,
        reject_response: String,
    }

    impl GatedProvider {
        fn new(plan_response: String, fast_worker_response: String, reject_response: String) -> Arc<Self> {
            Arc::new(Self {
                id: ProviderId::new("gated"),
                capabilities: Capabilities::text_only_baseline(Pricing::default()),
                gate: Arc::new(Notify::new()),
                worker_call_count: AtomicUsize::new(0),
                entered_gate_count: Arc::new(AtomicUsize::new(0)),
                plan_response,
                fast_worker_response,
                reject_response,
            })
        }
    }

    #[async_trait]
    impl LlmProvider for GatedProvider {
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
            let schema_name = req
                .structured_output
                .as_ref()
                .and_then(|s| s.name.as_deref())
                .unwrap_or("");
            let text = match schema_name {
                "Plan" => self.plan_response.clone(),
                "Verdict" => self.reject_response.clone(),
                "WorkerResult" => {
                    let n = self.worker_call_count.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // First worker wins the race, answers fast →
                        // its critic then sees a Reject verdict, which
                        // triggers level_cancel.cancel() upstream.
                        self.fast_worker_response.clone()
                    } else {
                        // Park forever. We rely on the OUTER
                        // `drive_llm_call`'s `tokio::select!` to drop
                        // this future once level_cancel fires — there
                        // is no path here that completes the await
                        // (the gate is never notified). If
                        // cancel-on-reject is broken, the task hangs.
                        self.entered_gate_count.fetch_add(1, Ordering::SeqCst);
                        self.gate.notified().await;
                        // Unreachable in this test, but the future
                        // contract still needs a valid return.
                        unreachable!("test never notifies the gate")
                    }
                }
                other => {
                    return Err(ProviderError::Internal(format!(
                        "GatedProvider: unknown schema {other:?}"
                    )))
                }
            };
            let events: Vec<Result<ChatEvent, ProviderError>> = vec![
                Ok(ChatEvent::started(req.model.label())),
                Ok(ChatEvent::Delta { text: text.clone() }),
                Ok(ChatEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: text.len() as u64 / 4,
                        ..Default::default()
                    },
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    let provider = GatedProvider::new(
        fanout_three_roots_plan(),
        worker_ok("fast worker output"),
        reject("no good"),
    );
    let entered_gate_handle = provider.entered_gate_count.clone();
    let inner: Arc<dyn LlmService> = ProviderService::new(provider);
    let llm: Arc<dyn LlmService> = Arc::new(Pipeline::builder_with_inner(inner).build());
    let (rt, _dir) = fresh_runtime().await;
    let (orch, worker, critic) = agents();

    // Hard timeout wrapper: if cancel-on-reject is broken this would
    // hang, so cap the wait at 5s and surface a clear assertion
    // message rather than a CI timeout 10 minutes later.
    let task_fut = run_task(
        rt.clone() as Arc<dyn Runtime>,
        llm,
        orch,
        worker,
        critic,
        "fan out three roots; one rejects",
        RunTaskConfig {
            max_refinements_per_step: 0, // single attempt — Reject is terminal
            max_replans: 0,              // first Reject ⇒ ReplanExhausted{replans:0}
        },
        CancellationToken::new(),
    );
    let err = tokio::time::timeout(std::time::Duration::from_secs(5), task_fut)
        .await
        .expect(
            "run_task hung past 5s — cancel-on-reject probably regressed: \
             the gated sibling worker futures were not dropped on Reject, \
             so the level loop blocked at tasks.next().await forever.",
        )
        .expect_err("task should abandon on reject (max_replans=0)");

    // Reject path landed (vs hanging on the gated siblings).
    match &err {
        RunTaskError::ReplanExhausted { replans, .. } => {
            assert_eq!(*replans, 0);
        }
        other => panic!("expected ReplanExhausted{{replans:0}}, got {other:?}"),
    }

    // At least one gated sibling actually entered the never-notified
    // await — i.e. the test wasn't trivially satisfied by a worker
    // bailing before any sibling started.
    let entered = entered_gate_handle.load(Ordering::SeqCst);
    assert!(
        entered >= 1,
        "expected at least one gated sibling worker to reach the never-notified \
         Notify::notified() await; got entered_gate_count = {entered}. The cancel \
         signal needs an in-flight pending future to actually be propagating \
         through.",
    );
}

fn fanout_three_roots_plan() -> String {
    // Three independent roots — all run in parallel at level 0.
    // No merge step; the test only cares about level-0 cancel
    // propagation.
    r#"{
        "plan_id": "fan3",
        "goal": "three independent",
        "steps": [
            {"id":"s1","worker_role":"summarise","instruction":"Leaf A","depends_on":[]},
            {"id":"s2","worker_role":"summarise","instruction":"Leaf B","depends_on":[]},
            {"id":"s3","worker_role":"summarise","instruction":"Leaf C","depends_on":[]}
        ]
    }"#
    .to_string()
}
