//! [`run_task`] — the multi-step Orchestrator → Worker → Critic loop.
//!
//! Public LLM-agent entry point. Given a free-form `goal`, drives a
//! real triad of agents end-to-end with full trajectory logging,
//! Critic-driven refinement, and replan-on-reject.
//!
//! ## Layering after the executor extraction
//!
//! Since `tars-runtime` 0.3, the DAG primitives (cancel-on-reject,
//! FuturesUnordered level batching, skip-cascade, refinement loop)
//! live in [`crate::executor`]. `run_task` is a thin LLM-flavored
//! shell that:
//!   1. Creates a trajectory.
//!   2. Calls [`OrchestratorAgent`] to produce a [`Plan`].
//!   3. Wraps the supplied `WorkerAgent` + `CriticAgent` as
//!      [`LlmWorker`](crate::LlmWorker) + [`LlmCritic`](crate::LlmCritic)
//!      and hands them to [`crate::run_plan`].
//!   4. On [`RunPlanError::Rejected`] — calls the orchestrator's
//!      replan path with the failed plan + partial results, loops
//!      back to step 3 with the new plan, up to
//!      `config.max_replans`.
//!   5. Closes the trajectory.
//!
//! Callers who already have a `Plan` (e.g. arc auto's
//! deterministic `scan → fix → verify` chain) should call
//! [`crate::run_plan`] directly — skips the Orchestrator LLM round
//! AND the replan loop, and accepts non-LLM workers via the
//! [`crate::Worker`] trait.
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

use crate::agent::{Agent, AgentError, AgentOutput};
use crate::critic::{CriticAgent, CriticError};
use crate::event::AgentEvent;
use crate::message::AgentMessage;
use crate::orchestrator::{OrchestratorAgent, OrchestratorError, Plan};
use crate::runtime::{AgentExecutionError, Runtime, execute_agent_step};
use crate::worker::{WorkerAgent, WorkerError};

/// Tunable knobs. Defaults are conservative — bump for harder tasks.
#[derive(Clone, Debug)]
pub struct RunTaskConfig {
    /// Maximum number of Refine retries per step (NOT counting the
    /// initial attempt). `0` means "Worker gets exactly one chance,
    /// any Refine verdict fails the task".
    pub max_refinements_per_step: u32,
    /// Maximum number of times the Orchestrator may replan after a
    /// Critic `Reject` verdict. `0` means "the first Reject is
    /// terminal" — the historical behaviour before replan landed.
    /// Counts each replan separately: with `max_replans = 2` the
    /// orchestrator gets the initial plan + up to 2 replans = 3
    /// total plan attempts before `ReplanExhausted` fires.
    pub max_replans: u32,
}

impl Default for RunTaskConfig {
    fn default() -> Self {
        Self {
            max_refinements_per_step: 2,
            max_replans: 2,
        }
    }
}

// `StepOutcome` and `TaskOutcome` moved to [`crate::executor`] in
// the executor-extraction refactor. Re-exported here so existing
// imports `use tars_runtime::{StepOutcome, TaskOutcome}` keep
// working unchanged. New callers should import from the top-level
// crate root.
pub use crate::executor::{StepOutcome, TaskOutcome};

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
    /// Critic returned `Reject{reason}` for a step **and** the
    /// orchestrator exhausted its `max_replans` budget — i.e. the
    /// original plan failed, every replan attempt also produced a
    /// rejected step, so the task is unsalvageable. The `step_id`
    /// is the step that triggered the LAST rejection; the trajectory
    /// log carries every intermediate plan + reject for forensics.
    /// (Pre-replan, this variant fired on the FIRST reject — kept
    /// the name to avoid breaking match-arms in downstream code that
    /// only ever cared about "task rejected, give up".)
    #[error("rejected after {replans} replan(s); last failure on step `{step_id}`: {reason}")]
    ReplanExhausted {
        trajectory_id: TrajectoryId,
        /// Step id of the LAST replan's rejection (the most recent).
        step_id: String,
        reason: String,
        /// Total replan attempts made before giving up (= max_replans).
        replans: u32,
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
            | Self::ReplanExhausted { trajectory_id, .. }
            | Self::RefineExhausted { trajectory_id, .. }
            | Self::Runtime { trajectory_id, .. }
            | Self::AgentStep { trajectory_id, .. } => trajectory_id,
        }
    }
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
    let traj = runtime
        .create_trajectory(None, &format!("run_task: {goal}"))
        .await
        .map_err(|e| RunTaskError::Runtime {
            trajectory_id: TrajectoryId::new("<uncreated>"),
            source: e,
        })?;

    // Wrap the LLM-flavored agents as executor [`Worker`] / [`Critic`]
    // trait impls. The same LlmWorker handles every worker_role
    // (single default registration — `run_task` exposes one
    // WorkerAgent), and the critic is always present (its absence is
    // the `run_plan` direct-call path, not the LLM agent loop).
    let llm_worker_impl: Arc<dyn crate::executor::Worker> =
        crate::llm_adapters::LlmWorker::new(worker.clone(), llm.clone());
    let llm_critic_impl: Arc<dyn crate::executor::Critic> =
        crate::llm_adapters::LlmCritic::new(critic.clone(), llm.clone());
    let workers = crate::executor::WorkerRegistry::new().with_default(llm_worker_impl);
    let run_plan_config = crate::executor::RunPlanConfig {
        max_refinements_per_step: config.max_refinements_per_step,
        // run_task's LLM agent loop keeps the historical semantics:
        // unbounded per-tier concurrency + no infra retry (the
        // LlmService layer owns its own transport retry). The infra
        // policy is opt-in for direct `run_plan` callers like arc.
        ..Default::default()
    };

    // Build the plan/replan helper closure context. The orchestrator
    // is the only piece that lives outside `run_plan` (it produces
    // the Plan that `run_plan` then executes).
    let plan_ctx = OrchestratorCallCtx {
        runtime: runtime.clone(),
        llm: llm.clone(),
        orchestrator: orchestrator.clone(),
        cancel: cancel.clone(),
    };

    // ── 1+2. Plan → run_plan → on Reject, replan from prior context. ──
    //
    // First iteration: `plan_step` produces a fresh Plan from the goal
    // alone. Subsequent iterations (after a Reject lands inside
    // `run_plan` and budget remains) use `replan_step` with the prior
    // plan + partial results + reject reason so the orchestrator can
    // propose a different decomposition. `run_plan` itself handles
    // the DAG (level batching, cancel-on-reject, refinement, skip /
    // cascade) — `run_task` is just the outer LLM agent-loop shell.
    //
    use std::collections::HashMap;
    let mut replan_attempt: u32 = 0;
    let mut prior_plan: Option<Plan> = None;
    let mut prior_completed: HashMap<String, AgentMessage> = HashMap::new();
    let mut last_reject: Option<(String, String)> = None;

    let outcome = loop {
        // ── Plan or replan ───────────────────────────────────────────
        let plan = match if replan_attempt == 0 {
            plan_step(&plan_ctx, &traj, goal).await
        } else {
            let (rejected_step, reject_reason) = last_reject
                .as_ref()
                .expect("replan path entered only via Reject, which sets last_reject");
            replan_step(
                &plan_ctx,
                &traj,
                goal,
                prior_plan
                    .as_ref()
                    .expect("replan path always has a prior_plan from the previous iteration"),
                &prior_completed,
                rejected_step,
                reject_reason,
                replan_attempt,
            )
            .await
        } {
            Ok(p) => p,
            Err(e) => {
                abandon(&runtime, &traj, &format!("orchestrator failed: {e}")).await;
                return Err(e);
            }
        };

        // ── Execute via the DAG executor ─────────────────────────────
        match crate::executor::run_plan(
            runtime.clone(),
            traj.clone(),
            plan,
            workers.clone(),
            Some(llm_critic_impl.clone()),
            run_plan_config.clone(),
            cancel.clone(),
        )
        .await
        {
            Ok(o) => break o,
            Err(crate::executor::RunPlanError::Rejected {
                plan: failed_plan,
                rejected_step_id,
                reason,
                completed,
                ..
            }) => {
                if replan_attempt >= config.max_replans {
                    let err = RunTaskError::ReplanExhausted {
                        trajectory_id: traj.clone(),
                        step_id: rejected_step_id,
                        reason,
                        replans: replan_attempt,
                    };
                    abandon(&runtime, &traj, &format!("{err}")).await;
                    return Err(err);
                }
                prior_completed = completed;
                prior_plan = Some(failed_plan);
                last_reject = Some((rejected_step_id, reason));
                replan_attempt += 1;
                continue;
            }
            Err(other) => {
                let mapped = map_run_plan_error(&traj, other);
                abandon(&runtime, &traj, &format!("{mapped}")).await;
                return Err(mapped);
            }
        }
    };

    // ── 3. Close ───────────────────────────────────────────────────────
    // Best-effort, mirroring `abandon`: every step's terminal event
    // is already in the log (executor + worker + critic emit their
    // own StepCompleted / StepFailed), so this trailing
    // `TrajectoryCompleted` is a convenience marker. Logging-only
    // failure here: task work is done, return the outcome anyway. A
    // trajectory missing this marker is recoverable / detectable
    // from the step events; reporting success as failure is not.
    let summary = format!("completed {} step(s) for goal: {goal}", outcome.steps.len());
    if let Err(e) = runtime
        .append(
            &traj,
            AgentEvent::TrajectoryCompleted {
                traj: traj.clone(),
                summary,
            },
        )
        .await
    {
        tracing::warn!(
            trajectory_id = %traj,
            error = %e,
            "run_task: failed to append TrajectoryCompleted; task work \
             succeeded, returning outcome anyway",
        );
    }

    Ok(outcome)
}

// ── Internal helpers ───────────────────────────────────────────────────

/// Bundles the runtime + LLM + orchestrator + cancel into one struct
/// so the plan / replan helpers below take a single
/// `&OrchestratorCallCtx` instead of four args. Built once at the top
/// of `run_task`.
struct OrchestratorCallCtx {
    runtime: Arc<dyn Runtime>,
    llm: Arc<dyn LlmService>,
    orchestrator: Arc<OrchestratorAgent>,
    cancel: CancellationToken,
}

/// Map a [`crate::executor::RunPlanError`] (other than `Rejected`,
/// which `run_task`'s replan loop handles inline) into the
/// equivalent [`RunTaskError`] variant. Keeps the public error shape
/// of `run_task` stable across the executor extraction.
fn map_run_plan_error(traj: &TrajectoryId, e: crate::executor::RunPlanError) -> RunTaskError {
    use crate::executor::RunPlanError;
    match e {
        RunPlanError::Rejected { .. } => unreachable!(
            "run_task handles RunPlanError::Rejected in its replan loop; this arm is dead",
        ),
        RunPlanError::RefineExhausted {
            step_id, attempts, ..
        } => RunTaskError::RefineExhausted {
            trajectory_id: traj.clone(),
            step_id,
            attempts,
        },
        RunPlanError::NoWorkerForRole {
            role, step_id, ..
        } => RunTaskError::AgentStep {
            trajectory_id: traj.clone(),
            step_id,
            source: AgentExecutionError::Agent(AgentError::Internal(format!(
                "no worker registered for role `{role}` — run_task only registers \
                 an LLM default worker, so the registry's default fallback should \
                 have matched; this is a bug in run_task's wiring",
            ))),
        },
        RunPlanError::Worker {
            step_id, source, ..
        } => RunTaskError::Worker {
            trajectory_id: traj.clone(),
            step_id,
            source,
        },
        RunPlanError::Critic {
            step_id, source, ..
        } => RunTaskError::Critic {
            trajectory_id: traj.clone(),
            step_id,
            source,
        },
        RunPlanError::AgentStep {
            step_id, source, ..
        } => RunTaskError::AgentStep {
            trajectory_id: traj.clone(),
            step_id,
            source,
        },
        RunPlanError::Runtime { source, .. } => RunTaskError::Runtime {
            trajectory_id: traj.clone(),
            source,
        },
        RunPlanError::InvalidPlan(msg) => RunTaskError::Orchestrator {
            trajectory_id: traj.clone(),
            source: OrchestratorError::InvalidPlan(msg),
        },
    }
}

/// Run the Orchestrator: one trajectory step, parse JSON → typed Plan.
async fn plan_step(
    ctx: &OrchestratorCallCtx,
    traj: &TrajectoryId,
    goal: &str,
) -> Result<Plan, RunTaskError> {
    let req = ctx.orchestrator.build_planner_request(goal);
    drive_orchestrator_call(ctx, traj, req, "<planner>").await
}

/// Same as [`plan_step`] but uses `build_replanner_request` — the
/// orchestrator sees the failed plan + its partial results + the
/// Critic's reject reason so it can propose a different decomposition.
/// `replan_attempt` is the 1-based count (caller increments before
/// calling).
#[allow(clippy::too_many_arguments)]
async fn replan_step(
    ctx: &OrchestratorCallCtx,
    traj: &TrajectoryId,
    goal: &str,
    previous_plan: &Plan,
    previous_results: &std::collections::HashMap<String, AgentMessage>,
    rejected_step: &str,
    reject_reason: &str,
    replan_attempt: u32,
) -> Result<Plan, RunTaskError> {
    let req = ctx.orchestrator.build_replanner_request(
        goal,
        previous_plan,
        previous_results,
        rejected_step,
        reject_reason,
        replan_attempt,
    );
    drive_orchestrator_call(
        ctx,
        traj,
        req,
        &format!("<replanner attempt={replan_attempt}>"),
    )
    .await
}

/// Shared body of `plan_step` + `replan_step`: drive one orchestrator
/// LLM call via `execute_agent_step`, parse the JSON into a `Plan`.
async fn drive_orchestrator_call(
    ctx: &OrchestratorCallCtx,
    traj: &TrajectoryId,
    req: tars_types::ChatRequest,
    step_label: &str,
) -> Result<Plan, RunTaskError> {
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
        step_id: step_label.to_string(),
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

/// Best-effort: append `TrajectoryAbandoned` so a recovery scan
/// sees the trajectory as terminal. Failure here is logged but
/// doesn't override the original error the caller is about to
/// surface.
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
            RunTaskError::ReplanExhausted {
                trajectory_id: t.clone(),
                step_id: "s".into(),
                reason: "x".into(),
                replans: 2,
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
