//! Doc 23 E2E tests that exercise the unified tool layer **through
//! `Session::send`** (the TUI backend path), not just the registry:
//!
//! - **E2E-1** — the same tool runs identically via `Session` and `WorkerAgent`.
//! - **gate-through-Session** — a `Session` whose `tool_ctx` denies a tool
//!   gates it (is_error, never runs) — the actual point of the change.
//! - **E2E-4 (faithful)** — dropping the `Session::send` future mid-approval
//!   runs nothing and doesn't panic.
//!
//! No live LLM: a small queueing provider feeds one canned event stream per
//! call (tool call → final text), mirroring `worker_with_tools.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;

use tars_pipeline::{LlmService, Pipeline, ProviderService};
use tars_provider::{LlmEventStream, LlmProvider};
use tars_runtime::{AgentContext, Budget, Session, SessionOptions, WorkerAgent};
use tars_tools::{
    ApprovalDecision, ApprovalRequest, ApprovalSink, PermissionView, ToolContext, ToolDecision,
    ToolRegistry, builtins::WriteFileTool,
};
use tars_types::{
    AgentId, Capabilities, ChatEvent, ChatRequest, ModelHint, Pricing, ProviderError, ProviderId,
    RequestContext, StopReason, TrajectoryId, Usage,
};
use tokio_util::sync::CancellationToken;

// ── Provider that pops one canned ChatEvent sequence per call ─────────

struct EventQueueProvider {
    id: ProviderId,
    capabilities: Capabilities,
    queue: Mutex<std::collections::VecDeque<Vec<ChatEvent>>>,
}

impl EventQueueProvider {
    fn new(sequences: Vec<Vec<ChatEvent>>) -> Arc<Self> {
        Arc::new(Self {
            id: ProviderId::new("event_queue_mock"),
            capabilities: Capabilities::text_only_baseline(Pricing::default()),
            queue: Mutex::new(sequences.into_iter().collect()),
        })
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
        _req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let next = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Internal("queue empty".into()))?;
        let mapped: Vec<Result<ChatEvent, ProviderError>> = next.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(mapped)))
    }
}

fn build_llm(provider: Arc<EventQueueProvider>) -> Arc<dyn LlmService> {
    let inner: Arc<dyn LlmService> = ProviderService::new(provider);
    Arc::new(Pipeline::builder_with_inner(inner).build())
}

fn write_call_events(path: &str, content: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::started("any-model"),
        ChatEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "fs.write_file".into(),
        },
        ChatEvent::ToolCallEnd {
            index: 0,
            id: "call_1".into(),
            parsed_args: serde_json::json!({ "path": path, "content": content }),
        },
        ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

fn final_text_events(text: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::started("any-model"),
        ChatEvent::Delta { text: text.into() },
        ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]
}

/// WorkerAgent parses the final turn as `{summary, confidence}` JSON.
fn final_json() -> String {
    serde_json::json!({ "summary": "wrote it", "confidence": 0.9 }).to_string()
}

fn owned_registry(dir: &std::path::Path) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register_owned(WriteFileTool::with_root(dir).unwrap())
        .unwrap();
    reg
}

#[tokio::test]
async fn same_tool_runs_identically_via_session_and_worker() {
    // ── Session path ─────────────────────────────────────────────────
    let sdir = tempfile::tempdir().unwrap();
    let spath = sdir.path().join("out.txt");
    let provider = EventQueueProvider::new(vec![
        write_call_events(spath.to_str().unwrap(), "parity!"),
        final_text_events("done"),
    ]);
    let mut session = Session::new(
        build_llm(provider),
        Capabilities::text_only_baseline(Pricing::default()),
        SessionOptions {
            system: "you write files".into(),
            budget: Budget::Chars(usize::MAX / 2),
            tools: Some(owned_registry(sdir.path())),
            tool_ctx: tars_tools::ToolContext::default(),
            default_max_output_tokens: None,
            model: ModelHint::Explicit("any-model".into()),
        },
    );
    let session_reply = session.send_text("write the file", None).await.unwrap();
    let session_file = std::fs::read_to_string(&spath).unwrap();

    // ── Worker path ──────────────────────────────────────────────────
    let wdir = tempfile::tempdir().unwrap();
    let wpath = wdir.path().join("out.txt");
    let worker = WorkerAgent::with_tools(
        AgentId::new("worker:writer"),
        "any-model",
        "write",
        Arc::new(owned_registry(wdir.path())),
    );
    let plan = tars_runtime::Plan {
        plan_id: "p1".into(),
        goal: "write a file".into(),
        steps: vec![tars_runtime::PlanStep {
            id: "s1".into(),
            worker_role: "write".into(),
            instruction: "write out.txt".into(),
            depends_on: vec![],
            condition: tars_runtime::StepCondition::Always,
        }],
    };
    let ctx = AgentContext {
        trajectory_id: TrajectoryId::new("parity_traj"),
        step_seq: 1,
        llm: build_llm(EventQueueProvider::new(vec![
            write_call_events(wpath.to_str().unwrap(), "parity!"),
            final_text_events(&final_json()),
        ])),
        cancel: CancellationToken::new(),
        cwd: None,
        permissions: Default::default(),
    };
    worker
        .execute_step(ctx, &plan, &plan.steps[0], &[])
        .await
        .expect("worker should finish");
    let worker_file = std::fs::read_to_string(&wpath).unwrap();

    // ── Parity ───────────────────────────────────────────────────────
    assert_eq!(session_file, "parity!", "session tool wrote the file");
    assert_eq!(worker_file, "parity!", "worker tool wrote the file");
    assert_eq!(
        session_file, worker_file,
        "the same tool produced identical results under both drivers"
    );
    assert!(!session_reply.is_empty(), "session returned a final reply");
}

// ── Gate-through-Session tests (the actual point of Doc 23) ───────────

/// Build a Session over the given canned-event sequences, tools jailed to
/// `dir`, with the supplied `tool_ctx` (permission / approval / sandbox).
fn session_with(
    dir: &std::path::Path,
    events: Vec<Vec<ChatEvent>>,
    tool_ctx: ToolContext,
) -> Session {
    Session::new(
        build_llm(EventQueueProvider::new(events)),
        Capabilities::text_only_baseline(Pricing::default()),
        SessionOptions {
            system: "you write files".into(),
            budget: Budget::Chars(usize::MAX / 2),
            tools: Some(owned_registry(dir)),
            tool_ctx,
            default_max_output_tokens: None,
            model: ModelHint::Explicit("any-model".into()),
        },
    )
}

/// Approval sink that never answers — for the drop-mid-approval test.
struct PendingSink;
#[async_trait]
impl ApprovalSink for PendingSink {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalDecision {
        std::future::pending::<()>().await;
        ApprovalDecision::Allow
    }
}

/// A `Session` whose permission view denies the write tool gates it *through
/// `send`*: the call comes back is_error, the model continues, the file is
/// never written. This is what the whole change is for — a gated Session path.
#[tokio::test]
async fn session_gates_a_denied_tool() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.txt");
    let deny_writes: Arc<dyn PermissionView> = Arc::new(|name: &str| {
        if name == "fs.write_file" {
            ToolDecision::Deny
        } else {
            ToolDecision::Allow
        }
    });
    let mut session = session_with(
        dir.path(),
        vec![
            write_call_events(path.to_str().unwrap(), "should not happen"),
            final_text_events("ok, I won't"),
        ],
        ToolContext {
            permission: Some(deny_writes),
            ..Default::default()
        },
    );
    let reply = session.send_text("write the file", None).await.unwrap();
    assert!(!path.exists(), "denied tool must NOT have written the file");
    assert_eq!(reply, "ok, I won't", "model continued past the refusal");
}

/// E2E-4 (faithful): dropping the `Session::send` future while it is blocked
/// awaiting approval runs nothing and doesn't panic (TurnGuard rolls back).
#[tokio::test]
async fn dropping_session_send_during_approval_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.txt");
    let ask: Arc<dyn PermissionView> = Arc::new(|_: &str| ToolDecision::Ask);
    let mut session = session_with(
        dir.path(),
        vec![write_call_events(
            path.to_str().unwrap(),
            "should not happen",
        )],
        ToolContext {
            permission: Some(ask),
            approval: Some(Arc::new(PendingSink)),
            ..Default::default()
        },
    );
    // `send` blocks forever at the approval await; the timeout drops the
    // future mid-flight — exactly the E2E-4 scenario.
    let r =
        tokio::time::timeout(Duration::from_millis(100), session.send_text("write", None)).await;
    assert!(
        r.is_err(),
        "send must still be blocked on approval (timed out)"
    );
    assert!(
        !path.exists(),
        "dropping mid-approval must NOT have run the tool"
    );
    // Session survives the dropped turn (no panic, history rolled back).
    assert_eq!(session.turns().len(), 0, "dropped turn rolled back");
}
