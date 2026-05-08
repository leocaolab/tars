//! End-to-end test for [`WorkerAgent::with_tools`] — drives one
//! Worker step that:
//!
//! 1. asks the LLM,
//! 2. gets back a tool call (`fs.read_file`),
//! 3. dispatches the tool against a real tempfile,
//! 4. re-prompts the LLM with the tool result,
//! 5. gets back the final `{summary, confidence}` JSON,
//! 6. parses it into a typed [`AgentMessage::PartialResult`].
//!
//! No live LLM. A small queueing provider feeds one canned event
//! stream per LLM call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;

use tars_pipeline::{LlmService, Pipeline, ProviderService};
use tars_provider::{LlmEventStream, LlmProvider};
use tars_runtime::{AgentContext, AgentMessage, WorkerAgent};
use tars_tools::{ToolRegistry, builtins::ReadFileTool};
use tars_types::{
    AgentId, Capabilities, ChatEvent, ChatRequest, Pricing, ProviderError, ProviderId,
    RequestContext, StopReason, TrajectoryId, Usage,
};
use tokio_util::sync::CancellationToken;

// ── Provider that pops one canned ChatEvent sequence per call ─────────

struct EventQueueProvider {
    id: ProviderId,
    capabilities: Capabilities,
    queue: Mutex<std::collections::VecDeque<Vec<ChatEvent>>>,
    history: Mutex<Vec<ChatRequest>>,
}

impl EventQueueProvider {
    fn new(sequences: Vec<Vec<ChatEvent>>) -> Arc<Self> {
        Arc::new(Self {
            id: ProviderId::new("event_queue_mock"),
            capabilities: Capabilities::text_only_baseline(Pricing::default()),
            queue: Mutex::new(sequences.into_iter().collect()),
            history: Mutex::new(Vec::new()),
        })
    }

    fn history(&self) -> Vec<ChatRequest> {
        self.history.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for EventQueueProvider {
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
        self.history.lock().unwrap().push(req);
        let next = self.queue.lock().unwrap().pop_front().ok_or_else(|| {
            ProviderError::Internal(
                "EventQueueProvider: queue empty (test fed too few sequences)".into(),
            )
        })?;
        let mapped: Vec<Result<ChatEvent, ProviderError>> = next.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(mapped)))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

fn build_llm(provider: Arc<EventQueueProvider>) -> Arc<dyn LlmService> {
    let inner: Arc<dyn LlmService> = ProviderService::new(provider);
    Arc::new(Pipeline::builder_with_inner(inner).build())
}

fn ctx(llm: Arc<dyn LlmService>) -> AgentContext {
    AgentContext {
        trajectory_id: TrajectoryId::new("worker_tools_traj"),
        step_seq: 1,
        llm,
        cancel: CancellationToken::new(),
    }
}

fn sample_plan() -> tars_runtime::Plan {
    tars_runtime::Plan {
        plan_id: "p1".into(),
        goal: "summarise the contents of hello.txt".into(),
        steps: vec![tars_runtime::PlanStep {
            id: "s1".into(),
            worker_role: "summarise".into(),
            instruction: "read hello.txt and summarise it".into(),
            depends_on: vec![],
        }],
    }
}

/// Build the canned events that simulate the model emitting a single
/// tool call and then waiting for the result.
fn tool_call_events(call_id: &str, tool_name: &str, args: serde_json::Value) -> Vec<ChatEvent> {
    vec![
        ChatEvent::started("any-model"),
        ChatEvent::ToolCallStart {
            index: 0,
            id: call_id.to_string(),
            name: tool_name.to_string(),
        },
        ChatEvent::ToolCallEnd {
            index: 0,
            id: call_id.to_string(),
            parsed_args: args,
        },
        ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 50,
                output_tokens: 5,
                ..Default::default()
            },
        },
    ]
}

/// Build the canned events that simulate the final text-only answer.
fn final_text_events(text: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::started("any-model"),
        ChatEvent::Delta {
            text: text.to_string(),
        },
        ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 30,
                output_tokens: 12,
                ..Default::default()
            },
        },
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn worker_dispatches_tool_call_and_returns_final_partial_result() {
    // Tempfile the LLM will ask the tool to read.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.txt");
    tokio::fs::write(&path, b"hello, world").await.unwrap();

    // Registry with the one tool, jailed to the tempdir.
    let mut reg = ToolRegistry::new();
    reg.register_owned(ReadFileTool::with_root(dir.path()).unwrap())
        .unwrap();
    let registry = Arc::new(reg);

    // 2-call sequence: tool call → final JSON answer.
    let final_json = serde_json::json!({
        "summary": "The file contains: hello, world.",
        "confidence": 0.9,
    })
    .to_string();
    let provider = EventQueueProvider::new(vec![
        tool_call_events(
            "call_1",
            "fs.read_file",
            serde_json::json!({"path": path.to_str().unwrap()}),
        ),
        final_text_events(&final_json),
    ]);
    let llm = build_llm(provider.clone());

    let worker = WorkerAgent::with_tools(
        AgentId::new("worker:summarise"),
        "any-model",
        "summarise",
        registry,
    );

    let plan = sample_plan();
    let msg = worker
        .execute_step(ctx(llm), &plan, &plan.steps[0], &[])
        .await
        .expect("worker should finish");

    match msg {
        AgentMessage::PartialResult {
            from_agent,
            step_id,
            summary,
            confidence,
        } => {
            assert_eq!(from_agent.as_ref(), "worker:summarise");
            assert_eq!(step_id.as_deref(), Some("s1"));
            assert!(
                summary.contains("hello, world"),
                "summary should reflect the tool result; got: {summary}",
            );
            assert!((confidence - 0.9).abs() < 1e-5);
        }
        other => panic!("expected PartialResult, got {other:?}"),
    }

    // Provider saw two LLM calls (the initial + one re-prompt after
    // the tool dispatch). The second call's messages should include
    // the assistant's tool-call message + the Tool result message.
    let history = provider.history();
    assert_eq!(history.len(), 2, "should be exactly 2 LLM calls");

    let second_call_messages = &history[1].messages;
    // Initial user message + assistant (tool call) + tool result =
    // at least 3 messages on the re-prompt.
    assert!(
        second_call_messages.len() >= 3,
        "re-prompt should append assistant + tool messages; got {:?}",
        second_call_messages,
    );
    let saw_tool_msg = second_call_messages.iter().any(
        |m| matches!(m, tars_types::Message::Tool { tool_call_id, .. } if tool_call_id == "call_1"),
    );
    assert!(
        saw_tool_msg,
        "re-prompt should carry the Tool result message"
    );
}

#[tokio::test]
async fn worker_surfaces_tool_specs_to_provider_on_first_call() {
    // Even before any tool is called, the provider should see
    // `req.tools` populated with the registry's specs — that's how
    // the model knows what's available.
    let mut reg = ToolRegistry::new();
    reg.register_owned(ReadFileTool::new()).unwrap();
    let registry = Arc::new(reg);

    let final_json =
        serde_json::json!({"summary": "no tool needed", "confidence": 1.0}).to_string();
    let provider = EventQueueProvider::new(vec![final_text_events(&final_json)]);
    let llm = build_llm(provider.clone());

    let worker = WorkerAgent::with_tools(AgentId::new("worker"), "any-model", "general", registry);

    let plan = sample_plan();
    let _ = worker
        .execute_step(ctx(llm), &plan, &plan.steps[0], &[])
        .await
        .unwrap();

    let history = provider.history();
    assert_eq!(history.len(), 1);
    let req = &history[0];
    assert_eq!(
        req.tools.len(),
        1,
        "first call must advertise the tool registry to the model",
    );
    assert_eq!(req.tools[0].name, "fs.read_file");
}

#[tokio::test]
async fn worker_loop_aborts_after_max_iterations() {
    // Model keeps emitting tool calls forever. We cap iterations at
    // 2 → 2 LLM calls + 2 tool dispatches, then the loop bails.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ok.txt");
    tokio::fs::write(&path, b"x").await.unwrap();

    let mut reg = ToolRegistry::new();
    reg.register_owned(ReadFileTool::with_root(dir.path()).unwrap())
        .unwrap();
    let registry = Arc::new(reg);

    let provider = EventQueueProvider::new(vec![
        tool_call_events(
            "c1",
            "fs.read_file",
            serde_json::json!({"path": path.to_str().unwrap()}),
        ),
        tool_call_events(
            "c2",
            "fs.read_file",
            serde_json::json!({"path": path.to_str().unwrap()}),
        ),
        // No third sequence — if the loop tries a third call it
        // would surface a different error (queue empty), which is
        // exactly what we DON'T want — the iteration cap should
        // fire first.
    ]);
    let llm = build_llm(provider);

    let worker = WorkerAgent::with_tools(AgentId::new("worker"), "any-model", "general", registry)
        .with_max_tool_iterations(2);

    let plan = sample_plan();
    let err = worker
        .execute_step(ctx(llm), &plan, &plan.steps[0], &[])
        .await
        .expect_err("should hit the iteration cap");
    let msg = format!("{err}");
    assert!(
        msg.contains("max_tool_iterations") || msg.contains("tool loop"),
        "error should mention the iteration cap; got: {msg}",
    );
}

#[tokio::test]
async fn stub_worker_without_tools_still_works_unchanged() {
    // Regression: enabling the tools field for the with_tools flavour
    // mustn't break the no-tools path.
    let final_json =
        serde_json::json!({"summary": "stub did stuff", "confidence": 0.5}).to_string();
    let provider = EventQueueProvider::new(vec![final_text_events(&final_json)]);
    let llm = build_llm(provider.clone());

    let worker = WorkerAgent::new(AgentId::new("worker"), "any-model", "general");
    assert!(!worker.has_tools());

    let plan = sample_plan();
    let msg = worker
        .execute_step(ctx(llm), &plan, &plan.steps[0], &[])
        .await
        .unwrap();
    assert!(matches!(msg, AgentMessage::PartialResult { .. }));

    // No tools advertised on the request.
    let history = provider.history();
    assert_eq!(history.len(), 1);
    assert!(
        history[0].tools.is_empty(),
        "stub worker must NOT advertise tools",
    );
}
