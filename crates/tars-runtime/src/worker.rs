//! `WorkerAgent` — third concrete default agent (Doc 04 §4.1).
//!
//! Executes one [`PlanStep`] and emits a typed
//! [`AgentMessage::PartialResult`]. Today's WorkerAgent comes in two
//! flavours, sharing the same Agent-trait surface and the same typed
//! output envelope:
//!
//! - **No-tools (stub)** — constructed via [`WorkerAgent::new`]. The
//!   model is asked to *describe* how it would perform the step.
//!   Useful for testing the orchestration loop without spinning up
//!   real I/O.
//! - **Tool-using** — constructed via [`WorkerAgent::with_tools`].
//!   The model has a [`ToolRegistry`] available; on each LLM turn it
//!   may emit tool calls instead of (or alongside) a final answer.
//!   The Worker dispatches each call via the registry, threads the
//!   results back into the conversation, and re-prompts until the
//!   model emits a text-only answer (or hits
//!   `max_tool_iterations`). The same [`AgentMessage::PartialResult`]
//!   envelope flows out either way, so downstream code (Critic,
//!   orchestration loop, replay) doesn't change between stub and
//!   real.
//!
//! ## Wire format
//!
//! Same flat-JSON pattern as [`crate::CriticAgent`]: the LLM emits
//! `{"summary": "...", "confidence": 0.0..1.0}` once it's ready to
//! finalise, we map it to `AgentMessage::PartialResult`. Schema is
//! enforced via [`ChatRequest::structured_output`] so providers
//! reject malformed responses upstream. Strict-mode + tool calls
//! coexist: the model is free to emit tool calls (which bypass the
//! response_format constraint) on any turn, and only the final
//! text-only answer must conform to the schema.
//!
//! ## Trajectory observability — known gap
//!
//! When tools are present, one `Agent::execute` call drives **N** LLM
//! calls internally (one per tool round-trip). The trajectory layer
//! records this as a single `StepStarted/LlmCallCaptured/StepCompleted`
//! triple — `LlmCallCaptured.usage` sums across the N calls and
//! `response_summary` reflects the final text answer. The intermediate
//! LLM calls and tool dispatches don't get their own events. This is
//! a real observability loss; it's deferred until a consumer needs
//! per-call replay (which slots in alongside Backtrack + Saga; the
//! new event variants would be `LlmSubcallCaptured` + `ToolCallExecuted`).

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tars_pipeline::LlmService;
use tars_tools::{ToolContext, ToolRegistry};
use tars_types::{
    AgentId, ChatRequest, ChatResponseBuilder, ContentBlock, Message, RequestContext,
};

use crate::agent::{Agent, AgentContext, AgentError, AgentOutput, AgentRole, AgentStepResult};
use crate::message::AgentMessage;
use crate::orchestrator::{Plan, PlanStep};
use crate::prompt::PromptBuilder;

/// Default safety cap on Worker tool-loop iterations. 8 round-trips
/// (each LLM call + tool dispatch) is enough headroom for a Worker
/// to chain a handful of file reads / git lookups / etc., and small
/// enough that a confused model can't burn through the budget by
/// looping on the same call indefinitely.
pub const DEFAULT_MAX_TOOL_ITERATIONS: u32 = 8;

/// LLM-driven Worker — the agent that actually does the work for one
/// plan step. See module docs for stub-vs-tool flavours.
pub struct WorkerAgent {
    id: AgentId,
    model: String,
    /// Free-form domain label (`"summarise"`, `"code_review"`, …).
    /// Surfaces in [`Agent::role`] so a future router can match
    /// `PlanStep::worker_role` → `WorkerAgent` by domain.
    domain: String,
    /// `Some` for tool-using Workers; `None` for the stub flavour.
    /// `Arc` so a single registry instance can be shared across many
    /// Workers (the orchestration loop typically has one registry +
    /// one Worker per domain).
    tools: Option<Arc<ToolRegistry>>,
    /// Cap on tool round-trips per `execute` call. Only consulted
    /// when `tools.is_some()`. See [`DEFAULT_MAX_TOOL_ITERATIONS`].
    max_tool_iterations: u32,
}

impl WorkerAgent {
    /// Stub Worker — no tools. The LLM is asked to describe how it
    /// would perform the step. Useful for testing the orchestration
    /// loop without spinning up real I/O.
    pub fn new(
        id: impl Into<AgentId>,
        model: impl Into<String>,
        domain: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            model: model.into(),
            domain: domain.into(),
            tools: None,
            max_tool_iterations: DEFAULT_MAX_TOOL_ITERATIONS,
        })
    }

    /// Tool-using Worker. The model gets `registry.to_tool_specs()`
    /// in `req.tools` and may emit tool calls; the Worker dispatches
    /// each via the registry and threads results back until the model
    /// emits a text-only answer (or `max_tool_iterations` fires).
    ///
    /// Default iteration cap is [`DEFAULT_MAX_TOOL_ITERATIONS`]; use
    /// [`Self::with_max_tool_iterations`] to override on the returned
    /// Arc (one of the rare places we need an Arc-mut pattern; we
    /// take + rebuild rather than expose interior mutability).
    pub fn with_tools(
        id: impl Into<AgentId>,
        model: impl Into<String>,
        domain: impl Into<String>,
        tools: Arc<ToolRegistry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            model: model.into(),
            domain: domain.into(),
            tools: Some(tools),
            max_tool_iterations: DEFAULT_MAX_TOOL_ITERATIONS,
        })
    }

    /// Override the tool-loop iteration cap. Returns a fresh Arc.
    /// No-op for stub Workers (no tools means no loop).
    pub fn with_max_tool_iterations(self: Arc<Self>, n: u32) -> Arc<Self> {
        Arc::new(Self {
            id: self.id.clone(),
            model: self.model.clone(),
            domain: self.domain.clone(),
            tools: self.tools.clone(),
            max_tool_iterations: n,
        })
    }

    /// Domain label this Worker handles. Used by the orchestration loop
    /// to pick a Worker for each [`PlanStep`].
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// True iff this Worker has a tool registry attached.
    pub fn has_tools(&self) -> bool {
        self.tools.is_some()
    }

    /// Typed convenience: build the worker [`ChatRequest`] for `step`,
    /// run it through `self.execute`, parse the JSON into a typed
    /// [`AgentMessage::PartialResult`].
    ///
    /// `refinements` carries any prior Critic suggestions; an empty
    /// slice means this is the step's first attempt.
    pub async fn execute_step(
        self: Arc<Self>,
        ctx: AgentContext,
        plan: &Plan,
        step: &PlanStep,
        refinements: &[String],
    ) -> Result<AgentMessage, WorkerError> {
        let req = self.build_worker_request(plan, step, refinements);
        let agent_result = self.clone().execute(ctx, req).await?;
        let json_text = match agent_result.output {
            AgentOutput::Text { text } => text,
            other => {
                return Err(WorkerError::UnexpectedOutput(format!(
                    "expected JSON text from worker; got {other:?}"
                )));
            }
        };
        Self::parse_worker_response(&json_text, &self.id, Some(step.id.as_str()))
    }

    /// Lower-level: build the worker `ChatRequest`. Exposed `pub` so
    /// the orchestration loop can drive trajectory-logged execution
    /// via [`crate::execute_agent_step`] and parse the result with
    /// [`Self::parse_worker_response`].
    pub fn build_worker_request(
        &self,
        plan: &Plan,
        step: &PlanStep,
        refinements: &[String],
    ) -> ChatRequest {
        let payload = serde_json::json!({
            "goal": plan.goal,
            "step": {
                "id": step.id,
                "worker_role": step.worker_role,
                "instruction": step.instruction,
                "depends_on": step.depends_on,
            },
            "refinements": refinements,
        });
        let user_text = serde_json::to_string_pretty(&payload)
            .expect("JSON encoding of plan/step is infallible for valid types");

        let system_prompt = if self.tools.is_some() {
            WORKER_SYSTEM_PROMPT_WITH_TOOLS
        } else {
            WORKER_SYSTEM_PROMPT
        };
        let tool_specs = self
            .tools
            .as_ref()
            .map(|r| r.to_tool_specs())
            .unwrap_or_default();

        PromptBuilder::new(self.model.clone(), user_text)
            .system(system_prompt)
            .structured_output("WorkerResult", worker_json_schema())
            .tools(tool_specs)
            .deterministic()
            .build()
    }

    /// Lower-level: parse the JSON the model emitted into a typed
    /// [`AgentMessage::PartialResult`]. `from_agent` is the worker's
    /// id; `step_id` is the [`PlanStep::id`] this result addresses
    /// (`None` for free-standing worker output not tied to a plan).
    pub fn parse_worker_response(
        json_text: &str,
        from_agent: &AgentId,
        step_id: Option<&str>,
    ) -> Result<AgentMessage, WorkerError> {
        let raw: RawWorkerResult =
            serde_json::from_str(json_text).map_err(WorkerError::Decode)?;
        if raw.summary.trim().is_empty() {
            return Err(WorkerError::InvalidResult("summary is empty".into()));
        }
        let confidence = raw.confidence.clamp(0.0, 1.0);
        Ok(AgentMessage::PartialResult {
            from_agent: from_agent.clone(),
            step_id: step_id.map(str::to_string),
            summary: raw.summary,
            confidence,
        })
    }
}

#[async_trait]
impl Agent for WorkerAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn role(&self) -> AgentRole {
        AgentRole::Worker { domain: self.domain.clone() }
    }

    async fn execute(
        self: Arc<Self>,
        ctx: AgentContext,
        input: ChatRequest,
    ) -> Result<AgentStepResult, AgentError> {
        // Take the Arc<ToolRegistry> out via clone so the subsequent
        // `self` move into `drive_with_tools` doesn't conflict with
        // the borrow used to read `self.tools`.
        let tools = self.tools.clone();
        match tools {
            // Stub Worker — same single-call shape as Orchestrator /
            // Critic; the typed parsing happens in
            // `execute_step` / `parse_worker_response`.
            None => crate::agent::drive_llm_call(ctx, input).await,
            // Tool-using Worker — drive the multi-call dispatch loop.
            // See module docs for the trajectory observability tradeoff
            // (one Agent::execute hides N internal LLM calls).
            Some(registry) => self.drive_with_tools(ctx, input, registry).await,
        }
    }
}

impl WorkerAgent {
    /// Inner loop for the tool-using flavour. Drains one LLM call at a
    /// time; if the model emits tool calls, dispatches each via the
    /// registry, appends the assistant + tool messages to the
    /// conversation, and loops. Stops on the first text-only response
    /// or when `max_tool_iterations` fires.
    ///
    /// Usage is summed across every internal LLM call so the resulting
    /// `AgentStepResult.usage` reflects the true cost of the step.
    async fn drive_with_tools(
        self: Arc<Self>,
        ctx: AgentContext,
        initial_input: ChatRequest,
        registry: Arc<ToolRegistry>,
    ) -> Result<AgentStepResult, AgentError> {
        let mut req = initial_input;
        let mut total_usage = tars_types::Usage::default();
        for iteration in 0..self.max_tool_iterations {
            // One LLM round-trip — same cancel-aware drain shape as
            // `agent::drive_llm_call`.
            let response = drain_one_call(&ctx, req.clone()).await?;
            total_usage = total_usage.merge(response.usage);

            if response.tool_calls.is_empty() {
                // Final answer — text only. Build the AgentStepResult
                // and return.
                let output = AgentOutput::from_response_parts(response.text, vec![]);
                return Ok(AgentStepResult { output, usage: total_usage });
            }

            // Tool calls present. Build the next request: append the
            // assistant's tool-call message, then one Tool message
            // per dispatched call.
            let assistant_text = response.text.clone();
            let assistant_msg = Message::Assistant {
                content: if assistant_text.is_empty() {
                    Vec::new()
                } else {
                    vec![ContentBlock::text(assistant_text)]
                },
                tool_calls: response.tool_calls.clone(),
            };
            req.messages.push(assistant_msg);

            for call in &response.tool_calls {
                let tool_ctx = ToolContext {
                    cancel: ctx.cancel.clone(),
                    cwd: None,
                };
                let tool_msg = registry.dispatch(call, tool_ctx).await;
                req.messages.push(tool_msg);
            }

            tracing::debug!(
                iteration = iteration + 1,
                tool_calls = response.tool_calls.len(),
                "worker: dispatched tools, looping",
            );
        }

        Err(AgentError::Internal(format!(
            "worker tool loop hit max_tool_iterations={} without a text-only \
             response (model kept emitting tool calls)",
            self.max_tool_iterations,
        )))
    }
}

/// Drain one LLM call to a [`tars_types::ChatResponse`]. Same
/// cancel-handling pattern as `agent::drive_llm_call` but returns
/// the full response (we need the tool_calls + usage explicitly,
/// not flattened into AgentOutput).
async fn drain_one_call(
    ctx: &AgentContext,
    input: ChatRequest,
) -> Result<tars_types::ChatResponse, AgentError> {
    let mut req_ctx = RequestContext::test_default();
    req_ctx.cancel = ctx.cancel.clone();

    let llm: Arc<dyn LlmService> = ctx.llm.clone();

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
    Ok(builder.finish())
}


// ── Wire format ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
struct RawWorkerResult {
    summary: String,
    confidence: f32,
}

// ── Errors ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum WorkerError {
    /// Underlying LLM call failed.
    #[error("agent: {0}")]
    Agent(#[from] AgentError),
    /// Model returned text that didn't parse as the worker shape.
    #[error("decode: {0}")]
    Decode(serde_json::Error),
    /// Model returned tool calls or empty output instead of JSON.
    #[error("unexpected output: {0}")]
    UnexpectedOutput(String),
    /// Decoded shape was structurally valid but semantically broken
    /// (e.g. empty summary).
    #[error("invalid result: {0}")]
    InvalidResult(String),
}

// ── Prompt + schema ────────────────────────────────────────────────────

const WORKER_SYSTEM_PROMPT: &str = "\
You are a Worker agent. You receive ONE step from a larger plan and execute it.

Your input JSON contains:
  - `goal`: the original task the Orchestrator was asked to solve.
  - `step`: the specific step you are executing (`id`, `worker_role`,
    `instruction`, `depends_on`).
  - `refinements`: a list of suggestions from a prior Critic review of
    your previous attempt at this step. Empty on the first attempt.
    When non-empty, address each suggestion in your new attempt.

You currently have no external tools — describe what you would do or \
produce the requested artifact directly in your `summary`.

Output rules:
  - Respond with JSON only matching the schema; do NOT include any prose.
  - `summary`: a concise (1–3 sentence) description of what you did /
    produced for this step. The Critic and the next Worker step both
    consume this — write something useful, not boilerplate.
  - `confidence`: float in [0.0, 1.0] — how sure you are this addresses
    the step's instruction. 1.0 = certain; 0.5 = unsure but best effort;
    0.0 = couldn't make progress.";

const WORKER_SYSTEM_PROMPT_WITH_TOOLS: &str = "\
You are a Worker agent. You receive ONE step from a larger plan and execute it.

Your input JSON contains:
  - `goal`: the original task the Orchestrator was asked to solve.
  - `step`: the specific step you are executing (`id`, `worker_role`,
    `instruction`, `depends_on`).
  - `refinements`: a list of suggestions from a prior Critic review of
    your previous attempt at this step. Empty on the first attempt.
    When non-empty, address each suggestion in your new attempt.

You have access to a set of tools. Call them when you need to inspect or \
manipulate the outside world (read files, fetch git diffs, etc.). The \
runtime dispatches each call and feeds the result back to you on the \
next turn. Only emit your final answer when you have what you need.

Output rules:
  - On any turn, you may either (a) call one or more tools, OR (b) emit \
    your final JSON answer matching the schema. NOT both.
  - The final answer JSON: `summary` is a concise (1–3 sentence) \
    description of what you produced for this step. `confidence` is a \
    float in [0.0, 1.0] — how sure you are this addresses the step's \
    instruction.
  - Don't loop on the same tool call. If a tool returned an error, try \
    something different or finalise with a low confidence + a summary \
    explaining the obstacle.";

fn worker_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": {
                "type": "string",
                "description": "Concise description of what this Worker did for the step."
            },
            "confidence": {
                "type": "number",
                "description": "Worker self-reported confidence in the result, 0.0..=1.0."
            }
        },
        "required": ["summary", "confidence"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_plan() -> Plan {
        Plan {
            plan_id: "p1".into(),
            goal: "summarise PR #42".into(),
            steps: vec![PlanStep {
                id: "s1".into(),
                worker_role: "summarise".into(),
                instruction: "produce a 2-sentence summary".into(),
                depends_on: vec![],
            }],
        }
    }

    #[test]
    fn worker_role_carries_domain() {
        let w = WorkerAgent::new(AgentId::new("w"), "gpt-4o", "summarise");
        match w.role() {
            AgentRole::Worker { domain } => assert_eq!(domain, "summarise"),
            other => panic!("expected Worker role, got {other:?}"),
        }
        assert_eq!(w.domain(), "summarise");
        assert_eq!(w.id().as_ref(), "w");
    }

    #[test]
    fn build_worker_request_sets_strict_schema_and_temperature() {
        let w = WorkerAgent::new(AgentId::new("w"), "gpt-4o", "summarise");
        let plan = sample_plan();
        let req = w.build_worker_request(&plan, &plan.steps[0], &[]);
        assert_eq!(req.temperature, Some(0.0));
        assert!(req.system.as_ref().unwrap().contains("Worker"));
        let schema = req.structured_output.as_ref().expect("structured_output set");
        assert!(schema.strict);
        assert_eq!(schema.name.as_deref(), Some("WorkerResult"));
        let required: Vec<&str> = schema.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"summary"));
        assert!(required.contains(&"confidence"));
    }

    #[test]
    fn build_worker_request_payload_includes_step_and_goal_and_refinements() {
        let w = WorkerAgent::new(AgentId::new("w"), "gpt-4o", "summarise");
        let plan = sample_plan();
        let refinements = vec!["mention security".to_string(), "shorter".to_string()];
        let req = w.build_worker_request(&plan, &plan.steps[0], &refinements);
        let user_text = req.messages[0].content()[0].as_text().unwrap();
        assert!(user_text.contains("summarise PR #42"));
        assert!(user_text.contains("\"step\""));
        assert!(user_text.contains("produce a 2-sentence summary"));
        assert!(user_text.contains("mention security"));
        assert!(user_text.contains("\"refinements\""));
    }

    #[test]
    fn parse_worker_response_happy_path() {
        let id = AgentId::new("worker:summarise");
        let json = r#"{"summary":"Summarised the diff in two sentences.","confidence":0.85}"#;
        let msg = WorkerAgent::parse_worker_response(json, &id, Some("s1")).unwrap();
        match msg {
            AgentMessage::PartialResult { from_agent, step_id, summary, confidence } => {
                assert_eq!(from_agent.as_ref(), "worker:summarise");
                assert_eq!(step_id.as_deref(), Some("s1"));
                assert_eq!(summary, "Summarised the diff in two sentences.");
                assert!((confidence - 0.85).abs() < f32::EPSILON);
            }
            other => panic!("expected PartialResult, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_response_clamps_confidence_into_range() {
        let id = AgentId::new("w");
        let high = r#"{"summary":"ok","confidence":1.5}"#;
        let low = r#"{"summary":"ok","confidence":-0.3}"#;
        let h = WorkerAgent::parse_worker_response(high, &id, None).unwrap();
        let l = WorkerAgent::parse_worker_response(low, &id, None).unwrap();
        match (h, l) {
            (
                AgentMessage::PartialResult { confidence: ch, .. },
                AgentMessage::PartialResult { confidence: cl, .. },
            ) => {
                assert!((ch - 1.0).abs() < f32::EPSILON);
                assert!(cl.abs() < f32::EPSILON);
            }
            _ => panic!("expected PartialResult"),
        }
    }

    #[test]
    fn parse_worker_response_rejects_empty_summary() {
        let id = AgentId::new("w");
        let json = r#"{"summary":"   ","confidence":0.5}"#;
        match WorkerAgent::parse_worker_response(json, &id, Some("s1")) {
            Err(WorkerError::InvalidResult(msg)) => assert!(msg.contains("summary")),
            other => panic!("expected InvalidResult, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_response_surfaces_decode_error_on_bad_json() {
        let id = AgentId::new("w");
        match WorkerAgent::parse_worker_response("not json", &id, None) {
            Err(WorkerError::Decode(_)) => {}
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn schema_marks_all_top_level_fields_required() {
        let schema = worker_json_schema();
        let required = schema["required"].as_array().unwrap();
        let properties = schema["properties"].as_object().unwrap();
        for prop_name in properties.keys() {
            assert!(
                required.iter().any(|r| r.as_str() == Some(prop_name.as_str())),
                "property `{prop_name}` is in `properties` but not in `required` — \
                 OpenAI strict mode will reject this schema",
            );
        }
        assert_eq!(schema["additionalProperties"], json!(false));
    }
}
