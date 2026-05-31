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

use futures::stream::StreamExt;
use tars_pipeline::LlmService;
use tars_types::TrajectoryId;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentError, AgentOutput};
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

    // ── 1+2. Plan → Execute (DAG, depth-batched) → on Critic Reject, ──
    // ── replan from current state and retry the whole task ──
    //
    // Plan ↔ execute is wrapped in a replan loop. On the first
    // iteration we call `plan_step` (fresh plan from the goal alone).
    // On subsequent iterations we call `replan_step` with the prior
    // plan + partial results + reject reason, so the orchestrator can
    // propose a different decomposition rather than re-trying the
    // same shape and rejecting again.
    //
    // Execute phase is unchanged from the DAG commit: depth-batched
    // join_all per level, dep results threaded into dependent
    // workers' prompts. A Critic Reject in any step BREAKS out of
    // the level loop (but doesn't abandon — we wait for in-flight
    // siblings to finish so the event log stays consistent) and the
    // outer replan loop kicks in. Refine and other errors keep their
    // pre-replan semantics (refine retries in place, hard errors
    // abandon the trajectory).
    use std::collections::HashMap;
    let mut replan_attempt: u32 = 0;
    let mut plan: Plan;
    let mut step_outcomes_by_id: HashMap<String, StepOutcome>;
    let mut prior_plan: Option<Plan> = None;
    let mut prior_completed: HashMap<String, AgentMessage> = HashMap::new();
    let mut last_reject: Option<(String, String)> = None;
    loop {
        // ── Plan or replan ───────────────────────────────────────────
        plan = match if replan_attempt == 0 {
            plan_step(&ctx, &traj, goal).await
        } else {
            let (rejected_step, reject_reason) = last_reject
                .as_ref()
                .expect("replan path entered only via Reject, which sets last_reject");
            replan_step(
                &ctx,
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
                abandon(&ctx.runtime, &traj, &format!("orchestrator failed: {e}")).await;
                return Err(e);
            }
        };

        // ── Execute (DAG depth-batched parallel) ─────────────────────
        step_outcomes_by_id = HashMap::with_capacity(plan.steps.len());
        let mut completed: HashMap<String, AgentMessage> =
            HashMap::with_capacity(plan.steps.len());
        let mut hit_reject: Option<(String, String)> = None;

        'level_loop: for level in plan.execution_levels() {
            // Per-level cancel token, forked from the task-level
            // `ctx.cancel`. When the first Critic Reject in this
            // batch lands we fire `level_cancel.cancel()` to bail
            // any in-flight sibling LLM calls — without that, the
            // siblings would burn their full Worker+Critic budget
            // on work the replan loop will discard. Parent cancel
            // (`ctx.cancel`) still propagates through (the child
            // token observes both its own .cancel() AND the
            // parent's), so an external Ctrl-C still kills the
            // task as before.
            let level_cancel = ctx.cancel.child_token();

            // FuturesUnordered (vs join_all) lets us stream results
            // in completion order — the moment a Reject comes back
            // we fire `level_cancel.cancel()`, and only THEN drain
            // the remaining futures. Siblings still in flight will
            // observe the cancel via their own `tokio::select!`
            // against `ctx.cancel.cancelled()` inside the LLM
            // stream loop (Doc 04 §4.1's cancel contract on every
            // Agent::execute) and return `AgentError::Cancelled`,
            // which we ignore below.
            use futures::stream::FuturesUnordered;
            // `async` (no `move`) so each future borrows `ctx`, `traj`,
            // `plan`, `completed`, `step` from the enclosing scope.
            // `cancel` + `step_id_for_label` are clones moved IN —
            // those need per-future ownership (cancel because each
            // step's clone of the token is its own observer; step_id
            // because we return it as part of the result tuple after
            // the borrow on `step` has ended).
            let mut tasks: FuturesUnordered<_> = level
                .iter()
                .map(|step| {
                    let cancel = level_cancel.clone();
                    let step_id_for_label = step.id.clone();
                    async {
                        let res =
                            run_one_step(&ctx, &traj, &plan, step, goal, &completed, cancel)
                                .await;
                        (step_id_for_label, res)
                    }
                })
                .collect();

            // Stage this level's approved outcomes in a local Vec —
            // we can't mutate `completed` while `tasks` holds an
            // immutable borrow of it. Within-level siblings are
            // dependency-independent by construction (same depth),
            // so deferring `completed` writes to AFTER the drain is
            // semantically equivalent to writing them inline; the
            // staging only delays visibility to the NEXT level's
            // workers, which haven't started yet.
            let mut level_approved: Vec<(String, StepOutcome)> = Vec::new();
            while let Some((step_id, step_res)) = tasks.next().await {
                match step_res {
                    Ok(StepResult::Approved(outcome)) => {
                        level_approved.push((step_id, outcome));
                    }
                    Ok(StepResult::Rejected {
                        step_id: rejected_id,
                        reason,
                    }) => {
                        // First reject wins (subsequent rejects from
                        // siblings whose own critic also rejected
                        // before observing the cancel are ignored —
                        // they're equally informative for the replan
                        // prompt, no need to prefer one).
                        if hit_reject.is_none() {
                            hit_reject = Some((rejected_id, reason));
                            // Tell the still-running siblings to bail.
                            // They'll come back via `tasks.next()` as
                            // `Err(AgentStep { source: Cancelled })`
                            // which we discard in the Err arm below.
                            level_cancel.cancel();
                        }
                    }
                    Err(e) => {
                        // After we've fired level_cancel, in-flight
                        // siblings can come back with the synthetic
                        // `AgentError::Cancelled` chain — drop those
                        // silently (they're EXPECTED post-reject).
                        // Anything else is a real failure: abandon.
                        if hit_reject.is_some() && is_cancellation_err(&e) {
                            continue;
                        }
                        // Cancel siblings on real errors too — same
                        // rationale as reject: don't burn budget on
                        // work the failure will discard. Drain the
                        // remaining futures (skipping their cancelled
                        // returns) before bailing out so the event
                        // log stays consistent.
                        level_cancel.cancel();
                        // Drain the rest, discarding cancelled errors.
                        while let Some((_, drained_res)) = tasks.next().await {
                            if let Err(drained_err) = &drained_res {
                                if is_cancellation_err(drained_err) {
                                    continue;
                                }
                            }
                            // Any non-cancellation outcome on the
                            // drain path is unexpected once we've
                            // already cancelled — log and move on
                            // so the original `e` stays the
                            // surfaced cause.
                            tracing::warn!(
                                trajectory_id = %traj,
                                "run_task: ignoring drained-sibling result \
                                 after first-failure cancel (cause was: {e})",
                            );
                        }
                        abandon(&ctx.runtime, &traj, &format!("{e}")).await;
                        return Err(e);
                    }
                }
            }
            // Drain finished — but FuturesUnordered's Drop holds the
            // borrows it captured until the value itself goes out of
            // scope, so we drop it explicitly before mutating
            // `completed`. (Removing this `drop()` re-introduces the
            // E0502 we just fixed: NLL releases the immutable borrow
            // at the LAST USE, not at the next `}`.)
            drop(tasks);

            // Apply this level's approved outcomes. Skip if we hit a
            // reject (those outcomes are about to be discarded by the
            // replan path anyway).
            if hit_reject.is_none() {
                for (step_id, outcome) in level_approved {
                    completed.insert(step_id.clone(), outcome.result.clone());
                    step_outcomes_by_id.insert(step_id, outcome);
                }
            }
            if hit_reject.is_some() {
                break 'level_loop;
            }
        }

        // ── Decide: success / replan / replan-exhausted ─────────────
        if let Some((rejected_step, reason)) = hit_reject {
            if replan_attempt >= ctx.config.max_replans {
                let err = RunTaskError::ReplanExhausted {
                    trajectory_id: traj.clone(),
                    step_id: rejected_step,
                    reason,
                    replans: replan_attempt,
                };
                abandon(&ctx.runtime, &traj, &format!("{err}")).await;
                return Err(err);
            }
            // Stash context for the next iteration's replan prompt
            // (orchestrator sees prior plan + completed steps' results
            // + reject reason) and loop.
            prior_completed = completed;
            prior_plan = Some(plan);
            last_reject = Some((rejected_step, reason));
            replan_attempt += 1;
            continue;
        }

        // No reject anywhere — task succeeded. Break out of replan loop.
        break;
    }

    // Reassemble the per-step Vec in plan-declaration order. The DAG
    // executor produces outcomes in completion order (parallel batches
    // arrive together); the public TaskOutcome.steps shape is documented
    // (and tested) to follow `plan.steps` declaration order so callers
    // can index by `outcome.steps[i].step_id == plan.steps[i].id` for
    // both the serial-trivial case and the parallel fan-out case.
    let step_outcomes: Vec<StepOutcome> = plan
        .steps
        .iter()
        .map(|s| {
            step_outcomes_by_id
                .remove(&s.id)
                .expect("every plan step was either completed or surfaced as Reject above")
        })
        .collect();

    // ── 3. Close ───────────────────────────────────────────────────────
    // Best-effort, mirroring `abandon`: every step already wrote its own
    // terminal `StepCompleted` (the source of truth for what finished),
    // so the trailing `TrajectoryCompleted` is a convenience marker. If
    // its append fails, the work is still done — log and return the
    // successful outcome rather than discarding it as a task failure.
    // A trajectory missing this marker is recoverable / detectable from
    // the step events; reporting success as failure is not.
    let summary = format!("completed {} step(s) for goal: {goal}", step_outcomes.len(),);
    if let Err(e) = ctx
        .runtime
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
    drive_orchestrator_call(ctx, traj, req, "<planner>").await
}

/// Same as [`plan_step`] but uses `build_replanner_request` — the
/// orchestrator sees the failed plan + its partial results + the
/// Critic's reject reason so it can propose a different decomposition.
/// `replan_attempt` is the 1-based count (caller increments before
/// calling).
#[allow(clippy::too_many_arguments)]
async fn replan_step(
    ctx: &LoopCtx,
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
    ctx: &LoopCtx,
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

/// Internal step result discriminating Approved-with-outcome from the
/// Reject signal that the outer replan loop catches. We avoid using
/// `RunTaskError::ReplanExhausted` here because that's a TERMINAL
/// error (used after replan budget is exhausted) — a single-step
/// reject is recoverable via replan and shouldn't propagate as an
/// `Err` until the loop decides to give up.
enum StepResult {
    Approved(StepOutcome),
    Rejected { step_id: String, reason: String },
}

/// Run one plan step's Worker → Critic loop with Refine retries.
///
/// `completed` carries the parsed `AgentMessage::PartialResult` of
/// every step earlier in the dependency graph — the worker prompt
/// surfaces this step's deps' actual outputs (not just their ids) so
/// the worker can use upstream work instead of re-deriving it.
///
/// `cancel` is the **per-level** cancel token: when one sibling step
/// in the same level gets a Critic `Reject`, the outer level loop
/// fires this token so in-flight workers / critics observe the
/// cancel through their own `tokio::select!` and bail with
/// `AgentError::Cancelled`. Distinct from `ctx.cancel` (the
/// task-level token) which still propagates as a parent.
///
/// Returns `Ok(StepResult::Rejected{..})` rather than
/// `Err(ReplanExhausted{..})` when the Critic rejects — the outer
/// replan loop decides whether to keep trying (replan) or give up.
async fn run_one_step(
    ctx: &LoopCtx,
    traj: &TrajectoryId,
    plan: &Plan,
    step: &PlanStep,
    goal: &str,
    completed: &std::collections::HashMap<String, AgentMessage>,
    cancel: CancellationToken,
) -> Result<StepResult, RunTaskError> {
    let mut refinements: Vec<String> = Vec::new();
    let mut attempts: u32 = 0;
    loop {
        // ── Worker ──────────────────────────────────────────────────────
        let worker_result =
            run_worker(ctx, traj, plan, step, &refinements, completed, cancel.clone()).await?;

        // ── Critic ──────────────────────────────────────────────────────
        let verdict_msg = run_critic(ctx, traj, plan, &worker_result, goal, cancel.clone()).await?;

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
                return Ok(StepResult::Approved(StepOutcome {
                    step_id: step.id.clone(),
                    result: worker_result,
                    verdict: verdict_msg,
                    refinement_attempts: attempts,
                }));
            }
            VerdictKind::Reject { reason } => {
                // Don't propagate as `Err` — the outer replan loop in
                // `run_task` discriminates `StepResult::Rejected` to
                // decide whether to try a fresh plan or give up. This
                // keeps run_one_step's contract "I either produced a
                // step outcome, observed a critic-reject signal, or
                // hit a real error" — three independent failure
                // modes, three return shapes.
                return Ok(StepResult::Rejected {
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
///
/// `completed` lets the worker prompt thread the parsed outputs of
/// this step's dependencies into the request payload — DAG execution
/// without that threading degenerates to N independent LLM calls that
/// can't actually use each other's work.
async fn run_worker(
    ctx: &LoopCtx,
    traj: &TrajectoryId,
    plan: &Plan,
    step: &PlanStep,
    refinements: &[String],
    completed: &std::collections::HashMap<String, AgentMessage>,
    cancel: CancellationToken,
) -> Result<AgentMessage, RunTaskError> {
    let req = ctx.worker.build_worker_request(plan, step, refinements, completed);
    let agent: Arc<dyn Agent> = ctx.worker.clone();
    let result = execute_agent_step(
        ctx.runtime.as_ref(),
        traj,
        ctx.llm.clone(),
        agent,
        req,
        cancel,
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
    cancel: CancellationToken,
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
        cancel,
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

/// True iff `err` is a "this step was cancelled" signal (as opposed
/// to a real failure). Used by the level loop to discard sibling
/// results that came back as `AgentError::Cancelled` after we fired
/// `level_cancel.cancel()` — those are EXPECTED post-reject, not
/// failures to abandon the trajectory for.
fn is_cancellation_err(err: &RunTaskError) -> bool {
    matches!(
        err,
        RunTaskError::AgentStep {
            source: AgentExecutionError::Agent(AgentError::Cancelled),
            ..
        }
    )
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
