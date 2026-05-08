//! [`run_task`] — the multi-step Orchestrator → Worker → Critic loop.
//!
//! This is the **first user-facing M3 milestone**: given a goal, drive
//! a real triad of agents end-to-end with full trajectory logging and
//! Critic-driven retry. Doc 04 §4.2's typed [`AgentMessage`] envelope
//! threads through all the agent boundaries; the loop never falls back
//! to free-form text.
//!
//! ## Loop shape
//!
//! 1. Create a fresh trajectory.
//! 2. Orchestrator → typed [`Plan`] (1 step in the trajectory).
//! 3. For each [`PlanStep`] in declaration order (which the Orchestrator
//!    is required to leave dep-respecting; see [`Plan::validate`]):
//!    - **Attempt 0..=`max_refinements`**:
//!      - Worker → [`AgentMessage::PartialResult`] (1 step).
//!      - Critic → [`AgentMessage::Verdict`] (1 step).
//!      - On `Approve` → record the result, advance to next plan step.
//!      - On `Refine{suggestions}` → loop back with the suggestions
//!        threaded into the next Worker prompt.
//!      - On `Reject{reason}` → write [`AgentEvent::TrajectoryAbandoned`]
//!        and return [`RunTaskError::Rejected`].
//!    - If we exhaust `max_refinements` without an Approve →
//!      [`RunTaskError::RefineExhausted`] (also Abandoned).
//! 4. Write [`AgentEvent::TrajectoryCompleted`], return the
//!    [`TaskOutcome`] with the per-step results.
//!
//! ## Why this is "stub Worker" today
//!
//! [`crate::WorkerAgent`] currently has no tool registry — it only
//! asks the LLM to describe how it would perform the step. The loop's
//! contract doesn't change when B-9 (`tars-tools`) lands and Workers
//! gain real I/O; the same `AgentMessage::PartialResult` flows
//! through.
//!
//! ## Replan on Reject — explicitly deferred
//!
//! Doc 04 §4.2's full design has Reject trigger a **replan** (call the
//! Orchestrator again with the rejection reason as feedback) instead
//! of failing the whole task. We pick the simpler "Reject = task
//! failed" semantics for the first cut so we don't have to design the
//! replan-feedback prompt under time pressure. When a real consumer
//! needs replan, it slots in here.

use std::sync::Arc;

use tars_pipeline::LlmService;
use tars_types::TrajectoryId;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentOutput};
use crate::critic::{CriticAgent, CriticError};
use crate::event::AgentEvent;
use crate::message::{AgentMessage, VerdictKind};
use crate::orchestrator::{OrchestratorAgent, OrchestratorError, Plan, PlanStep};
use crate::runtime::{AgentExecutionError, Runtime, execute_agent_step};
use crate::worker::{WorkerAgent, WorkerError};

/// Tunable knobs. Defaults are conservative — bump for harder tasks.
#[derive(Clone, Debug)]
pub struct RunTaskConfig {
    /// Maximum number of Refine retries per step (NOT counting the
    /// initial attempt). `0` means "Worker gets exactly one chance,
    /// any Refine verdict fails the task".
    pub max_refinements_per_step: u32,
}

impl Default for RunTaskConfig {
    fn default() -> Self {
        Self {
            max_refinements_per_step: 2,
        }
    }
}

/// Per-step record in the [`TaskOutcome`].
#[derive(Clone, Debug)]
pub struct StepOutcome {
    pub step_id: String,
    /// The Worker's final accepted output for this step.
    pub result: AgentMessage,
    /// The Critic's accepting verdict (always `VerdictKind::Approve`
    /// on success).
    pub verdict: AgentMessage,
    /// How many refinement loops were needed before approval.
    /// `0` = approved on first attempt.
    pub refinement_attempts: u32,
}

/// What [`run_task`] returns on success.
#[derive(Clone, Debug)]
pub struct TaskOutcome {
    pub trajectory_id: TrajectoryId,
    pub plan: Plan,
    pub steps: Vec<StepOutcome>,
}

/// Errors [`run_task`] can surface. All variants carry the
/// `trajectory_id` so callers can replay the trajectory log to see
/// what happened. The trajectory itself is left in its terminal
/// state: TrajectoryAbandoned for the failure variants,
/// TrajectoryCompleted for success.
#[derive(Debug, thiserror::Error)]
pub enum RunTaskError {
    #[error("orchestrator: {source}")]
    Orchestrator {
        trajectory_id: TrajectoryId,
        #[source]
        source: OrchestratorError,
    },
    #[error("worker (step `{step_id}`): {source}")]
    Worker {
        trajectory_id: TrajectoryId,
        step_id: String,
        #[source]
        source: WorkerError,
    },
    #[error("critic (step `{step_id}`): {source}")]
    Critic {
        trajectory_id: TrajectoryId,
        step_id: String,
        #[source]
        source: CriticError,
    },
    /// Critic returned `Reject{reason}` for a step. We treat reject as
    /// terminal for the task (replan is future work).
    #[error("rejected (step `{step_id}`): {reason}")]
    Rejected {
        trajectory_id: TrajectoryId,
        step_id: String,
        reason: String,
    },
    /// Critic kept asking for Refine and we hit the per-step retry cap.
    #[error("refine exhausted (step `{step_id}`) after {attempts} attempts")]
    RefineExhausted {
        trajectory_id: TrajectoryId,
        step_id: String,
        attempts: u32,
    },
    /// Trajectory event-store write failed somewhere in the loop.
    #[error("runtime: {source}")]
    Runtime {
        trajectory_id: TrajectoryId,
        #[source]
        source: crate::RuntimeError,
    },
    /// Underlying agent step failed (LLM provider error, cancellation,
    /// internal). Wraps [`AgentExecutionError`] from
    /// [`execute_agent_step`].
    #[error("agent step (step `{step_id}`): {source}")]
    AgentStep {
        trajectory_id: TrajectoryId,
        step_id: String,
        #[source]
        source: AgentExecutionError,
    },
}

impl RunTaskError {
    pub fn trajectory_id(&self) -> &TrajectoryId {
        match self {
            Self::Orchestrator { trajectory_id, .. }
            | Self::Worker { trajectory_id, .. }
            | Self::Critic { trajectory_id, .. }
            | Self::Rejected { trajectory_id, .. }
            | Self::RefineExhausted { trajectory_id, .. }
            | Self::Runtime { trajectory_id, .. }
            | Self::AgentStep { trajectory_id, .. } => trajectory_id,
        }
    }
}

/// Bundles the runtime + LLM + agent triad + cancel token + tunables
/// so the loop's internal helpers can take a single `&LoopCtx`
/// instead of seven-plus discrete arguments. Built once at the top of
/// [`run_task`] and never mutated.
struct LoopCtx {
    runtime: Arc<dyn Runtime>,
    llm: Arc<dyn LlmService>,
    orchestrator: Arc<OrchestratorAgent>,
    worker: Arc<WorkerAgent>,
    critic: Arc<CriticAgent>,
    config: RunTaskConfig,
    cancel: CancellationToken,
}

/// Drive the Orchestrator → Worker → Critic loop for `goal`. See
/// module docs for the loop shape.
///
/// `cancel` is honoured at every agent boundary — a triggered token
/// surfaces as `RunTaskError::AgentStep` carrying
/// `AgentError::Cancelled`. The trajectory is left Abandoned in that
/// case so a recovery scan sees it as terminal.
//
// `too_many_arguments`: this is the user-facing entry point; explicit
// args make call sites self-documenting (which is which when you're
// reading a `run_task(...)` call). A builder/input-struct hides the
// triad behind a name without removing any complexity.
#[allow(clippy::too_many_arguments)]
pub async fn run_task(
    runtime: Arc<dyn Runtime>,
    llm: Arc<dyn LlmService>,
    orchestrator: Arc<OrchestratorAgent>,
    worker: Arc<WorkerAgent>,
    critic: Arc<CriticAgent>,
    goal: &str,
    config: RunTaskConfig,
    cancel: CancellationToken,
) -> Result<TaskOutcome, RunTaskError> {
    let ctx = LoopCtx {
        runtime,
        llm,
        orchestrator,
        worker,
        critic,
        config,
        cancel,
    };
    let traj = ctx
        .runtime
        .create_trajectory(None, &format!("run_task: {goal}"))
        .await
        .map_err(|e| RunTaskError::Runtime {
            trajectory_id: TrajectoryId::new("<uncreated>"),
            source: e,
        })?;

    // ── 1. Plan ────────────────────────────────────────────────────────
    let plan = match plan_step(&ctx, &traj, goal).await {
        Ok(p) => p,
        Err(e) => {
            abandon(&ctx.runtime, &traj, &format!("orchestrator failed: {e}")).await;
            return Err(e);
        }
    };

    // ── 2. Execute each plan step with Critic-driven retry ─────────────
    let mut step_outcomes = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let outcome = match run_one_step(&ctx, &traj, &plan, step, goal).await {
            Ok(o) => o,
            Err(e) => {
                abandon(&ctx.runtime, &traj, &format!("{e}")).await;
                return Err(e);
            }
        };
        step_outcomes.push(outcome);
    }

    // ── 3. Close ───────────────────────────────────────────────────────
    let summary = format!("completed {} step(s) for goal: {goal}", step_outcomes.len(),);
    ctx.runtime
        .append(
            &traj,
            AgentEvent::TrajectoryCompleted {
                traj: traj.clone(),
                summary,
            },
        )
        .await
        .map_err(|e| RunTaskError::Runtime {
            trajectory_id: traj.clone(),
            source: e,
        })?;

    Ok(TaskOutcome {
        trajectory_id: traj,
        plan,
        steps: step_outcomes,
    })
}

// ── Internal helpers ───────────────────────────────────────────────────

/// Run the Orchestrator: one trajectory step, parse JSON → typed Plan.
async fn plan_step(ctx: &LoopCtx, traj: &TrajectoryId, goal: &str) -> Result<Plan, RunTaskError> {
    let req = ctx.orchestrator.build_planner_request(goal);
    let agent: Arc<dyn Agent> = ctx.orchestrator.clone();
    let result = execute_agent_step(
        ctx.runtime.as_ref(),
        traj,
        ctx.llm.clone(),
        agent,
        req,
        ctx.cancel.clone(),
    )
    .await
    .map_err(|e| RunTaskError::AgentStep {
        trajectory_id: traj.clone(),
        step_id: "<planner>".into(),
        source: e,
    })?;

    let json_text = match result.output {
        AgentOutput::Text { text } => text,
        other => {
            return Err(RunTaskError::Orchestrator {
                trajectory_id: traj.clone(),
                source: OrchestratorError::UnexpectedOutput(format!(
                    "expected JSON text from the planner; got {other:?}"
                )),
            });
        }
    };
    OrchestratorAgent::parse_plan_response(&json_text).map_err(|source| {
        RunTaskError::Orchestrator {
            trajectory_id: traj.clone(),
            source,
        }
    })
}

/// Run one plan step's Worker → Critic loop with Refine retries.
async fn run_one_step(
    ctx: &LoopCtx,
    traj: &TrajectoryId,
    plan: &Plan,
    step: &PlanStep,
    goal: &str,
) -> Result<StepOutcome, RunTaskError> {
    let mut refinements: Vec<String> = Vec::new();
    let mut attempts: u32 = 0;
    loop {
        // ── Worker ──────────────────────────────────────────────────────
        let worker_result = run_worker(ctx, traj, plan, step, &refinements).await?;

        // ── Critic ──────────────────────────────────────────────────────
        let verdict_msg = run_critic(ctx, traj, plan, &worker_result, goal).await?;

        let verdict_kind = match &verdict_msg {
            AgentMessage::Verdict { verdict, .. } => verdict.clone(),
            other => {
                // parse_verdict_response is supposed to guarantee Verdict;
                // this is defensive so a future refactor can't quietly
                // break the loop's contract.
                return Err(RunTaskError::Critic {
                    trajectory_id: traj.clone(),
                    step_id: step.id.clone(),
                    source: CriticError::UnexpectedOutput(format!(
                        "expected Verdict envelope from critic; got {other:?}",
                    )),
                });
            }
        };

        match verdict_kind {
            VerdictKind::Approve => {
                return Ok(StepOutcome {
                    step_id: step.id.clone(),
                    result: worker_result,
                    verdict: verdict_msg,
                    refinement_attempts: attempts,
                });
            }
            VerdictKind::Reject { reason } => {
                return Err(RunTaskError::Rejected {
                    trajectory_id: traj.clone(),
                    step_id: step.id.clone(),
                    reason,
                });
            }
            VerdictKind::Refine { suggestions } => {
                if attempts >= ctx.config.max_refinements_per_step {
                    return Err(RunTaskError::RefineExhausted {
                        trajectory_id: traj.clone(),
                        step_id: step.id.clone(),
                        attempts: attempts + 1,
                    });
                }
                refinements = suggestions;
                attempts += 1;
            }
        }
    }
}

/// Run one Worker call, log it as a trajectory step, parse the typed
/// PartialResult.
async fn run_worker(
    ctx: &LoopCtx,
    traj: &TrajectoryId,
    plan: &Plan,
    step: &PlanStep,
    refinements: &[String],
) -> Result<AgentMessage, RunTaskError> {
    let req = ctx.worker.build_worker_request(plan, step, refinements);
    let agent: Arc<dyn Agent> = ctx.worker.clone();
    let result = execute_agent_step(
        ctx.runtime.as_ref(),
        traj,
        ctx.llm.clone(),
        agent,
        req,
        ctx.cancel.clone(),
    )
    .await
    .map_err(|e| RunTaskError::AgentStep {
        trajectory_id: traj.clone(),
        step_id: step.id.clone(),
        source: e,
    })?;

    let json_text = match result.output {
        AgentOutput::Text { text } => text,
        other => {
            return Err(RunTaskError::Worker {
                trajectory_id: traj.clone(),
                step_id: step.id.clone(),
                source: WorkerError::UnexpectedOutput(format!(
                    "expected JSON text from worker; got {other:?}"
                )),
            });
        }
    };
    WorkerAgent::parse_worker_response(&json_text, ctx.worker.id(), Some(step.id.as_str())).map_err(
        |source| RunTaskError::Worker {
            trajectory_id: traj.clone(),
            step_id: step.id.clone(),
            source,
        },
    )
}

/// Run one Critic call, log it as a trajectory step, parse the typed
/// Verdict envelope.
async fn run_critic(
    ctx: &LoopCtx,
    traj: &TrajectoryId,
    plan: &Plan,
    worker_result: &AgentMessage,
    goal: &str,
) -> Result<AgentMessage, RunTaskError> {
    let result_ref = crate::PartialResultRef::from_message(worker_result).ok_or_else(|| {
        RunTaskError::Critic {
            trajectory_id: traj.clone(),
            step_id: "<critic-input>".into(),
            source: CriticError::UnexpectedOutput(
                "worker did not produce a PartialResult message".into(),
            ),
        }
    })?;
    let target_step_id = result_ref.step_id.map(str::to_string);

    let req = ctx.critic.build_critique_request(plan, &result_ref, goal);
    let agent: Arc<dyn Agent> = ctx.critic.clone();
    let result = execute_agent_step(
        ctx.runtime.as_ref(),
        traj,
        ctx.llm.clone(),
        agent,
        req,
        ctx.cancel.clone(),
    )
    .await
    .map_err(|e| RunTaskError::AgentStep {
        trajectory_id: traj.clone(),
        step_id: target_step_id.clone().unwrap_or_else(|| "<critic>".into()),
        source: e,
    })?;

    let json_text = match result.output {
        AgentOutput::Text { text } => text,
        other => {
            return Err(RunTaskError::Critic {
                trajectory_id: traj.clone(),
                step_id: target_step_id.unwrap_or_else(|| "<critic>".into()),
                source: CriticError::UnexpectedOutput(format!(
                    "expected JSON verdict; got {other:?}",
                )),
            });
        }
    };
    CriticAgent::parse_verdict_response(&json_text, ctx.critic.id(), target_step_id.as_deref())
        .map_err(|source| RunTaskError::Critic {
            trajectory_id: traj.clone(),
            step_id: target_step_id.unwrap_or_else(|| "<critic>".into()),
            source,
        })
}

/// Best-effort: append `TrajectoryAbandoned` so a recovery scan sees
/// the trajectory as terminal. Failure here is logged but doesn't
/// override the original error the caller is about to surface.
async fn abandon(runtime: &Arc<dyn Runtime>, traj: &TrajectoryId, cause: &str) {
    let event = AgentEvent::TrajectoryAbandoned {
        traj: traj.clone(),
        cause: cause.to_string(),
    };
    if let Err(e) = runtime.append(traj, event).await {
        tracing::warn!(
            trajectory_id = %traj,
            error = %e,
            "run_task: failed to append TrajectoryAbandoned",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_task_config_default_is_two_refinements() {
        let c = RunTaskConfig::default();
        assert_eq!(c.max_refinements_per_step, 2);
    }

    #[test]
    fn error_trajectory_id_extraction_works_for_every_variant() {
        let t = TrajectoryId::new("t");
        let cases: Vec<RunTaskError> = vec![
            RunTaskError::Rejected {
                trajectory_id: t.clone(),
                step_id: "s".into(),
                reason: "x".into(),
            },
            RunTaskError::RefineExhausted {
                trajectory_id: t.clone(),
                step_id: "s".into(),
                attempts: 3,
            },
        ];
        for e in cases {
            assert_eq!(e.trajectory_id(), &t);
        }
    }
}
