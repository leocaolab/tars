//! `CriticAgent` — the second concrete default agent (Doc 04 §4.1).
//!
//! Takes a `(Plan, PartialResult, original goal)` triple and emits a
//! typed [`AgentMessage::Verdict`] (`Approve` / `Reject{reason}` /
//! `Refine{suggestions}`). The orchestration loop reads the verdict
//! to decide between continue / replan / re-run-with-suggestions.
//!
//! ## Design
//!
//! - **Same Agent-trait shape as OrchestratorAgent**: pass-through
//!   `execute` (runs the LLM call), typed `critique()` helper does
//!   the prompt + parse. Keeps the trait surface uniform.
//! - **Flat JSON schema** for the verdict on the wire — `kind` plus
//!   always-present `reason` + `suggestions` (empty when not
//!   relevant). Avoids `oneOf` / `anyOf` at the schema root which
//!   OpenAI strict mode handles awkwardly and Anthropic's
//!   tool-emulation path translates poorly. Mapped to typed
//!   [`VerdictKind`] in the `critique()` helper.
//! - **`temperature = 0.0`** baked in (cache-friendly, replay-friendly,
//!   the Critic should be deterministic given the same inputs).
//! - **No CLI subcommand**: critique only makes sense inside an
//!   orchestration loop. `tars critique` standalone has no real
//!   consumer — defer until the multi-step loop ships.
//!
//! ## What it doesn't do (yet)
//!
//! - **Multi-step trace input**: today the Critic sees only the
//!   plan + one PartialResult + the goal. The real Critic should
//!   see the prior step's outputs too. We'll add when the
//!   orchestration loop has a way to thread those (probably as
//!   `Vec<AgentMessage::PartialResult>`).
//! - **Confidence thresholds**: a Critic could short-circuit to
//!   Approve when the Worker's confidence is high. Defer until we
//!   have data on calibration.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use tars_types::{AgentId, ChatRequest, JsonSchema, ModelHint};

use crate::agent::{Agent, AgentContext, AgentError, AgentOutput, AgentRole, AgentStepResult};
use crate::message::{AgentMessage, VerdictKind};
use crate::orchestrator::Plan;

// ── CriticAgent ────────────────────────────────────────────────────────

/// LLM-driven Critic. Wraps a model name + the critique prompt; each
/// `critique()` call emits a typed [`AgentMessage::Verdict`].
pub struct CriticAgent {
    id: AgentId,
    model: String,
}

impl CriticAgent {
    pub fn new(id: impl Into<AgentId>, model: impl Into<String>) -> Arc<Self> {
        Arc::new(Self { id: id.into(), model: model.into() })
    }

    /// Typed convenience: build the critique [`ChatRequest`] for the
    /// given (plan, result, goal), run it through `self.execute`, parse
    /// the JSON into a typed [`AgentMessage::Verdict`].
    ///
    /// `target_step_id` defaults to `result.step_id` so callers who
    /// already filled it in on the PartialResult don't have to repeat.
    /// Pass `None` if the verdict is for the whole plan rather than
    /// one step.
    pub async fn critique(
        self: Arc<Self>,
        ctx: AgentContext,
        plan: &Plan,
        result: &PartialResultRef<'_>,
        goal: &str,
    ) -> Result<AgentMessage, CriticError> {
        let req = self.build_critique_request(plan, result, goal);
        let agent_result = self.clone().execute(ctx, req).await?;
        let json_text = match agent_result.output {
            AgentOutput::Text { text } => text,
            other => {
                return Err(CriticError::UnexpectedOutput(format!(
                    "expected JSON verdict; got {other:?}"
                )));
            }
        };
        Self::parse_verdict_response(&json_text, &self.id, result.step_id)
    }

    /// Lower-level: parse the JSON the Critic emitted into a typed
    /// [`AgentMessage::Verdict`]. Exposed `pub` so the orchestration
    /// loop can drive trajectory-logged execution via
    /// [`crate::execute_agent_step`] and parse the result here.
    pub fn parse_verdict_response(
        json_text: &str,
        from_agent: &AgentId,
        target_step_id: Option<&str>,
    ) -> Result<AgentMessage, CriticError> {
        let raw: RawVerdict = serde_json::from_str(json_text).map_err(CriticError::Decode)?;
        let verdict = raw.into_verdict_kind()?;
        Ok(AgentMessage::Verdict {
            from_agent: from_agent.clone(),
            target_step_id: target_step_id.map(str::to_string),
            verdict,
        })
    }

    /// Exposed `pub` so the orchestration loop can drive
    /// trajectory-logged execution via [`crate::execute_agent_step`];
    /// integration tests use it to inspect what we'd send without
    /// invoking an LLM.
    pub fn build_critique_request(
        &self,
        plan: &Plan,
        result: &PartialResultRef<'_>,
        goal: &str,
    ) -> ChatRequest {
        let user_payload = serde_json::json!({
            "goal": goal,
            "plan": plan,
            "step_under_review": result.step_id,
            "worker_summary": result.summary,
            "worker_confidence": result.confidence,
        });
        let user_text = serde_json::to_string_pretty(&user_payload)
            .expect("JSON encoding of plan/result is infallible for valid types");

        let mut req = ChatRequest::user(ModelHint::Explicit(self.model.clone()), user_text);
        req.system = Some(CRITIC_SYSTEM_PROMPT.to_string());
        req.structured_output = Some(JsonSchema::strict("Verdict", verdict_json_schema()));
        // Critic must be deterministic given the same inputs — cache /
        // replay both rely on it.
        req.temperature = Some(0.0);
        req
    }
}

#[async_trait]
impl Agent for CriticAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn role(&self) -> AgentRole {
        AgentRole::Critic
    }

    async fn execute(
        self: Arc<Self>,
        ctx: AgentContext,
        input: ChatRequest,
    ) -> Result<AgentStepResult, AgentError> {
        // Same shape as OrchestratorAgent — pass through to the
        // shared drive_llm_call helper. The typed parsing happens in
        // `critique()` above.
        crate::agent::drive_llm_call(ctx, input).await
    }
}

// ── Inputs ─────────────────────────────────────────────────────────────

/// Borrowed view of a [`AgentMessage::PartialResult`] for `critique()`.
/// Avoids moving an owned PartialResult into the Critic when the
/// caller usually still wants it (for the trajectory log, the
/// orchestrator's history, etc.).
#[derive(Clone, Copy, Debug)]
pub struct PartialResultRef<'a> {
    pub step_id: Option<&'a str>,
    pub summary: &'a str,
    pub confidence: f32,
}

impl<'a> PartialResultRef<'a> {
    /// Construct from an owned [`AgentMessage::PartialResult`] variant.
    /// Returns `None` for any other AgentMessage variant — the Critic
    /// expects PartialResult input.
    pub fn from_message(msg: &'a AgentMessage) -> Option<Self> {
        match msg {
            AgentMessage::PartialResult { step_id, summary, confidence, .. } => Some(Self {
                step_id: step_id.as_deref(),
                summary,
                confidence: *confidence,
            }),
            _ => None,
        }
    }
}

// ── Wire format → typed VerdictKind ────────────────────────────────────

/// Flat shape we ask the LLM to emit. Mapped to the typed
/// [`VerdictKind`] enum in `into_verdict_kind`. This split keeps the
/// JSON schema simple (no oneOf at the root) so OpenAI strict mode
/// handles it cleanly across providers.
#[derive(Debug, Deserialize, Serialize)]
struct RawVerdict {
    /// `"approve"` / `"reject"` / `"refine"`. Validated below.
    kind: String,
    /// Required by the schema (so all providers accept the shape);
    /// expected to be empty unless `kind == "reject"`.
    reason: String,
    /// Required by the schema; expected to be empty unless
    /// `kind == "refine"`.
    suggestions: Vec<String>,
}

impl RawVerdict {
    fn into_verdict_kind(self) -> Result<VerdictKind, CriticError> {
        match self.kind.as_str() {
            "approve" => Ok(VerdictKind::Approve),
            "reject" => {
                if self.reason.is_empty() {
                    return Err(CriticError::InvalidVerdict(
                        "kind=reject but reason is empty".into(),
                    ));
                }
                Ok(VerdictKind::Reject { reason: self.reason })
            }
            "refine" => {
                if self.suggestions.is_empty() {
                    return Err(CriticError::InvalidVerdict(
                        "kind=refine but suggestions list is empty".into(),
                    ));
                }
                Ok(VerdictKind::Refine { suggestions: self.suggestions })
            }
            other => Err(CriticError::InvalidVerdict(format!(
                "unknown verdict kind `{other}` (expected approve / reject / refine)"
            ))),
        }
    }
}

// ── Errors ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum CriticError {
    /// Underlying LLM call failed.
    #[error("agent: {0}")]
    Agent(#[from] AgentError),
    /// Model returned text that didn't parse as the verdict shape.
    #[error("decode: {0}")]
    Decode(serde_json::Error),
    /// Model returned tool calls or empty output instead of the JSON
    /// verdict.
    #[error("unexpected output: {0}")]
    UnexpectedOutput(String),
    /// Decoded shape was structurally valid but semantically broken
    /// (e.g. `kind=reject` with no `reason`).
    #[error("invalid verdict: {0}")]
    InvalidVerdict(String),
}

// ── Prompt + schema ────────────────────────────────────────────────────

const CRITIC_SYSTEM_PROMPT: &str = "\
You are a strict but constructive Critic. Your job is to review one Worker's output \
against the original goal and the plan it was part of, then issue ONE of three verdicts:

  - approve — the work is good enough; the orchestration loop moves on.
  - reject  — the work fundamentally fails the goal; the orchestration loop should \
              replan this step from scratch. You MUST give a `reason`.
  - refine  — the work is on the right track but needs specific improvements; the \
              orchestration loop re-runs with your `suggestions` as extra context. \
              You MUST give at least one `suggestion`.

Output rules:
  - Respond with JSON only, matching the schema; do NOT include any prose.
  - `kind` is one of `approve` / `reject` / `refine`.
  - `reason` is a single sentence; required when `kind=reject`, empty string otherwise.
  - `suggestions` is a list of short, concrete improvement items (each item is a \
    full sentence describing one concrete change); required non-empty when \
    `kind=refine`, empty list otherwise.
  - When uncertain between approve and refine, prefer refine with a clear suggestion.";

/// JSON schema for [`RawVerdict`]. Flat object with all fields
/// required (OpenAI strict mode requirement) so providers can
/// enforce the shape without `oneOf` gymnastics.
fn verdict_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["approve", "reject", "refine"],
                "description": "Which verdict variant this is."
            },
            "reason": {
                "type": "string",
                "description": "Required when kind=reject; empty string otherwise."
            },
            "suggestions": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Required non-empty when kind=refine; empty list otherwise."
            }
        },
        "required": ["kind", "reason", "suggestions"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::orchestrator::PlanStep;

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

    fn sample_partial_result_msg() -> AgentMessage {
        AgentMessage::PartialResult {
            from_agent: AgentId::new("worker:summarise"),
            step_id: Some("s1".into()),
            summary: "It changed thing X.".into(),
            confidence: 0.6,
        }
    }

    // ── PartialResultRef extraction ─────────────────────────────────

    #[test]
    fn partial_result_ref_extracts_from_message() {
        let msg = sample_partial_result_msg();
        let r = PartialResultRef::from_message(&msg).unwrap();
        assert_eq!(r.step_id, Some("s1"));
        assert_eq!(r.summary, "It changed thing X.");
        assert!((r.confidence - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn partial_result_ref_returns_none_for_other_message_types() {
        let msg = AgentMessage::PlanIssued { plan: sample_plan() };
        assert!(PartialResultRef::from_message(&msg).is_none());
    }

    // ── RawVerdict → VerdictKind mapping ────────────────────────────

    #[test]
    fn raw_verdict_approve_maps_cleanly() {
        let raw = RawVerdict {
            kind: "approve".into(),
            reason: String::new(),
            suggestions: vec![],
        };
        assert!(matches!(raw.into_verdict_kind().unwrap(), VerdictKind::Approve));
    }

    #[test]
    fn raw_verdict_reject_requires_reason() {
        let raw = RawVerdict {
            kind: "reject".into(),
            reason: String::new(),
            suggestions: vec![],
        };
        match raw.into_verdict_kind() {
            Err(CriticError::InvalidVerdict(msg)) => {
                assert!(msg.contains("reject") && msg.contains("reason"));
            }
            other => panic!("expected InvalidVerdict, got {other:?}"),
        }
        // With reason populated, it works.
        let raw = RawVerdict {
            kind: "reject".into(),
            reason: "too vague".into(),
            suggestions: vec![],
        };
        match raw.into_verdict_kind().unwrap() {
            VerdictKind::Reject { reason } => assert_eq!(reason, "too vague"),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn raw_verdict_refine_requires_non_empty_suggestions() {
        let raw = RawVerdict {
            kind: "refine".into(),
            reason: String::new(),
            suggestions: vec![],
        };
        match raw.into_verdict_kind() {
            Err(CriticError::InvalidVerdict(msg)) => {
                assert!(msg.contains("refine") && msg.contains("suggestion"));
            }
            other => panic!("expected InvalidVerdict, got {other:?}"),
        }
        let raw = RawVerdict {
            kind: "refine".into(),
            reason: String::new(),
            suggestions: vec!["add an example".into()],
        };
        match raw.into_verdict_kind().unwrap() {
            VerdictKind::Refine { suggestions } => {
                assert_eq!(suggestions, vec!["add an example".to_string()]);
            }
            other => panic!("expected Refine, got {other:?}"),
        }
    }

    #[test]
    fn raw_verdict_unknown_kind_errors() {
        let raw = RawVerdict {
            kind: "explode".into(),
            reason: String::new(),
            suggestions: vec![],
        };
        match raw.into_verdict_kind() {
            Err(CriticError::InvalidVerdict(msg)) => {
                assert!(msg.contains("explode"));
            }
            other => panic!("expected InvalidVerdict, got {other:?}"),
        }
    }

    // ── Critic request shape ────────────────────────────────────────

    #[test]
    fn build_critique_request_sets_strict_schema_and_temperature() {
        let critic = CriticAgent::new(AgentId::new("critic"), "gpt-4o");
        let plan = sample_plan();
        let msg = sample_partial_result_msg();
        let result = PartialResultRef::from_message(&msg).unwrap();

        let req = critic.build_critique_request(&plan, &result, "summarise PR #42");
        assert_eq!(req.temperature, Some(0.0));
        assert!(req.system.is_some());
        assert!(req.system.as_ref().unwrap().contains("Critic"));
        let schema = req.structured_output.as_ref().expect("structured_output set");
        assert!(schema.strict);
        assert_eq!(schema.name.as_deref(), Some("Verdict"));
        // Schema describes the flat (kind, reason, suggestions) shape.
        let required = schema.schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"kind"));
        assert!(names.contains(&"reason"));
        assert!(names.contains(&"suggestions"));
    }

    #[test]
    fn build_critique_request_payload_includes_plan_and_result() {
        let critic = CriticAgent::new(AgentId::new("critic"), "gpt-4o");
        let plan = sample_plan();
        let msg = sample_partial_result_msg();
        let result = PartialResultRef::from_message(&msg).unwrap();

        let req = critic.build_critique_request(&plan, &result, "summarise PR #42");
        // The user-message text is a JSON-encoded payload with the
        // plan + result + goal embedded — confirm the model sees all
        // the inputs it needs.
        let user_text = req.messages[0].content()[0].as_text().unwrap();
        assert!(user_text.contains("\"goal\""));
        assert!(user_text.contains("summarise PR #42"));
        assert!(user_text.contains("\"plan_id\""));
        assert!(user_text.contains("\"worker_summary\""));
        assert!(user_text.contains("It changed thing X."));
        assert!(user_text.contains("\"worker_confidence\""));
    }

    #[test]
    fn critic_role_and_id() {
        let c = CriticAgent::new(AgentId::new("critic_a"), "gpt-4o");
        assert_eq!(c.id().as_ref(), "critic_a");
        assert!(matches!(c.role(), AgentRole::Critic));
    }

    // ── Pin the wire-format keys the LLM sees ───────────────────────

    #[test]
    fn schema_includes_enum_constraint_on_kind() {
        let schema = verdict_json_schema();
        let kind_enum = schema["properties"]["kind"]["enum"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = kind_enum.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(names, vec!["approve", "reject", "refine"]);
    }

    #[test]
    fn schema_marks_all_top_level_fields_required() {
        // OpenAI strict mode requires every property to be in
        // `required`; pin that so a future schema edit doesn't
        // silently regress.
        let schema = verdict_json_schema();
        let required = schema["required"].as_array().unwrap();
        let properties = schema["properties"].as_object().unwrap();
        for prop_name in properties.keys() {
            assert!(
                required.iter().any(|r| r.as_str() == Some(prop_name.as_str())),
                "property `{prop_name}` is in `properties` but not in `required` — \
                 OpenAI strict mode will reject this schema",
            );
        }
        // additionalProperties: false also required by strict mode.
        assert_eq!(schema["additionalProperties"], json!(false));
    }
}
