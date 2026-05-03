//! Integration test for `execute_agent_step` — proves the full
//! pipeline-cache-agent-trajectory stack works together.
//!
//! Dependency graph exercised here:
//!
//! ```text
//! tars-runtime::execute_agent_step
//!  ├── Agent::execute (via SingleShotAgent)
//!  │    └── ctx.llm: Arc<dyn LlmService>  ← from tars-pipeline
//!  │          └── ProviderService → MockProvider (tars-provider)
//!  └── runtime.append → SqliteEventStore (tars-storage on disk)
//! ```
//!
//! If any of those layers regresses on its public contract, this
//! test should catch it.

use std::sync::Arc;

use tars_pipeline::{LlmService, Pipeline, ProviderService};
use tars_provider::backends::mock::{CannedResponse, MockProvider};
use tars_runtime::{
    execute_agent_step, AgentEvent, AgentExecutionError, AgentOutput, LocalRuntime, Runtime,
    SingleShotAgent,
};
use tars_storage::{open_event_store_at_path, EventStore};
use tars_types::{AgentId, ChatRequest, ModelHint};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn full_stack_agent_step_lands_in_trajectory_log() {
    // ── Wire the stack ──────────────────────────────────────────────
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn EventStore> =
        open_event_store_at_path(&dir.path().join("events.sqlite")).unwrap();
    let runtime = LocalRuntime::new(store.clone());

    // Pipeline with just ProviderService at the leaf — keep the
    // pipeline minimal so any failure points at the agent code, not
    // a middleware quirk.
    let mock_provider = MockProvider::new("mock_for_agent", CannedResponse::text("hello"));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock_provider);
    let pipeline = Pipeline::builder_with_inner(inner).build();
    let llm: Arc<dyn LlmService> = Arc::new(pipeline);

    let agent = SingleShotAgent::new(AgentId::new("test_agent"));

    // ── Run one trajectory step ─────────────────────────────────────
    let traj = runtime.create_trajectory(None, "agent-step-test").await.unwrap();
    let req = ChatRequest::user(ModelHint::Explicit("mock".into()), "say hi");

    let result = execute_agent_step(
        runtime.as_ref(),
        &traj,
        llm,
        agent,
        req,
        CancellationToken::new(),
    )
    .await
    .expect("agent step should succeed");

    // ── Agent produced what we expect ───────────────────────────────
    match result.output {
        AgentOutput::Text { text } => assert_eq!(text, "hello"),
        other => panic!("expected Text output, got {other:?}"),
    }
    // MockProvider sets output_tokens = text.len()/4 = "hello"/4 = 1.
    assert!(result.usage.output_tokens > 0);

    // ── Trajectory log is correctly populated ───────────────────────
    let events = runtime.replay(&traj).await.unwrap();
    // TrajectoryStarted (from create) + StepStarted + LlmCallCaptured + StepCompleted = 4
    assert_eq!(events.len(), 4, "expected 4 events, got {events:#?}");
    assert!(matches!(events[0], AgentEvent::TrajectoryStarted { .. }));
    match &events[1] {
        AgentEvent::StepStarted {
            step_seq, agent, ..
        } => {
            assert_eq!(*step_seq, 1, "first step in fresh trajectory is seq=1");
            assert_eq!(agent, "test_agent");
        }
        other => panic!("expected StepStarted, got {other:?}"),
    }
    assert!(matches!(events[2], AgentEvent::LlmCallCaptured { step_seq: 1, .. }));
    match &events[3] {
        AgentEvent::StepCompleted { step_seq, output_summary, usage, .. } => {
            assert_eq!(*step_seq, 1);
            assert_eq!(output_summary, "hello");
            assert_eq!(usage.output_tokens, result.usage.output_tokens);
        }
        other => panic!("expected StepCompleted, got {other:?}"),
    }
}

#[tokio::test]
async fn agent_failure_writes_step_failed_and_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn EventStore> =
        open_event_store_at_path(&dir.path().join("events.sqlite")).unwrap();
    let runtime = LocalRuntime::new(store.clone());

    // Mock provider that errors at open time.
    let mock_provider = MockProvider::new(
        "always_fails",
        CannedResponse::Error("upstream is broken".into()),
    );
    let inner: Arc<dyn LlmService> = ProviderService::new(mock_provider);
    let pipeline = Pipeline::builder_with_inner(inner).build();
    let llm: Arc<dyn LlmService> = Arc::new(pipeline);

    let agent = SingleShotAgent::new(AgentId::new("doomed_agent"));
    let traj = runtime.create_trajectory(None, "failure-test").await.unwrap();

    let err = execute_agent_step(
        runtime.as_ref(),
        &traj,
        llm,
        agent,
        ChatRequest::user(ModelHint::Explicit("mock".into()), "x"),
        CancellationToken::new(),
    )
    .await
    .expect_err("agent step should fail");
    assert!(
        matches!(err, AgentExecutionError::Agent(_)),
        "expected Agent error, got {err:?}",
    );

    // Trajectory log should have: Started + StepStarted + StepFailed (NO LlmCallCaptured)
    let events = runtime.replay(&traj).await.unwrap();
    assert_eq!(events.len(), 3, "expected 3 events, got {events:#?}");
    assert!(matches!(events[0], AgentEvent::TrajectoryStarted { .. }));
    assert!(matches!(events[1], AgentEvent::StepStarted { .. }));
    match &events[2] {
        AgentEvent::StepFailed { error, classification, .. } => {
            assert!(error.contains("upstream is broken"));
            // MockProvider's CannedResponse::Error becomes
            // ProviderError::Internal → MaybeRetriable.
            assert_eq!(classification, "maybe_retriable");
        }
        other => panic!("expected StepFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn step_seq_increments_across_multiple_agent_calls() {
    // Two agent steps in the same trajectory should see seq 1 then 2.
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn EventStore> =
        open_event_store_at_path(&dir.path().join("events.sqlite")).unwrap();
    let runtime = LocalRuntime::new(store.clone());

    let traj = runtime.create_trajectory(None, "multi-step").await.unwrap();
    let agent = SingleShotAgent::new(AgentId::new("a"));

    for i in 1..=3 {
        let mock_provider =
            MockProvider::new("p", CannedResponse::text(format!("turn {i}")));
        let inner: Arc<dyn LlmService> = ProviderService::new(mock_provider);
        let pipeline = Pipeline::builder_with_inner(inner).build();
        let llm: Arc<dyn LlmService> = Arc::new(pipeline);

        execute_agent_step(
            runtime.as_ref(),
            &traj,
            llm,
            agent.clone(),
            ChatRequest::user(ModelHint::Explicit("m".into()), format!("turn {i}")),
            CancellationToken::new(),
        )
        .await
        .expect("step should succeed");
    }

    let events = runtime.replay(&traj).await.unwrap();
    // 1 (Started) + 3 × 3 (StepStarted, LlmCallCaptured, StepCompleted) = 10
    assert_eq!(events.len(), 10);
    let step_starts: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::StepStarted { .. }))
        .collect();
    assert_eq!(step_starts.len(), 3);
    for (i, ev) in step_starts.iter().enumerate() {
        if let AgentEvent::StepStarted { step_seq, .. } = ev {
            assert_eq!(
                *step_seq,
                (i + 1) as u32,
                "step_seq should be 1, 2, 3 in order",
            );
        }
    }
}
