//! `OrchestratorAgent` — first concrete agent that does real work
//! beyond pass-through.
//!
//! Doc 04 §4.1 makes Orchestrator the planning agent: it takes a goal,
//! consults an LLM, and emits a structured `Plan` (a list of steps,
//! each tagged with the worker role that should execute it). The
//! Orchestrator does NOT execute the plan — that's the orchestration
//! loop's job (B-4 follow-on, depends on B-9 `tars-tools` for any
//! step that needs a tool).
//!
//! ## Design notes
//!
//! - **Structured output, not text-then-parse**. We set
//!   `ChatRequest.structured_output` so providers force the LLM to
//!   emit valid JSON matching the [`Plan`] schema. Doc 01 §9 shows
//!   each backend's translation (OpenAI `response_format=json_schema`,
//!   Gemini `responseSchema`, Anthropic forced-tool emulation).
//! - **Linear plans for MVP**. `PlanStep::depends_on` is in the
//!   schema but the only valid values today are previous steps' ids
//!   (no fan-out / merge edges). The full DAG semantics arrive when
//!   the orchestration loop actually consumes them.
//! - **Plan parsing kept on the Orchestrator**, not on `Agent`. The
//!   trait surface stays uniform (input = `ChatRequest`); the typed
//!   `plan(goal)` helper does the prompt construction + parse so
//!   callers get a typed `Plan` back without casting through
//!   `AgentOutput::Text`.
//! - **`temperature = 0.0`** baked into the planner request so the
//!   same goal yields the same plan (cache-friendly, replay-friendly,
//!   debugging-friendly).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use tars_types::{AgentId, ChatRequest};

use crate::agent::{Agent, AgentContext, AgentError, AgentOutput, AgentRole, AgentStepResult};
use crate::prompt::PromptBuilder;

// ── Plan data model ────────────────────────────────────────────────────

/// What the LLM-driven planner returns. Currently a flat-ish list of
/// steps with optional dependency ids; growing into a real DAG with
/// edge labels is left for when a consumer (the orchestration loop)
/// actually needs the richer shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    /// Unique id for this plan instance. The LLM is asked to fill it
    /// (typically a short slug like `"plan-abc"`); we don't enforce
    /// uniqueness across plans — that's the trajectory log's job.
    pub plan_id: String,
    /// The goal as the planner understood it. Useful for trajectory
    /// audit ("did the model understand what we asked?").
    pub goal: String,
    /// Steps in declaration order. `depends_on` carries the dependency
    /// graph; steps with empty `depends_on` are roots and can start
    /// immediately.
    pub steps: Vec<PlanStep>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanStep {
    /// Unique within the plan. Other steps reference this in
    /// `depends_on`.
    pub id: String,
    /// Which kind of worker should execute. Free-form today
    /// (`"code_review"`, `"summarise"`, `"answer_question"`, …);
    /// becomes a typed enum once `WorkerAgent` selection is
    /// type-checked at compile time.
    pub worker_role: String,
    /// Free-form instruction the worker receives as its prompt.
    pub instruction: String,
    /// IDs of steps that must complete first. Empty = can run
    /// immediately. The orchestration loop interprets this; the
    /// Orchestrator just records what the LLM said.
    pub depends_on: Vec<String>,
}

impl Plan {
    /// True iff the dependency graph is well-formed: every id is
    /// unique, every `depends_on` references a known step, and no
    /// step depends on itself or on a later-declared step (cycles
    /// would be a separate check; we keep it simple).
    pub fn validate(&self) -> Result<(), OrchestratorError> {
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.steps.len());
        for step in &self.steps {
            if !seen.insert(step.id.as_str()) {
                return Err(OrchestratorError::InvalidPlan(format!(
                    "duplicate step id: `{}`",
                    step.id,
                )));
            }
            for dep in &step.depends_on {
                if dep == &step.id {
                    return Err(OrchestratorError::InvalidPlan(format!(
                        "step `{}` depends on itself",
                        step.id,
                    )));
                }
                if !seen.contains(dep.as_str()) {
                    return Err(OrchestratorError::InvalidPlan(format!(
                        "step `{}` depends on unknown / later step `{}`",
                        step.id, dep,
                    )));
                }
            }
        }
        Ok(())
    }
}

// ── OrchestratorAgent ──────────────────────────────────────────────────

/// LLM-driven planner. Wraps a model name + the planner prompt; each
/// `plan(goal)` call produces a typed [`Plan`].
pub struct OrchestratorAgent {
    id: AgentId,
    model: String,
}

impl OrchestratorAgent {
    pub fn new(id: impl Into<AgentId>, model: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            model: model.into(),
        })
    }

    /// Typed convenience: build the planner [`ChatRequest`] for `goal`,
    /// run it through `self.execute()`, parse the resulting JSON into
    /// a [`Plan`], and validate the dependency shape.
    ///
    /// Returns the typed `Plan` on success. Errors split into:
    ///   `OrchestratorError::Agent`   — LLM call failed
    ///   `OrchestratorError::Decode`  — model emitted invalid JSON
    ///   `OrchestratorError::InvalidPlan` — dependency graph broken
    ///   `OrchestratorError::UnexpectedOutput` — output wasn't text
    ///                                            (e.g. tool calls)
    pub async fn plan(
        self: Arc<Self>,
        ctx: AgentContext,
        goal: &str,
    ) -> Result<Plan, OrchestratorError> {
        let req = self.build_planner_request(goal);
        let result = self.clone().execute(ctx, req).await?;
        let json_text = match result.output {
            AgentOutput::Text { text } => text,
            other => {
                return Err(OrchestratorError::UnexpectedOutput(format!(
                    "expected JSON text from the planner; got {other:?}"
                )));
            }
        };
        Self::parse_plan_response(&json_text)
    }

    /// Lower-level: parse the JSON the planner emitted into a typed,
    /// validated [`Plan`]. Exposed `pub` so the orchestration loop can
    /// drive trajectory-logged execution via [`crate::execute_agent_step`]
    /// (which returns raw [`AgentOutput`]) and parse the result here.
    pub fn parse_plan_response(json_text: &str) -> Result<Plan, OrchestratorError> {
        let plan: Plan = serde_json::from_str(json_text).map_err(OrchestratorError::Decode)?;
        plan.validate()?;
        Ok(plan)
    }

    /// Construct the planner ChatRequest. Exposed `pub` so the
    /// orchestration loop can drive trajectory-logged execution via
    /// [`crate::execute_agent_step`]; integration tests use it to
    /// inspect what we'd send without invoking an LLM.
    pub fn build_planner_request(&self, goal: &str) -> ChatRequest {
        // Determinism rationale lives on `PromptBuilder::deterministic`.
        PromptBuilder::new(self.model.clone(), goal)
            .system(PLANNER_SYSTEM_PROMPT)
            .structured_output("Plan", plan_json_schema())
            .deterministic()
            .build()
    }
}

#[async_trait]
impl Agent for OrchestratorAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn role(&self) -> AgentRole {
        AgentRole::Orchestrator
    }

    async fn execute(
        self: Arc<Self>,
        ctx: AgentContext,
        input: ChatRequest,
    ) -> Result<AgentStepResult, AgentError> {
        // Same shape as SingleShotAgent — drain the stream into a
        // ChatResponse, wrap as AgentOutput. The structured-output
        // contract belongs to whoever built `input`; we're a
        // pass-through at this layer (the typed parsing happens in
        // `plan()` above).
        crate::agent::drive_llm_call(ctx, input).await
    }
}

// ── Errors ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// Underlying LLM call failed.
    #[error("agent: {0}")]
    Agent(#[from] AgentError),
    /// Model returned text that wasn't valid JSON for [`Plan`].
    #[error("decode: {0}")]
    Decode(serde_json::Error),
    /// Dependency graph in the parsed plan is malformed.
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
    /// Model returned tool calls or empty output instead of the JSON
    /// plan. Usually means structured output was disabled / ignored
    /// at the provider level.
    #[error("unexpected output: {0}")]
    UnexpectedOutput(String),
}

// ── Prompt + schema ────────────────────────────────────────────────────

const PLANNER_SYSTEM_PROMPT: &str = "\
You are a task planner. Given a goal, produce a JSON plan that breaks the goal into \
1–5 concrete steps.

Each step has:
  - `id`: a short unique identifier (e.g. \"s1\", \"s2\"); other steps reference it.
  - `worker_role`: which kind of worker should execute (free-form string;
    common choices: \"summarise\", \"answer_question\", \"code_review\", \"search\",
    \"write_file\", \"web_fetch\"). Pick whatever best describes the work.
  - `instruction`: a clear, self-contained instruction the worker can follow without
    seeing the full conversation.
  - `depends_on`: list of step ids that must complete before this one can start.
    Empty = can run immediately. Don't reference future steps; declarations are
    in execution-allowed order.

Constraints:
  - Keep plans small (1–5 steps).
  - Steps must form a valid DAG (no cycles, no forward references).
  - Respond with JSON only, matching the schema; do NOT include any prose.";

/// JSON schema for the [`Plan`] type. Used as the
/// `ChatRequest.structured_output` so providers enforce the shape.
fn plan_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "plan_id": { "type": "string" },
            "goal":    { "type": "string" },
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "id":          { "type": "string" },
                        "worker_role": { "type": "string" },
                        "instruction": { "type": "string" },
                        "depends_on": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["id", "worker_role", "instruction", "depends_on"]
                }
            }
        },
        "required": ["plan_id", "goal", "steps"]
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
            steps: vec![
                PlanStep {
                    id: "s1".into(),
                    worker_role: "search".into(),
                    instruction: "fetch the PR diff".into(),
                    depends_on: vec![],
                },
                PlanStep {
                    id: "s2".into(),
                    worker_role: "summarise".into(),
                    instruction: "summarise the diff for a non-engineer".into(),
                    depends_on: vec!["s1".into()],
                },
            ],
        }
    }

    #[test]
    fn plan_round_trips_through_json() {
        let original = sample_plan();
        let v = serde_json::to_value(&original).unwrap();
        let back: Plan = serde_json::from_value(v).unwrap();
        assert_eq!(back.plan_id, original.plan_id);
        assert_eq!(back.steps.len(), 2);
        assert_eq!(back.steps[1].depends_on, vec!["s1".to_string()]);
    }

    #[test]
    fn validate_accepts_well_formed_plan() {
        assert!(sample_plan().validate().is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_step_ids() {
        let mut p = sample_plan();
        p.steps[1].id = "s1".into();
        match p.validate() {
            Err(OrchestratorError::InvalidPlan(msg)) => {
                assert!(msg.contains("duplicate"));
            }
            other => panic!("expected InvalidPlan, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_self_dependency() {
        let mut p = sample_plan();
        p.steps[0].depends_on = vec!["s1".into()];
        match p.validate() {
            Err(OrchestratorError::InvalidPlan(msg)) => {
                assert!(msg.contains("itself"));
            }
            other => panic!("expected InvalidPlan, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_unknown_dependency() {
        let mut p = sample_plan();
        p.steps[1].depends_on = vec!["does_not_exist".into()];
        match p.validate() {
            Err(OrchestratorError::InvalidPlan(msg)) => {
                assert!(msg.contains("unknown"));
                assert!(msg.contains("does_not_exist"));
            }
            other => panic!("expected InvalidPlan, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_forward_reference() {
        // s1 depends on s2 which is declared later.
        let mut p = sample_plan();
        p.steps[0].depends_on = vec!["s2".into()];
        let result = p.validate();
        // "later step" is bucketed under the "unknown" check (we
        // build the seen set in declaration order).
        assert!(result.is_err());
    }

    #[test]
    fn build_planner_request_sets_strict_schema_and_temperature() {
        let agent = OrchestratorAgent::new(AgentId::new("orch"), "gpt-4o");
        let req = agent.build_planner_request("summarise PR #42");
        assert_eq!(req.temperature, Some(0.0));
        assert!(req.system.is_some());
        assert!(req.system.as_ref().unwrap().contains("planner"));
        let schema = req
            .structured_output
            .as_ref()
            .expect("structured_output set");
        assert!(schema.strict);
        assert_eq!(schema.name.as_deref(), Some("Plan"));
        // Schema describes a Plan-shaped object.
        assert_eq!(schema.schema["type"], json!("object"));
        assert!(schema.schema["properties"]["steps"].is_object());
    }

    #[test]
    fn schema_is_self_consistent_with_plan_struct() {
        // A real Plan must serialize into something the schema would
        // accept. We don't run a full validator here (no jsonschema
        // dep); just spot-check the field names.
        let plan_json = serde_json::to_value(sample_plan()).unwrap();
        let schema = plan_json_schema();
        let required = schema["required"].as_array().unwrap();
        for field in required {
            let f = field.as_str().unwrap();
            assert!(
                plan_json.get(f).is_some(),
                "Plan struct missing schema-required field `{f}`",
            );
        }
    }

    #[test]
    fn orchestrator_role_is_orchestrator() {
        let agent = OrchestratorAgent::new(AgentId::new("orch"), "gpt-4o");
        assert!(matches!(agent.role(), AgentRole::Orchestrator));
        assert_eq!(agent.id().as_ref(), "orch");
    }
}
