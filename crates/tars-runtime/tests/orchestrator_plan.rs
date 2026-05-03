//! End-to-end test for OrchestratorAgent.
//!
//! Drives the full stack: pipeline → Mock provider returning canned
//! JSON → OrchestratorAgent::plan() → typed Plan parse + validation.
//! No live LLM needed; the mock plays back exactly what a real model
//! would emit when given the planner system prompt + the strict
//! Plan JSON schema.

use std::sync::Arc;

use tars_pipeline::{LlmService, Pipeline, ProviderService};
use tars_provider::backends::mock::{CannedResponse, MockProvider};
use tars_runtime::{
    execute_agent_step, AgentContext, AgentEvent, AgentOutput, LocalRuntime, OrchestratorAgent,
    OrchestratorError, Runtime,
};
use tars_storage::{EventStore, SqliteEventStore};
use tars_types::AgentId;
use tokio_util::sync::CancellationToken;

fn build_llm(canned_json: &str) -> Arc<dyn LlmService> {
    let mock = MockProvider::new("mock_planner", CannedResponse::text(canned_json.to_string()));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    Arc::new(Pipeline::builder_with_inner(inner).build())
}

fn ctx(llm: Arc<dyn LlmService>) -> AgentContext {
    AgentContext {
        trajectory_id: tars_types::TrajectoryId::new("orch_test_traj"),
        step_seq: 1,
        llm,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn happy_path_parses_canned_plan() {
    // What a real planner would emit when handed the Plan schema.
    let canned = r#"{
        "plan_id": "plan-summarise-pr-42",
        "goal": "summarise PR #42 for a non-engineer",
        "steps": [
            {
                "id": "s1",
                "worker_role": "search",
                "instruction": "fetch the diff for PR #42 from GitHub",
                "depends_on": []
            },
            {
                "id": "s2",
                "worker_role": "summarise",
                "instruction": "produce a 3-sentence summary of the diff",
                "depends_on": ["s1"]
            }
        ]
    }"#;
    let llm = build_llm(canned);
    let agent = OrchestratorAgent::new(AgentId::new("orch"), "gpt-4o");

    let plan = agent
        .plan(ctx(llm), "summarise PR #42 for a non-engineer")
        .await
        .expect("plan should parse");

    assert_eq!(plan.plan_id, "plan-summarise-pr-42");
    assert_eq!(plan.steps.len(), 2);
    assert_eq!(plan.steps[0].worker_role, "search");
    assert!(plan.steps[0].depends_on.is_empty());
    assert_eq!(plan.steps[1].depends_on, vec!["s1".to_string()]);
}

#[tokio::test]
async fn malformed_json_surfaces_decode_error() {
    let llm = build_llm("this is definitely not JSON");
    let agent = OrchestratorAgent::new(AgentId::new("orch"), "gpt-4o");

    let err = agent
        .plan(ctx(llm), "do anything")
        .await
        .expect_err("should fail to parse");
    match err {
        OrchestratorError::Decode(_) => {}
        other => panic!("expected Decode, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_dependency_graph_surfaces_invalid_plan() {
    // Valid JSON shape but the dep graph is broken: s2 depends on
    // s99 which doesn't exist.
    let canned = r#"{
        "plan_id": "p1",
        "goal": "x",
        "steps": [
            {"id": "s1", "worker_role": "w", "instruction": "do x", "depends_on": []},
            {"id": "s2", "worker_role": "w", "instruction": "do y", "depends_on": ["s99"]}
        ]
    }"#;
    let llm = build_llm(canned);
    let agent = OrchestratorAgent::new(AgentId::new("orch"), "gpt-4o");

    let err = agent.plan(ctx(llm), "x").await.expect_err("should reject");
    match err {
        OrchestratorError::InvalidPlan(msg) => {
            assert!(msg.contains("s99"));
        }
        other => panic!("expected InvalidPlan, got {other:?}"),
    }
}

#[tokio::test]
async fn orchestrator_step_logs_in_trajectory_via_execute_agent_step() {
    // Prove OrchestratorAgent integrates with the trajectory layer
    // — running it through execute_agent_step writes the standard
    // StepStarted / LlmCallCaptured / StepCompleted lifecycle.
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn EventStore> = SqliteEventStore::open(
        tars_storage::SqliteEventStoreConfig::new(dir.path().join("events.sqlite")),
    )
    .unwrap();
    let runtime = LocalRuntime::new(store);

    let canned = r#"{"plan_id":"p","goal":"x","steps":[]}"#;
    let llm = build_llm(canned);
    let agent: Arc<dyn tars_runtime::Agent> =
        OrchestratorAgent::new(AgentId::new("orch"), "gpt-4o");

    let traj = runtime.create_trajectory(None, "orch-test").await.unwrap();
    // Build a planner request manually; we're testing the
    // execute_agent_step ↔ Agent integration, not the typed
    // plan() helper here.
    let req = tars_types::ChatRequest::user(
        tars_types::ModelHint::Explicit("gpt-4o".into()),
        "any goal",
    );

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

    // Output is the canned JSON text.
    match result.output {
        AgentOutput::Text { text } => assert!(text.contains("plan_id")),
        other => panic!("expected Text output, got {other:?}"),
    }

    // Trajectory log captures the standard 4 events (Started + 3
    // step events). orchestrator agent name appears on StepStarted.
    let events = runtime.replay(&traj).await.unwrap();
    assert_eq!(events.len(), 4);
    if let AgentEvent::StepStarted { agent, .. } = &events[1] {
        assert_eq!(agent, "orch");
    } else {
        panic!("expected StepStarted at events[1], got {:?}", events[1]);
    }
}
