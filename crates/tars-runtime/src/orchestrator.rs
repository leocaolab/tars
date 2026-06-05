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
    /// Optional gating predicate: skip this step at runtime when the
    /// predicate evaluates false against the deps' results. The
    /// default ([`StepCondition::Always`]) reproduces the historical
    /// "every step runs" behaviour, so existing plan JSON that
    /// doesn't carry a `condition` field deserialises identically.
    /// See [`StepCondition`] for the predicate surface; the executor
    /// in [`crate::task::run_task`] checks this before scheduling the
    /// step into a level batch.
    #[serde(default)]
    pub condition: StepCondition,
}

/// Conditional gate on a [`PlanStep`]. When the predicate evaluates
/// false against this step's deps' results, the executor skips the
/// step (no Worker / Critic LLM calls) and emits an
/// [`crate::event::AgentEvent::StepSkipped`] for forensics. Skipping
/// cascades: any step depending on a skipped step is itself skipped
/// with reason "dep `X` was skipped".
///
/// ## Predicate surface — kept narrow on purpose
///
/// `IfDepSummaryContains` only — substring match on a single dep's
/// [`crate::message::AgentMessage::PartialResult::summary`]. The shape
/// is one tag + two strings, which is the smallest schema an LLM
/// reliably emits without escaping mishaps. Future variants (regex,
/// numeric thresholds on `confidence`, JSON pointer into the result
/// body) extend the enum without breaking the JSON shape — clients
/// match on `kind` and ignore variants they don't recognise via
/// `#[serde(other)]` on a future fallback (not present yet because
/// only one variant has a payload today).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepCondition {
    /// Always run. Default — omit `condition` from the plan JSON to
    /// get this behaviour.
    #[default]
    Always,
    /// Run iff `if_dep`'s `PartialResult.summary` contains the
    /// `contains` substring (case-sensitive). Use this to gate a
    /// step on a previous step's textual outcome — e.g. an "auto-fix"
    /// step that only runs when the upstream check reports failure.
    ///
    /// `if_dep` MUST appear in this step's `depends_on` list;
    /// `Plan::validate` rejects plans that violate this so a missing
    /// dep can't silently flip the gate. Missing-dep semantics at
    /// evaluation time would be ambiguous (skip? force-run?
    /// error?), so we lock it down at the plan-validation boundary.
    IfDepSummaryContains {
        /// Dep step id whose result drives the decision. Must be in
        /// `depends_on`.
        if_dep: String,
        /// Case-sensitive substring matched against the dep's
        /// `PartialResult.summary`. Empty string matches every
        /// completed dep (degenerate but valid — same effect as
        /// `Always` when the dep ran).
        contains: String,
    },
}

impl StepCondition {
    /// Evaluate the predicate against the completed steps' results.
    /// Returns `true` ⇒ step should run, `false` ⇒ step is skipped.
    /// Pre-condition: `Plan::validate` already established that
    /// `if_dep` is in this step's `depends_on`, so the lookup either
    /// finds a `PartialResult` or the dep itself was skipped — and a
    /// skipped dep is detected by the caller (the executor) BEFORE
    /// this method is invoked (skip-cascade), so a missing key here
    /// is genuinely an executor bug, not a plan-shape issue. Return
    /// `false` defensively in that case rather than panicking, so a
    /// future refactor can't accidentally execute work with stale
    /// dep state.
    pub fn matches(
        &self,
        completed: &std::collections::HashMap<String, crate::message::AgentMessage>,
    ) -> bool {
        match self {
            Self::Always => true,
            Self::IfDepSummaryContains { if_dep, contains } => {
                let Some(msg) = completed.get(if_dep) else {
                    tracing::warn!(
                        if_dep = %if_dep,
                        "StepCondition::matches: dep not in completed map — \
                         executor should have detected skip-cascade first; \
                         falling through to `false` (skip).",
                    );
                    return false;
                };
                match msg {
                    crate::message::AgentMessage::PartialResult { summary, .. } => {
                        summary.contains(contains)
                    }
                    // Non-PartialResult shapes shouldn't reach here in
                    // normal flow (workers always emit PartialResult);
                    // treat as "no signal", skip.
                    other => {
                        tracing::warn!(
                            if_dep = %if_dep,
                            shape = ?std::mem::discriminant(other),
                            "StepCondition::matches: dep produced a non-PartialResult \
                             message; treating predicate as false (skip).",
                        );
                        false
                    }
                }
            }
        }
    }

    /// Short human-readable description for the
    /// [`crate::event::AgentEvent::StepSkipped::reason`] field.
    /// Format: `"condition not met (if_dep=`X`, contains=`Y`)"`.
    pub fn skip_reason(&self) -> String {
        match self {
            Self::Always => "always (unreachable: Always never produces a skip)".into(),
            Self::IfDepSummaryContains { if_dep, contains } => {
                format!("condition not met (if_dep=`{if_dep}`, contains=`{contains}`)")
            }
        }
    }
}

impl Plan {
    /// Group `steps` into dependency depth-levels for parallel
    /// execution. Level 0 = no deps (roots). Level N = steps whose
    /// deepest dep is at level N-1. Steps in the same level have no
    /// dep on each other and can run concurrently.
    ///
    /// **Pre-condition**: `validate()` must succeed first. Without that,
    /// the depth computation may panic on missing-dep lookups or
    /// silently return wrong levels on a cycle. (`validate()` already
    /// rules out cycles by requiring deps to point at earlier-declared
    /// steps — a topological order is implicit, so no explicit cycle
    /// detection is needed here.)
    pub fn execution_levels(&self) -> Vec<Vec<&PlanStep>> {
        use std::collections::HashMap;
        // depths: step.id → 0 (no deps) or 1 + max(dep depths).
        // Because deps point at earlier steps, by the time we reach
        // step S every dep's depth is already in the map.
        let mut depths: HashMap<&str, usize> = HashMap::with_capacity(self.steps.len());
        for step in &self.steps {
            let d = step
                .depends_on
                .iter()
                .map(|dep| {
                    depths.get(dep.as_str()).copied().unwrap_or_else(|| {
                        // validate() ruled this out; the unreachable
                        // path defends against a future caller skipping
                        // validation.
                        panic!(
                            "execution_levels: step `{}` depends on unknown `{}` — call validate() first",
                            step.id, dep,
                        )
                    }) + 1
                })
                .max()
                .unwrap_or(0);
            depths.insert(step.id.as_str(), d);
        }
        // Group steps by depth, preserving declaration order within
        // each level (callers may want stable batch composition for
        // logging / replay).
        let max_depth = depths.values().copied().max().unwrap_or(0);
        let mut levels: Vec<Vec<&PlanStep>> = (0..=max_depth).map(|_| Vec::new()).collect();
        for step in &self.steps {
            let d = depths[step.id.as_str()];
            levels[d].push(step);
        }
        levels
    }

    /// True iff the dependency graph is well-formed: every id is
    /// unique, every `depends_on` references a known step, and no
    /// step depends on itself or on a later-declared step (cycles
    /// would be a separate check; we keep it simple).
    pub fn validate(&self) -> Result<(), OrchestratorError> {
        use std::collections::HashSet;
        if self.plan_id.trim().is_empty() {
            return Err(OrchestratorError::InvalidPlan("plan_id is empty".into()));
        }
        if self.goal.trim().is_empty() {
            return Err(OrchestratorError::InvalidPlan("goal is empty".into()));
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.steps.len());
        for step in &self.steps {
            if step.id.trim().is_empty() {
                return Err(OrchestratorError::InvalidPlan("step id is empty".into()));
            }
            if step.worker_role.trim().is_empty() {
                return Err(OrchestratorError::InvalidPlan(format!(
                    "step `{}` has empty worker_role",
                    step.id,
                )));
            }
            if step.instruction.trim().is_empty() {
                return Err(OrchestratorError::InvalidPlan(format!(
                    "step `{}` has empty instruction",
                    step.id,
                )));
            }
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
            // Condition's `if_dep` must be a declared dep — otherwise
            // the predicate would read from a step that hasn't
            // necessarily completed (or might not even be in the plan)
            // by the time we evaluate it. Catching this at the plan
            // boundary turns a runtime ambiguity ("skip? force-run?
            // error?") into a parse-time refusal.
            if let StepCondition::IfDepSummaryContains { if_dep, .. } = &step.condition
                && !step.depends_on.iter().any(|d| d == if_dep)
            {
                return Err(OrchestratorError::InvalidPlan(format!(
                    "step `{}`: condition.if_dep = `{}` is not in depends_on (= {:?}); \
                     the predicate would read from a step that may not have completed",
                    step.id, if_dep, step.depends_on,
                )));
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
        let result = Arc::clone(&self).execute(ctx, req).await?;
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

    /// Construct a *replan* ChatRequest: the Critic rejected a step of
    /// the prior plan, so the orchestrator gets another shot with all
    /// the context from the failed attempt in hand.
    ///
    /// The payload is structured (not free-form prose) so the
    /// orchestrator's training can pattern-match on it:
    ///
    ///   - `goal` — the original user goal, unchanged across replans.
    ///   - `previous_plan` — the plan that just failed, in full.
    ///   - `previous_results` — which steps had already produced
    ///     accepted PartialResults before the rejection; the new plan
    ///     can incorporate (or redo) the work they represent.
    ///   - `rejected_step` + `reject_reason` — exact failure site and
    ///     the Critic's verbatim reason, so the planner doesn't
    ///     re-propose the same shape.
    ///   - `replan_attempt` — N-th replan (1-based), so the planner
    ///     can escalate strategy under repeated failure (e.g. fall
    ///     back to a simpler / more conservative shape on attempt 3+).
    ///
    /// The system prompt for replanning is the same as for a fresh
    /// plan (`PLANNER_SYSTEM_PROMPT`); we don't want the planner
    /// behaving differently in replan vs fresh — the context block
    /// is what carries the difference.
    pub fn build_replanner_request(
        &self,
        goal: &str,
        previous_plan: &Plan,
        previous_results: &std::collections::HashMap<String, crate::message::AgentMessage>,
        rejected_step: &str,
        reject_reason: &str,
        replan_attempt: u32,
    ) -> ChatRequest {
        let prior_results_json: serde_json::Map<String, serde_json::Value> = previous_results
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    serde_json::to_value(v).unwrap_or_else(
                        |e| serde_json::json!({"error": format!("encode prior result: {e}")}),
                    ),
                )
            })
            .collect();
        let payload = serde_json::json!({
            "goal": goal,
            "previous_plan": previous_plan,
            "previous_results": prior_results_json,
            "rejected_step": rejected_step,
            "reject_reason": reject_reason,
            "replan_attempt": replan_attempt,
            "instruction": format!(
                "The previous plan's step `{rejected_step}` was rejected by the Critic. \
                 Produce a NEW plan that addresses the rejection. You may reuse step ids \
                 from the previous plan ONLY if their work is unchanged (downstream \
                 callers will re-execute them — there is no checkpoint reuse). Prefer \
                 a different decomposition over re-trying the same step shape verbatim."
            ),
        });
        let user_text = serde_json::to_string_pretty(&payload)
            .expect("JSON encoding of replan context is infallible");
        PromptBuilder::new(self.model.clone(), user_text)
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
  - `condition` (OPTIONAL — omit for the common 'always run' case): a gating
    predicate evaluated against a dep's result at runtime. If the predicate
    is false, this step is SKIPPED (no LLM calls) and any step that depends
    on it is also skipped (cascade). Only one shape today:
    `{ \"kind\": \"if_dep_summary_contains\", \"if_dep\": \"<dep id>\", \"contains\": \"<substring>\" }`
    Use this for branching: e.g. an 'auto-fix' step that should only run
    when an upstream 'check' step's summary contains the word 'failed'.
    Constraint: `if_dep` MUST be in this step's `depends_on`.

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
            "plan_id": { "type": "string", "minLength": 1 },
            "goal":    { "type": "string", "minLength": 1 },
            "steps": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "id":          { "type": "string", "minLength": 1 },
                        "worker_role": { "type": "string", "minLength": 1 },
                        "instruction": { "type": "string", "minLength": 1 },
                        "depends_on": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "condition": {
                            // oneOf so the LLM sees the discriminated-union
                            // shape directly — `kind` picks the variant,
                            // each variant's payload is rigid. Optional at
                            // the top level (not in `required`) because
                            // omitting it is the dominant case.
                            "oneOf": [
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "kind": { "const": "always" }
                                    },
                                    "required": ["kind"]
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "kind":     { "const": "if_dep_summary_contains" },
                                        "if_dep":   { "type": "string", "minLength": 1 },
                                        "contains": { "type": "string" }
                                    },
                                    "required": ["kind", "if_dep", "contains"]
                                }
                            ]
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
                    condition: StepCondition::Always,
                },
                PlanStep {
                    id: "s2".into(),
                    worker_role: "summarise".into(),
                    instruction: "summarise the diff for a non-engineer".into(),
                    depends_on: vec!["s1".into()],
                    condition: StepCondition::Always,
                },
            ],
        }
    }

    /// `execution_levels` groups deps-free steps into level 0, every
    /// step whose deepest dep is at level N-1 into level N, etc.
    /// Within a level steps are sibling (independent); across levels
    /// they're sequential. This is the contract the DAG executor in
    /// task.rs relies on for its `for level in plan.execution_levels()`
    /// → `join_all(level)` shape.
    #[test]
    fn execution_levels_groups_diamond_fanout_into_three_layers() {
        // diamond:
        //     s1 ──┐
        //          ├─→ s4
        //     s2 ──┤
        //          └─→ s5
        //     s3 ──┘
        //                  s4 + s5 → s6 (the merge)
        let plan = Plan {
            plan_id: "diamond".into(),
            goal: "fan out then merge".into(),
            steps: vec![
                step("s1", "a", vec![]),
                step("s2", "a", vec![]),
                step("s3", "a", vec![]),
                step("s4", "a", vec!["s1", "s2"]),
                step("s5", "a", vec!["s2", "s3"]),
                step("s6", "a", vec!["s4", "s5"]),
            ],
        };
        plan.validate().expect("diamond is well-formed");
        let levels = plan.execution_levels();
        assert_eq!(levels.len(), 3, "diamond has depths 0, 1, 2");
        fn ids<'a>(lv: &[&'a PlanStep]) -> Vec<&'a str> {
            lv.iter().map(|s| s.id.as_str()).collect()
        }
        assert_eq!(ids(&levels[0]), vec!["s1", "s2", "s3"]);
        assert_eq!(ids(&levels[1]), vec!["s4", "s5"]);
        assert_eq!(ids(&levels[2]), vec!["s6"]);
    }

    #[test]
    fn execution_levels_handles_pure_serial_chain_as_one_step_per_level() {
        // Pure chain s1 → s2 → s3 → s4: each step its own level.
        let plan = Plan {
            plan_id: "chain".into(),
            goal: "linear".into(),
            steps: vec![
                step("s1", "a", vec![]),
                step("s2", "a", vec!["s1"]),
                step("s3", "a", vec!["s2"]),
                step("s4", "a", vec!["s3"]),
            ],
        };
        plan.validate().unwrap();
        let levels = plan.execution_levels();
        assert_eq!(levels.len(), 4, "chain has 4 distinct depths");
        for (i, lv) in levels.iter().enumerate() {
            assert_eq!(lv.len(), 1, "chain level {i} should have exactly 1 step");
        }
    }

    #[test]
    fn execution_levels_handles_all_independent_steps_as_a_single_wide_level() {
        let plan = Plan {
            plan_id: "wide".into(),
            goal: "all independent".into(),
            steps: vec![
                step("s1", "a", vec![]),
                step("s2", "a", vec![]),
                step("s3", "a", vec![]),
            ],
        };
        plan.validate().unwrap();
        let levels = plan.execution_levels();
        assert_eq!(levels.len(), 1, "all roots → one level");
        assert_eq!(levels[0].len(), 3);
    }

    fn step(id: &str, role: &str, deps: Vec<&str>) -> PlanStep {
        PlanStep {
            id: id.into(),
            worker_role: role.into(),
            instruction: format!("do {id}"),
            depends_on: deps.into_iter().map(String::from).collect(),
            condition: StepCondition::Always,
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

    // ── StepCondition ────────────────────────────────────────────────

    #[test]
    fn step_condition_default_is_always() {
        // Defaulting to Always is the "no gating" contract that keeps
        // existing plan JSON (no `condition` field) deserializing
        // unchanged. If a future refactor flips the default, every
        // plan without an explicit `condition` would silently start
        // skipping — this test is the canary.
        let c: StepCondition = Default::default();
        assert_eq!(c, StepCondition::Always);
    }

    #[test]
    fn step_condition_omitted_field_deserialises_as_always() {
        // The wire-level contract: a plan-step JSON that doesn't
        // mention `condition` at all still parses. Without
        // `#[serde(default)]` on the field this would error out.
        let step_json = serde_json::json!({
            "id": "s1",
            "worker_role": "summarise",
            "instruction": "do it",
            "depends_on": [],
        });
        let step: PlanStep = serde_json::from_value(step_json).unwrap();
        assert_eq!(step.condition, StepCondition::Always);
    }

    #[test]
    fn step_condition_substring_predicate_matches_when_summary_contains() {
        use crate::message::AgentMessage;
        let mut completed = std::collections::HashMap::new();
        completed.insert(
            "s1".to_string(),
            AgentMessage::PartialResult {
                from_agent: AgentId::new("w"),
                step_id: Some("s1".into()),
                summary: "lint reported: failed".into(),
                confidence: 0.9,
            },
        );
        let c = StepCondition::IfDepSummaryContains {
            if_dep: "s1".into(),
            contains: "failed".into(),
        };
        assert!(c.matches(&completed));
    }

    #[test]
    fn step_condition_substring_predicate_does_not_match_when_summary_lacks_substring() {
        use crate::message::AgentMessage;
        let mut completed = std::collections::HashMap::new();
        completed.insert(
            "s1".to_string(),
            AgentMessage::PartialResult {
                from_agent: AgentId::new("w"),
                step_id: Some("s1".into()),
                summary: "lint reported: clean".into(),
                confidence: 0.9,
            },
        );
        let c = StepCondition::IfDepSummaryContains {
            if_dep: "s1".into(),
            contains: "failed".into(),
        };
        assert!(!c.matches(&completed));
    }

    #[test]
    fn step_condition_substring_predicate_skips_when_dep_missing() {
        // Missing dep ⇒ matches() returns false (defensive: skip
        // rather than panic). The executor's skip-cascade pass
        // should normally prevent this state by detecting a skipped
        // dep first, but matches() shouldn't crash on it.
        let completed: std::collections::HashMap<String, crate::message::AgentMessage> =
            std::collections::HashMap::new();
        let c = StepCondition::IfDepSummaryContains {
            if_dep: "s1".into(),
            contains: "failed".into(),
        };
        assert!(!c.matches(&completed));
    }

    #[test]
    fn validate_rejects_condition_with_unknown_dep() {
        // Plan-validation refuses a step whose `condition.if_dep`
        // isn't in `depends_on` — runtime ambiguity ("read from a
        // step that may not have completed") becomes a parse-time
        // error.
        let mut p = sample_plan();
        p.steps[1].condition = StepCondition::IfDepSummaryContains {
            if_dep: "not_a_dep".into(),
            contains: "x".into(),
        };
        match p.validate() {
            Err(OrchestratorError::InvalidPlan(msg)) => {
                assert!(msg.contains("not_a_dep"));
                assert!(msg.contains("depends_on"));
            }
            other => panic!("expected InvalidPlan, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_condition_with_dep_in_depends_on() {
        let mut p = sample_plan();
        p.steps[1].condition = StepCondition::IfDepSummaryContains {
            if_dep: "s1".into(), // s1 IS in s2's depends_on
            contains: "x".into(),
        };
        assert!(p.validate().is_ok());
    }

    #[test]
    fn plan_with_condition_round_trips_through_json() {
        let mut p = sample_plan();
        p.steps[1].condition = StepCondition::IfDepSummaryContains {
            if_dep: "s1".into(),
            contains: "failed".into(),
        };
        let v = serde_json::to_value(&p).unwrap();
        // Wire shape: condition.kind = "if_dep_summary_contains".
        assert_eq!(
            v["steps"][1]["condition"]["kind"],
            json!("if_dep_summary_contains"),
        );
        let back: Plan = serde_json::from_value(v).unwrap();
        assert_eq!(back.steps[1].condition, p.steps[1].condition);
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
