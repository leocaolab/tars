//! [`TarsAgent`] — the LLM-backed implementer of [`tars_agent::Agent`].
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

use tars_agent::{
    Agent, AgentContext, TaskError, AgentId, AgentOutput, AgentRole, SkillSet, Task,
};
use tars_pipeline::LlmService;
use tars_tools::ToolRegistry;
use tars_types::TrajectoryId;

use std::collections::HashMap;

use crate::agent::{Agent as StepAgent, AgentContext as StepContext};
use crate::executor::{Worker, WorkerContext, WorkerOutput};
use crate::message::AgentMessage;
use crate::orchestrator::{Plan, PlanStep, StepCondition};
use crate::runtime::execute_agent_step;
use crate::worker::{WorkerAgent, WorkerError};

/// An LLM-backed [`Agent`]: a [`SkillSet`] (backed by a tars-tools
/// `ToolRegistry`) driven over a pure-inference provider.
///
/// `Clone` is cheap — `id`/`role`/`skills` are small value types and
/// `llm`/`worker` are `Arc`s (the clone shares the provider + tool loop).
/// It lets a domain agent set a PER-CALL [`WorkerPersona`] (e.g. a critic
/// whose system prompt varies by rubric / L4-vs-L5 / stateful) by cloning
/// then `with_persona`, instead of baking one persona at construction.
#[derive(Clone)]
pub struct TarsAgent {
    id: AgentId,
    role: AgentRole,
    skills: SkillSet,
    /// The pure-inference provider, pipeline-wrapped. Swapping this is what
    /// makes a "gemini agent" vs a "claude_cli agent".
    llm: LlmService,
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
        llm: LlmService,
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

    /// Set the thinking/reasoning mode the inner worker applies to every
    /// request. Thread this from the provider config: a thinking-ONLY model
    /// (gemini-3.x-pro) rejects the default 0 budget, so without it the agent
    /// can't call such a model at all.
    pub fn with_thinking(mut self, thinking: tars_types::ThinkingMode) -> Self {
        self.worker = self.worker.clone().with_thinking(thinking);
        self
    }

    /// Give the agent a domain [`WorkerPersona`] — its own system prompt and
    /// (optionally) its own structured-output schema, instead of the built-in
    /// worker protocol. This is what makes a TarsAgent a *reviewer* / *verifier*
    /// / *critic* (own persona + verdict schema) rather than a generic
    /// plan-step worker, while still being scheduled as an `Agent` + `Worker`.
    pub fn with_persona(mut self, persona: crate::worker::WorkerPersona) -> Self {
        self.worker = self.worker.clone().with_persona(persona);
        self
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

/// Lift a [`WorkerError`] (runtime layer) to the task-level
/// [`tars_agent::TaskError`] WITHOUT burning the typed provider error.
///
/// The inner worker chain keeps the [`ProviderError`](tars_types::ProviderError)
/// typed all the way up: `ProviderError` → `runtime::StepError::Provider` →
/// `WorkerError::Agent`. When that's the shape, we hand the SAME typed
/// `ProviderError` to `tars_agent::TaskError::Provider`, so a consumer
/// (arc) classes the failure by matching the variant (rate-limit / auth /
/// overloaded) instead of grepping a stringified message. A bare cancel
/// maps to the typed `Cancelled`. Every other `WorkerError`
/// (decode / unexpected output / invalid result / panic / timeout /
/// agent-internal) has no typed provider error to keep — those really are
/// just text, so `Execution(String)` is honest there. `Display` walks the
/// `#[source]` chain, so no double-stringify.
fn worker_error_to_agent_error(e: WorkerError) -> TaskError {
    match e {
        WorkerError::Agent(crate::agent::StepError::Provider(pe)) => TaskError::Provider(pe),
        WorkerError::Agent(crate::agent::StepError::Cancelled) => TaskError::Cancelled,
        other => TaskError::Execution(other.to_string()),
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

    async fn run(&self, task: Task, ctx: AgentContext) -> Result<AgentOutput, TaskError> {
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
            // The native-agent path runs one worker step directly (no executor
            // WorkerContext). Its per-role sandbox would come from the
            // `tars_agent::AgentContext` once that boundary carries a policy;
            // until then it's unconfined (DangerFullAccess), same as before.
            sandbox: tars_tools::SandboxPolicy::default(),
            llm_request_ctx: None,
            stream_hooks: None,
        };

        let msg = self
            .worker
            .clone()
            .execute_step(step_ctx, &plan, &plan.steps[0], &[])
            .await
            .map_err(worker_error_to_agent_error)?;

        match msg {
            AgentMessage::PartialResult {
                summary,
                confidence,
                ..
            } => Ok(AgentOutput::new(summary)
                .with_data(serde_json::json!({ "confidence": confidence }))),
            other => Err(TaskError::Execution(format!(
                "native agent produced a non-result message: {other:?}"
            ))),
        }
    }
}

/// `TarsAgent` is ALSO a DAG [`Worker`], not only a task-level [`Agent`]:
/// the SAME inner `WorkerAgent` tool loop, driven from a [`PlanStep`] instead
/// of a [`Task`]. So one type is both the thing you hand a task to AND the
/// unit the pipeline schedules — no separate per-step worker wrapper.
///
/// Mirrors [`crate::LlmWorker`]: `build_worker_request` (threading
/// `prior_results` + `refinements`) → [`execute_agent_step`] (trajectory
/// logging) → `parse_worker_response`. A blackboard `commit` is the
/// consumer's concern (via `ctx.shared`); a generic agent doesn't know the
/// domain — so this bridge stays commit-free, exactly like `LlmWorker`.
#[async_trait]
impl Worker for TarsAgent {
    async fn run(
        &self,
        plan: &Plan,
        step: &PlanStep,
        prior_results: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let req =
            self.worker
                .build_worker_request(plan, step, &ctx.refinements, prior_results);
        let agent: Arc<dyn StepAgent> = self.worker.clone();
        let result = execute_agent_step(
            ctx.runtime.as_ref(),
            &ctx.trajectory_id,
            self.llm.clone(),
            agent,
            req,
            ctx.cancel.clone(),
            // Worker/fixer confinement (D5/D6) from the executor's WorkerContext.
            ctx.sandbox.clone(),
        )
        .await
        .map_err(|e| match e {
            crate::runtime::AgentExecutionError::Agent(a) => WorkerError::Agent(a),
            crate::runtime::AgentExecutionError::Runtime(r) => {
                WorkerError::InvalidResult(format!("runtime: {r}"))
            }
        })?;
        let json_text = match result.output {
            crate::agent::AgentOutput::Text { text } => text,
            other => {
                return Err(WorkerError::UnexpectedOutput(format!(
                    "expected JSON text from worker; got {other:?}"
                )));
            }
        };
        let message = WorkerAgent::parse_worker_response(
            &json_text,
            self.worker.id(),
            Some(step.id.as_str()),
        )?;
        Ok(WorkerOutput {
            message,
            usage: result.usage,
            created: result.created,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_agent::TaskId;
    use tars_pipeline::{LlmEventStream, LlmService};
    use tars_provider::LlmProvider;
    use tars_types::{
        Capabilities, ChatRequest, Pricing, ProviderError, ProviderErrorKind, ProviderId,
        RequestContext,
    };

    /// A provider that always fails with a TYPED [`ProviderError`] (a
    /// rate-limit) — the shape a real provider raises when throttled.
    /// Bound into an [`LlmService`] at the call site.
    struct RateLimitedLlm {
        id: ProviderId,
        caps: Capabilities,
    }

    #[async_trait]
    impl LlmProvider for RateLimitedLlm {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        async fn stream(
            self: Arc<Self>,
            _req: ChatRequest,
            _model: &str,
            _ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            Err(ProviderError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(30)),
            })
        }
    }

    fn rate_limited_service() -> LlmService {
        LlmService::of(
            Arc::new(RateLimitedLlm {
                id: ProviderId::new("rate_limited"),
                caps: Capabilities::text_only_baseline(Pricing::default()),
            }),
            "test-model",
        )
    }

    /// The load-bearing guarantee: a provider failure inside the inner
    /// worker chain surfaces at the `Agent::run` boundary as a TYPED
    /// [`tars_agent::TaskError::Provider`], carrying the same
    /// [`ProviderError`] — NOT flattened into `Execution(String)`. Proved
    /// by MATCHING the variant + reading `kind()` (typed), never by
    /// grepping the message.
    #[tokio::test]
    async fn provider_error_survives_to_agent_run_as_typed_variant() {
        let agent = TarsAgent::new(
            "t-agent",
            "test",
            SkillSet::new(),
            "test-model",
            rate_limited_service(),
            Arc::new(ToolRegistry::new()),
        );

        let task = Task::new(TaskId::new("t1"), "do a thing");
        let err = Agent::run(&agent, task, AgentContext::new())
            .await
            .expect_err("a rate-limited provider must fail the run");

        match err {
            TaskError::Provider(pe) => {
                // Typed downcast, not a substring match: the retry_after
                // payload survives the whole lift intact.
                assert_eq!(pe.kind(), ProviderErrorKind::RateLimited);
                assert_eq!(pe.retry_after(), Some(std::time::Duration::from_secs(30)));
            }
            other => panic!("expected typed TaskError::Provider, got {other:?}"),
        }
    }

    /// The helper's other leg: a WorkerError with no typed provider error
    /// (a decode failure) is honestly text — `Execution(String)`, not a
    /// fabricated `Provider`.
    #[test]
    fn non_provider_worker_error_maps_to_execution() {
        let e = WorkerError::UnexpectedOutput("model returned tool calls".into());
        match worker_error_to_agent_error(e) {
            TaskError::Execution(_) => {}
            other => panic!("expected Execution for a text-only worker error, got {other:?}"),
        }
    }
}
