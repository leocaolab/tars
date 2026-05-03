//! `WorkerAgent` — third concrete default agent (Doc 04 §4.1).
//!
//! Executes one [`PlanStep`] and emits a typed
//! [`AgentMessage::PartialResult`]. Today's first cut is intentionally
//! a **stub**: the Worker has no tool registry yet (B-9 ships
//! `tars-tools`), so it can only ask the LLM to describe how it would
//! perform the step — no real I/O. The Worker still:
//!
//! - exercises the Agent trait + trajectory wiring end-to-end,
//! - threads refinement suggestions from a previous Critic verdict back
//!   into the prompt (the Refine loop in `run_task` needs this), and
//! - emits the same typed [`AgentMessage::PartialResult`] envelope a
//!   real tool-using Worker will produce later, so downstream code
//!   (Critic, orchestration loop, replay) doesn't need to change when
//!   the stub becomes a real Worker.
//!
//! ## Wire format
//!
//! Same flat-JSON pattern as [`crate::CriticAgent`]: the LLM emits
//! `{"summary": "...", "confidence": 0.0..1.0}`, we map it to
//! `AgentMessage::PartialResult`. Schema is enforced via
//! [`ChatRequest::structured_output`] so providers reject malformed
//! responses upstream.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use tars_types::{AgentId, ChatRequest, JsonSchema, ModelHint};

use crate::agent::{Agent, AgentContext, AgentError, AgentOutput, AgentRole, AgentStepResult};
use crate::message::AgentMessage;
use crate::orchestrator::{Plan, PlanStep};

/// LLM-driven Worker — the agent that actually does the work for one
/// plan step. See module docs for the stub-vs-real-tool caveat.
pub struct WorkerAgent {
    id: AgentId,
    model: String,
    /// Free-form domain label (`"summarise"`, `"code_review"`, …).
    /// Surfaces in [`Agent::role`] so a future router can match
    /// `PlanStep::worker_role` → `WorkerAgent` by domain.
    domain: String,
}

impl WorkerAgent {
    pub fn new(
        id: impl Into<AgentId>,
        model: impl Into<String>,
        domain: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            model: model.into(),
            domain: domain.into(),
        })
    }

    /// Domain label this Worker handles. Used by the orchestration loop
    /// to pick a Worker for each [`PlanStep`].
    pub fn domain(&self) -> &str {
        &self.domain
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

        let mut req = ChatRequest::user(ModelHint::Explicit(self.model.clone()), user_text);
        req.system = Some(WORKER_SYSTEM_PROMPT.to_string());
        req.structured_output = Some(JsonSchema::strict("WorkerResult", worker_json_schema()));
        // Worker output should be deterministic for the same step so
        // cache + replay both work.
        req.temperature = Some(0.0);
        req
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
        // Pass-through to the shared LLM-call drainer — same shape as
        // OrchestratorAgent and CriticAgent. The typed parsing happens
        // in `execute_step` / `parse_worker_response`.
        crate::agent::drive_llm_call(ctx, input).await
    }
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
