//! Agent abstraction (Doc 04 §4) — first-cut shape.
//!
//! ## Scope
//!
//! Ships the **trait + minimal supporting types + one concrete agent**
//! that drives an LLM call end-to-end through the pipeline. Enough
//! that a real consumer (next M3 commits' `tars chat`, future
//! orchestrator) has a contract to compose against.
//!
//! Out of scope this commit:
//! - **`AgentMessage`** typed inter-agent protocol (Doc 04 §4.2).
//!   Needed for Orchestrator → Worker handoff; no consumer exists.
//!   Today an Agent's input is just a [`ChatRequest`] and its output
//!   is the structured [`AgentOutput`] enum.
//! - **`OrchestratorAgent` / `WorkerAgent` / `CriticAgent`** defaults.
//!   Real prompt design lives in those — they get their own commits
//!   each, alongside the typed message protocol.
//! - **`ToolRegistry` field** on `AgentContext`. The `tars-tools`
//!   crate doesn't exist yet; agents can ask the LLM to emit
//!   `ToolCall` events via [`ChatRequest::tools`] (we surface them
//!   in `AgentOutput::ToolCalls`), but the actual execution loop
//!   for tool dispatch is later work.
//! - **Side-effect declarations**. Doc 04 §4.1's `declared_side_effects`
//!   slot in once the Saga compensation layer needs them.
//! - **Multi-step orchestration loop** (replanning, critic feedback,
//!   backtrack). One `Agent::execute` call = one trajectory step;
//!   composing many is the orchestrator's job.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use tars_pipeline::LlmService;
use tars_types::{
    AgentId, ChatRequest, ChatResponseBuilder, ProviderError, RequestContext, ToolCall,
    TrajectoryId, Usage,
};

/// What an agent IS, what it does, and how callers identify its kind
/// when routing inter-agent traffic. Mirrors Doc 04 §4.1's enum but
/// without the `Aggregator` variant for now (no consumer; it's a
/// pure-code agent that doesn't call LLMs, which we'll add when
/// something actually needs it).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentRole {
    /// Plans + delegates. Single-shot and multi-shot both possible.
    Orchestrator,
    /// Executes one domain task. `domain` is free-form
    /// (`"code_review"`, `"security_audit"`, …) and lets routing
    /// pick the right Worker for a Plan node.
    Worker { domain: String },
    /// Reviews someone else's output. Reads inputs + an output to
    /// critique, returns Approve/Reject/Refine.
    Critic,
}

/// Per-step environment Agent::execute receives. Deliberately small
/// today — each field has a concrete consumer right now. Doc 04
/// §4.1 lists more (budget, principal, deadline, context_store,
/// tool_registry); they slot in as their backing crates ship.
pub struct AgentContext {
    /// Trajectory this step belongs to. Logged with every appended
    /// event by the runtime layer.
    pub trajectory_id: TrajectoryId,
    /// 1-indexed step within the trajectory. Computed by the runtime
    /// (max of existing + 1) before calling the agent — agents don't
    /// need to manage it themselves.
    pub step_seq: u32,
    /// The pipeline-wrapped LLM. Agents call `llm.call(req, ctx)`
    /// rather than reaching for a raw provider; this keeps cache /
    /// retry / breaker / routing in the request path uniformly.
    pub llm: Arc<dyn LlmService>,
    /// Cooperative cancellation token. Agents that do anything
    /// expensive should `select!` against `cancel.cancelled()` so an
    /// upstream Drop / SIGINT propagates.
    pub cancel: CancellationToken,
}

/// What an Agent returns from one execute() call.
///
/// `AgentOutput` is the typed surface the next layer (orchestration
/// loop / inter-agent router) consumes. It's NOT the same shape as
/// Doc 04 §4.2's `AgentMessage` — that's a richer enum carrying
/// PlanIssued / NeedsClarification / etc. for inter-agent flows. We
/// start with the LLM-shape variants because every concrete agent
/// today ends in "model said X"; the `AgentMessage` envelope wraps
/// these once a multi-agent flow needs it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOutput {
    /// Pure text response.
    Text { text: String },
    /// Tool calls only — no text. The agent (or its caller) is
    /// expected to dispatch the tools and feed results back.
    ToolCalls { calls: Vec<ToolCall> },
    /// Both — the model emitted commentary AND wants to call tools.
    /// Common for reasoning-heavy models.
    Mixed { text: String, calls: Vec<ToolCall> },
}

impl AgentOutput {
    /// Construct from a [`tars_types::ChatResponse`]'s `text` +
    /// `tool_calls`. Picks the right variant based on which fields
    /// are populated.
    pub fn from_response_parts(text: String, tool_calls: Vec<ToolCall>) -> Self {
        match (text.is_empty(), tool_calls.is_empty()) {
            (true, true) => Self::Text { text }, // empty text — degenerate but legal
            (true, false) => Self::ToolCalls { calls: tool_calls },
            (false, true) => Self::Text { text },
            (false, false) => Self::Mixed {
                text,
                calls: tool_calls,
            },
        }
    }

    /// Concatenated head of the text portion (if any), capped to
    /// `max_chars`. Used for the trajectory log's output summary.
    pub fn summary(&self, max_chars: usize) -> String {
        let text_ref = match self {
            Self::Text { text } | Self::Mixed { text, .. } => text.as_str(),
            Self::ToolCalls { calls } => {
                return format!("{} tool call(s)", calls.len());
            }
        };
        if text_ref.chars().count() <= max_chars {
            text_ref.to_string()
        } else {
            let head: String = text_ref.chars().take(max_chars).collect();
            format!("{head}…")
        }
    }
}

/// What one execute() call produces. `usage` aggregates token cost
/// for the step (sum across multiple LLM calls if the agent makes
/// more than one — today's `SingleShotAgent` always makes exactly
/// one).
#[derive(Clone, Debug)]
pub struct AgentStepResult {
    pub output: AgentOutput,
    pub usage: Usage,
}

/// Errors an Agent itself can surface. Storage / event-log failures
/// during the wrapping `execute_agent_step` are kept separate (they
/// come back as [`crate::RuntimeError`]).
#[derive(Debug, Error)]
pub enum AgentError {
    /// Underlying LLM call failed. Carries the typed
    /// [`ProviderError`] so callers can class-by-error
    /// (Permanent / Retriable / MaybeRetriable) without re-parsing.
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    /// Caller cancelled mid-execution.
    #[error("cancelled")]
    Cancelled,
    /// Catch-all for agent-internal logic errors (bad prompt
    /// construction, malformed tool spec, etc.).
    #[error("internal: {0}")]
    Internal(String),
}

impl AgentError {
    /// One-word classification for the trajectory log. Maps to the
    /// `classification` field on `AgentEvent::StepFailed`.
    pub fn classification(&self) -> &'static str {
        match self {
            Self::Provider(e) => match e.class() {
                tars_types::ErrorClass::Permanent => "permanent",
                tars_types::ErrorClass::Retriable => "retriable",
                tars_types::ErrorClass::MaybeRetriable => "maybe_retriable",
            },
            Self::Cancelled => "cancelled",
            Self::Internal(_) => "internal",
        }
    }
}

/// The agent contract itself. Implementations are stateless wrt one
/// execute() call: any per-trajectory state belongs in the trajectory
/// event log, not on the agent struct.
#[async_trait]
pub trait Agent: Send + Sync + 'static {
    /// Stable id for this agent instance. Logged in
    /// `AgentEvent::StepStarted::agent` so trajectory readers can
    /// trace which agent did what.
    fn id(&self) -> &AgentId;

    /// What kind of agent this is. Routing / inter-agent flows use
    /// this to decide who-talks-to-whom. Today's `SingleShotAgent`
    /// is just a `Worker` of an opaque domain.
    fn role(&self) -> AgentRole;

    /// Single step: build the LLM call from `input`, drive it
    /// through `ctx.llm`, return the parsed output + usage.
    /// The runtime layer wraps this with trajectory event writes.
    async fn execute(
        self: Arc<Self>,
        ctx: AgentContext,
        input: ChatRequest,
    ) -> Result<AgentStepResult, AgentError>;
}

/// Baseline agent: forwards the input to the LLM unchanged, drains
/// the stream into a [`ChatResponse`], wraps as [`AgentOutput`].
///
/// Not a real Orchestrator/Worker/Critic — those need prompt design
/// plus the typed `AgentMessage` protocol. `SingleShotAgent` is the
/// smallest agent that exercises the trait surface so the runtime
/// and trajectory wiring has a concrete consumer.
///
/// Cancellation: races `ctx.cancel.cancelled()` against the LLM
/// stream so a Drop'd parent doesn't leave the subprocess / HTTP
/// connection hanging.
pub struct SingleShotAgent {
    id: AgentId,
}

impl SingleShotAgent {
    pub fn new(id: impl Into<AgentId>) -> Arc<Self> {
        Arc::new(Self { id: id.into() })
    }
}

#[async_trait]
impl Agent for SingleShotAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn role(&self) -> AgentRole {
        AgentRole::Worker {
            domain: "single_shot".into(),
        }
    }

    async fn execute(
        self: Arc<Self>,
        ctx: AgentContext,
        input: ChatRequest,
    ) -> Result<AgentStepResult, AgentError> {
        drive_llm_call(ctx, input).await
    }
}

/// Shared helper: drive one LLM call to completion through `ctx.llm`,
/// drain the stream, return the result as [`AgentStepResult`].
///
/// `pub(crate)` so concrete agents whose contract is "wrap one LLM
/// call" (today: [`SingleShotAgent`], [`crate::OrchestratorAgent`])
/// can delegate without duplicating the cancel-aware drain. Agents
/// that do something more complex (multi-call, tool-loop, planner
/// with self-critique) build their own loops on top.
///
/// Cancellation: races `ctx.cancel.cancelled()` against
/// (a) the LLM stream open AND
/// (b) every event poll
/// so a Drop'd parent doesn't leave the subprocess / HTTP connection
/// hanging.
pub(crate) async fn drive_llm_call(
    ctx: AgentContext,
    input: ChatRequest,
) -> Result<AgentStepResult, AgentError> {
    // Shape an LLM RequestContext from what AgentContext gives us.
    // IAM / principal / tenant come once tars-security exists; for
    // now we use the test default and inherit cancel.
    let mut req_ctx = RequestContext::test_default();
    req_ctx.cancel = ctx.cancel.clone();

    let llm = ctx.llm.clone();

    // Race the LLM open against cancel — fast-fail if cancelled
    // before the provider even gets the request.
    let stream_result = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => return Err(AgentError::Cancelled),
        r = llm.call(input, req_ctx) => r,
    };
    let mut stream = stream_result?;

    let mut builder = ChatResponseBuilder::new();
    loop {
        let event = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(AgentError::Cancelled),
            ev = stream.next() => ev,
        };
        match event {
            Some(Ok(ev)) => builder.apply(ev),
            Some(Err(e)) => return Err(AgentError::Provider(e)),
            None => break,
        }
    }
    let response = builder.finish();

    let output = AgentOutput::from_response_parts(response.text, response.tool_calls);
    Ok(AgentStepResult {
        output,
        usage: response.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use tars_types::ToolCall;

    #[test]
    fn agent_output_from_parts_picks_the_right_variant() {
        // Pure text.
        let o = AgentOutput::from_response_parts("hi".into(), vec![]);
        assert!(matches!(o, AgentOutput::Text { .. }));

        // Tool calls only.
        let calls = vec![ToolCall::new("c1", "search", serde_json::json!({"q": "x"}))];
        let o = AgentOutput::from_response_parts(String::new(), calls);
        assert!(matches!(o, AgentOutput::ToolCalls { .. }));

        // Mixed.
        let calls = vec![ToolCall::new("c2", "search", serde_json::json!({"q": "y"}))];
        let o = AgentOutput::from_response_parts("commentary".into(), calls);
        assert!(matches!(o, AgentOutput::Mixed { .. }));

        // Empty/empty falls into Text(""). Documented.
        let o = AgentOutput::from_response_parts(String::new(), vec![]);
        assert!(matches!(o, AgentOutput::Text { text } if text.is_empty()));
    }

    #[test]
    fn summary_caps_long_text() {
        let long = "x".repeat(500);
        let o = AgentOutput::Text { text: long };
        let s = o.summary(50);
        assert_eq!(s.chars().count(), 51, "50 chars + ellipsis");
        assert!(s.ends_with('…'));
    }

    #[test]
    fn summary_for_tool_calls_reports_count() {
        let calls = vec![
            ToolCall::new("a", "x", serde_json::json!({})),
            ToolCall::new("b", "y", serde_json::json!({})),
        ];
        let o = AgentOutput::ToolCalls { calls };
        assert_eq!(o.summary(100), "2 tool call(s)");
    }

    #[test]
    fn agent_error_classification_maps_provider_class() {
        let e: AgentError = ProviderError::Auth("bad".into()).into();
        assert_eq!(e.classification(), "permanent");
        let e: AgentError = ProviderError::ModelOverloaded.into();
        assert_eq!(e.classification(), "retriable");
        let e: AgentError = ProviderError::Parse("bad json".into()).into();
        assert_eq!(e.classification(), "maybe_retriable");
        assert_eq!(AgentError::Cancelled.classification(), "cancelled");
        assert_eq!(
            AgentError::Internal("x".into()).classification(),
            "internal"
        );
    }

    #[test]
    fn role_serializes_with_snake_case_tag() {
        let r = AgentRole::Worker {
            domain: "code_review".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "worker");
        assert_eq!(v["domain"], "code_review");
    }

    #[test]
    fn single_shot_agent_reports_id_and_role() {
        let agent = SingleShotAgent::new(AgentId::new("test_agent"));
        assert_eq!(agent.id().as_ref(), "test_agent");
        assert!(matches!(agent.role(), AgentRole::Worker { .. }));
    }
}
