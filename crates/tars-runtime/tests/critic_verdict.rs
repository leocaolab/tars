//! End-to-end test for [`CriticAgent`].
//!
//! Drives the full stack: Pipeline → Mock provider returning canned
//! verdict JSON → CriticAgent::critique() → typed
//! [`AgentMessage::Verdict`] envelope. No live LLM; the mock plays
//! back exactly the JSON shape a real model would emit when given the
//! critique system prompt + the strict Verdict JSON schema.

use std::sync::Arc;

use tars_pipeline::{LlmService, Pipeline, ProviderService};
use tars_provider::backends::mock::{CannedResponse, MockProvider};
use tars_runtime::{
    AgentContext, AgentMessage, CriticAgent, CriticError, PartialResultRef, Plan, PlanStep,
    VerdictKind,
};
use tars_types::{AgentId, TrajectoryId};
use tokio_util::sync::CancellationToken;

fn build_llm(canned_json: &str) -> Arc<dyn LlmService> {
    let mock = MockProvider::new("mock_critic", CannedResponse::text(canned_json.to_string()));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    Arc::new(Pipeline::builder_with_inner(inner).build())
}

fn ctx(llm: Arc<dyn LlmService>) -> AgentContext {
    AgentContext {
        trajectory_id: TrajectoryId::new("critic_test_traj"),
        step_seq: 1,
        llm,
        cancel: CancellationToken::new(),
    }
}

fn sample_plan() -> Plan {
    Plan {
        plan_id: "p1".into(),
        goal: "summarise PR #42".into(),
        steps: vec![PlanStep {
            id: "s1".into(),
            worker_role: "summarise".into(),
            instruction: "do it".into(),
            depends_on: vec![],
        }],
    }
}

fn sample_partial_result() -> AgentMessage {
    AgentMessage::PartialResult {
        from_agent: AgentId::new("worker:summarise"),
        step_id: Some("s1".into()),
        summary: "It changed thing X.".into(),
        confidence: 0.6,
    }
}

#[tokio::test]
async fn happy_path_approve_yields_typed_verdict() {
    let canned = r#"{"kind":"approve","reason":"","suggestions":[]}"#;
    let llm = build_llm(canned);
    let critic = CriticAgent::new(AgentId::new("critic_a"), "gpt-4o");
    let plan = sample_plan();
    let result_msg = sample_partial_result();
    let result_ref = PartialResultRef::from_message(&result_msg).unwrap();

    let envelope = critic
        .critique(ctx(llm), &plan, &result_ref, "summarise PR #42")
        .await
        .expect("critique should succeed");

    match envelope {
        AgentMessage::Verdict {
            from_agent,
            target_step_id,
            verdict,
        } => {
            assert_eq!(from_agent.as_ref(), "critic_a");
            assert_eq!(target_step_id.as_deref(), Some("s1"));
            assert!(matches!(verdict, VerdictKind::Approve));
        }
        other => panic!("expected Verdict envelope, got {other:?}"),
    }
}

#[tokio::test]
async fn reject_path_carries_reason_into_verdict() {
    let canned = r#"{"kind":"reject","reason":"summary missed the security fix","suggestions":[]}"#;
    let llm = build_llm(canned);
    let critic = CriticAgent::new(AgentId::new("critic_a"), "gpt-4o");
    let plan = sample_plan();
    let result_msg = sample_partial_result();
    let result_ref = PartialResultRef::from_message(&result_msg).unwrap();

    let envelope = critic
        .critique(ctx(llm), &plan, &result_ref, "summarise PR #42")
        .await
        .unwrap();

    match envelope {
        AgentMessage::Verdict {
            verdict: VerdictKind::Reject { reason },
            ..
        } => {
            assert_eq!(reason, "summary missed the security fix");
        }
        other => panic!("expected Verdict::Reject, got {other:?}"),
    }
}

#[tokio::test]
async fn refine_path_carries_suggestions_into_verdict() {
    let canned = r#"{
        "kind": "refine",
        "reason": "",
        "suggestions": ["mention the security fix", "shorten to 2 sentences"]
    }"#;
    let llm = build_llm(canned);
    let critic = CriticAgent::new(AgentId::new("critic_a"), "gpt-4o");
    let plan = sample_plan();
    let result_msg = sample_partial_result();
    let result_ref = PartialResultRef::from_message(&result_msg).unwrap();

    let envelope = critic
        .critique(ctx(llm), &plan, &result_ref, "summarise PR #42")
        .await
        .unwrap();

    match envelope {
        AgentMessage::Verdict {
            verdict: VerdictKind::Refine { suggestions },
            ..
        } => {
            assert_eq!(suggestions.len(), 2);
            assert_eq!(suggestions[0], "mention the security fix");
        }
        other => panic!("expected Verdict::Refine, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_json_surfaces_decode_error() {
    let llm = build_llm("definitely not JSON");
    let critic = CriticAgent::new(AgentId::new("critic_a"), "gpt-4o");
    let plan = sample_plan();
    let result_msg = sample_partial_result();
    let result_ref = PartialResultRef::from_message(&result_msg).unwrap();

    let err = critic
        .critique(ctx(llm), &plan, &result_ref, "summarise PR #42")
        .await
        .expect_err("should fail to parse");
    match err {
        CriticError::Decode(_) => {}
        other => panic!("expected Decode, got {other:?}"),
    }
}

#[tokio::test]
async fn semantically_invalid_verdict_surfaces_invalid_verdict_error() {
    // kind=reject without a reason violates the Critic's contract.
    let canned = r#"{"kind":"reject","reason":"","suggestions":[]}"#;
    let llm = build_llm(canned);
    let critic = CriticAgent::new(AgentId::new("critic_a"), "gpt-4o");
    let plan = sample_plan();
    let result_msg = sample_partial_result();
    let result_ref = PartialResultRef::from_message(&result_msg).unwrap();

    let err = critic
        .critique(ctx(llm), &plan, &result_ref, "summarise PR #42")
        .await
        .expect_err("should reject");
    match err {
        CriticError::InvalidVerdict(msg) => {
            assert!(msg.contains("reject"));
            assert!(msg.contains("reason"));
        }
        other => panic!("expected InvalidVerdict, got {other:?}"),
    }
}

#[tokio::test]
async fn target_step_id_falls_through_from_partial_result() {
    // step_id=None on the PartialResult → target_step_id=None on Verdict.
    let canned = r#"{"kind":"approve","reason":"","suggestions":[]}"#;
    let llm = build_llm(canned);
    let critic = CriticAgent::new(AgentId::new("critic"), "gpt-4o");
    let plan = sample_plan();
    let standalone = AgentMessage::PartialResult {
        from_agent: AgentId::new("worker"),
        step_id: None, // free-standing worker output not tied to a step
        summary: "ok".into(),
        confidence: 1.0,
    };
    let result_ref = PartialResultRef::from_message(&standalone).unwrap();

    let envelope = critic
        .critique(ctx(llm), &plan, &result_ref, "x")
        .await
        .unwrap();
    match envelope {
        AgentMessage::Verdict { target_step_id, .. } => {
            assert!(target_step_id.is_none());
        }
        other => panic!("{other:?}"),
    }
}
