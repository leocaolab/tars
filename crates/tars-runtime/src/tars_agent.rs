//! [`TarsAgent`] — the LLM-backed implementer of [`tars_model::Agent`].
//!
//! THIS is the "native agent" of Doc 20: you hand it a [`Task`], and it
//! internally turns the task into an LLM prompt and drives a tool loop over
//! a pure-inference provider — white-box (tars owns the loop + tools). The
//! same `TarsAgent` over a `gemini` provider is a "gemini agent"; over
//! `claude_cli` (Disabled tools = pure inference) it's a "claude_cli agent".
//!
//! Implementation: it reuses the existing [`WorkerAgent`] tool loop (the
//! one tars-tools registry path), wrapping it behind the task-level
//! `Agent::run(task)` contract and threading `ctx.cwd` so the agent acts on
//! its worktree. The `Task → synthetic one-step Plan` shaping is option A
//! of Doc 21 §3 — chosen for reuse; folds into a dedicated loop later.
//!
//! Known limitation (Doc 21): `WorkerAgent` parses the model's FINAL turn
//! as a `{summary, confidence}` worker result, so a native agent's last
//! message must be that shape. Fine for "do X and report"; a freer output
//! contract is a follow-on.

use std::sync::Arc;

use async_trait::async_trait;

use tars_model::{
    Agent, AgentContext, AgentError, AgentId, AgentOutput, AgentRole, SkillSet, Task,
};
use tars_pipeline::LlmService;
use tars_tools::ToolRegistry;
use tars_types::TrajectoryId;

use crate::agent::AgentContext as StepContext;
use crate::message::AgentMessage;
use crate::orchestrator::{Plan, PlanStep, StepCondition};
use crate::worker::WorkerAgent;

/// An LLM-backed [`Agent`]: a [`SkillSet`] (backed by a tars-tools
/// `ToolRegistry`) driven over a pure-inference provider.
pub struct TarsAgent {
    id: AgentId,
    role: AgentRole,
    skills: SkillSet,
    /// The pure-inference provider, pipeline-wrapped. Swapping this is what
    /// makes a "gemini agent" vs a "claude_cli agent".
    llm: Arc<dyn LlmService>,
    /// The inner tool loop (tars-tools registry path).
    worker: Arc<WorkerAgent>,
}

impl TarsAgent {
    /// Assemble a native agent.
    ///
    /// - `id` / `domain` — identity + the worker domain (its [`AgentRole`]).
    /// - `skills` — what it advertises it can do (should correspond to the
    ///   tools in `tools`).
    /// - `model` — the model id the provider serves.
    /// - `llm` — the pure-inference provider, pipeline-wrapped.
    /// - `tools` — the concrete capabilities (e.g. read/write/edit/bash).
    pub fn new(
        id: impl Into<String>,
        domain: impl Into<String>,
        skills: SkillSet,
        model: impl Into<String>,
        llm: Arc<dyn LlmService>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        let id = id.into();
        let domain = domain.into();
        let worker = WorkerAgent::with_tools(id.clone(), model, domain.clone(), tools);
        Self {
            id: AgentId::new(id),
            role: AgentRole::worker(domain),
            skills,
            llm,
            worker,
        }
    }

    /// Render a [`Task`] into the single-step instruction the worker reads.
    /// Goal first, then any named inputs as a labelled block.
    fn instruction_for(task: &Task) -> String {
        if task.inputs.is_empty() {
            return task.goal.clone();
        }
        let mut s = task.goal.clone();
        s.push_str("\n\nInputs:");
        for input in &task.inputs {
            s.push_str(&format!("\n- {}: {}", input.name, input.value));
        }
        if let Some(acc) = &task.acceptance {
            s.push_str(&format!("\n\nDone when: {acc}"));
        }
        s
    }
}

#[async_trait]
impl Agent for TarsAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn role(&self) -> &AgentRole {
        &self.role
    }

    fn skills(&self) -> &SkillSet {
        &self.skills
    }

    async fn run(&self, task: Task, ctx: AgentContext) -> Result<AgentOutput, AgentError> {
        // Shape the task into a one-step plan the WorkerAgent can run.
        let plan = Plan {
            plan_id: task.id.to_string(),
            goal: task.goal.clone(),
            steps: vec![PlanStep {
                id: task.id.to_string(),
                worker_role: self.role.kind().to_string(),
                instruction: Self::instruction_for(&task),
                depends_on: vec![],
                condition: StepCondition::Always,
            }],
        };

        // Build the runtime step-context: our provider as the llm, the
        // task's cancel + cwd threaded through (so tools act on the
        // worktree).
        let traj = ctx
            .trajectory_id
            .clone()
            .unwrap_or_else(|| format!("native:{}", self.id));
        let step_ctx = StepContext {
            trajectory_id: TrajectoryId::new(traj),
            step_seq: 1,
            llm: self.llm.clone(),
            cancel: ctx.cancel.clone(),
            cwd: ctx.cwd.clone(),
            permissions: ctx.permissions.clone(),
            readable_roots: ctx.readable_roots.clone(),
        };

        let msg = self
            .worker
            .clone()
            .execute_step(step_ctx, &plan, &plan.steps[0], &[])
            .await
            .map_err(|e| AgentError::Execution(e.to_string()))?;

        match msg {
            AgentMessage::PartialResult {
                summary,
                confidence,
                ..
            } => Ok(AgentOutput::new(summary)
                .with_data(serde_json::json!({ "confidence": confidence }))),
            other => Err(AgentError::Execution(format!(
                "native agent produced a non-result message: {other:?}"
            ))),
        }
    }
}
