//! [`DurableScheduler`]: a DB-driven, memoized re-run driver.
//!
//! Replaces `run_plan`'s in-memory `'schedule` loop
//! (`tars-runtime/src/executor.rs`) — the `completed_shared` map, the
//! `pending`/`tasks` frontier — with a driver whose entire state is DERIVED
//! from the durable [`AnswerStore`] on every pass:
//!
//! - **readiness** = a step's `depends_on` all have answers present;
//! - **skip** = a completed answer is present ⇒ the step is not re-run
//!   (memoized re-run — the LLM is never re-called);
//! - **execute** = a ready step runs via the existing
//!   `Worker::run(plan, step, prior_results, ctx)` seam, and its answer is
//!   persisted ([`AnswerStore::commit_step`]) — which unlocks its
//!   dependents on the next pass.
//!
//! `run_plan` stays intact for ephemeral callers; this is the durable path
//! alongside it, not a replacement.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use tars_storage::{JOB_STATUS_DONE, ResultEventKind};
use tars_types::TrajectoryId;

use crate::durable::error::DurableError;
use crate::durable::store::{AnswerStore, StepAnswer};
use crate::message::AgentMessage;
use crate::orchestrator::{Plan, PlanStep};
use crate::executor::{Worker, WorkerContext, WorkerRegistry};
use crate::runtime::Runtime;
use tars_tools::SandboxPolicy;

/// Drives durable jobs to completion. Holds the always-on [`AnswerStore`]
/// (the truth), the [`WorkerRegistry`] (the work seam), and the
/// observability [`Runtime`] (which MAY be events-off — it is never a
/// correctness dependency).
pub struct DurableScheduler {
    store: AnswerStore,
    workers: WorkerRegistry,
    /// Observability runtime handed to each `WorkerContext`. Workers emit
    /// their own `StepStarted`/`StepCompleted` here; if events are OFF
    /// those are no-ops and the durable checkpoint is unaffected.
    runtime: Arc<dyn Runtime>,
    sandbox: SandboxPolicy,
    shared: Option<Arc<dyn Any + Send + Sync>>,
}

impl DurableScheduler {
    pub fn new(store: AnswerStore, workers: WorkerRegistry, runtime: Arc<dyn Runtime>) -> Self {
        Self { store, workers, runtime, sandbox: SandboxPolicy::default(), shared: None }
    }

    /// Set the OS-confinement policy applied to every step's tools.
    pub fn with_sandbox(mut self, sandbox: SandboxPolicy) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Inject the run-scoped blackboard handle (type-erased) into every
    /// `WorkerContext.shared`.
    pub fn with_shared(mut self, shared: Arc<dyn Any + Send + Sync>) -> Self {
        self.shared = Some(shared);
        self
    }

    /// Convenience: persist a fresh job (its plan) and drive it to
    /// completion in one call.
    pub async fn submit_and_run(&self, job_id: &str, plan: &Plan) -> Result<(), DurableError> {
        self.store.create_job(job_id, plan)?;
        self.run_job(job_id).await
    }

    /// (Re-)drive `job_id`'s DAG from the durable store. Idempotent and
    /// crash-safe: completed steps are skipped (their answers are present),
    /// only un-done steps execute. Safe to call repeatedly — a
    /// fully-resolved job is a no-op that marks the job terminal.
    pub async fn run_job(&self, job_id: &str) -> Result<(), DurableError> {
        let plan = self.store.load_plan(job_id)?;
        plan.validate().map_err(|e| DurableError::InvalidPlan(e.to_string()))?;

        // Pre-flight: every role resolves, before any step runs (mirrors
        // run_plan's up-front check).
        for step in &plan.steps {
            if self.workers.find(&step.worker_role).is_none() {
                return Err(DurableError::NoWorkerForRole {
                    role: step.worker_role.clone(),
                    step_id: step.id.clone(),
                });
            }
        }

        loop {
            // The frontier is DERIVED from the store, every pass.
            let mut answers = self.store.answers(job_id)?;

            // ── Skip fixpoint: cascade (dep skipped) + condition. ──────
            // Skipping a step resolves it, which may cascade-skip a
            // dependent in the same pass — so loop to a fixpoint.
            loop {
                let mut progressed = false;
                for step in &plan.steps {
                    if answers.contains_key(&step.id) || !deps_present(step, &answers) {
                        continue;
                    }
                    // (a) cascade skip — priority over condition.
                    if let Some(dep) = step
                        .depends_on
                        .iter()
                        .find(|d| answers.get(d.as_str()).is_some_and(StepAnswer::is_skipped))
                    {
                        let reason = format!("dep `{dep}` was skipped");
                        self.record_skip(job_id, step, &reason, &mut answers)?;
                        progressed = true;
                        continue;
                    }
                    // (b) condition skip — evaluate against completed deps.
                    let completed = completed_messages(&answers);
                    if !step.condition.matches(&completed) {
                        let reason = step.condition.skip_reason();
                        self.record_skip(job_id, step, &reason, &mut answers)?;
                        progressed = true;
                    }
                }
                if !progressed {
                    break;
                }
            }

            // ── Runnable = un-done, deps present (survived the skip pass). ──
            let runnable: Vec<&PlanStep> = plan
                .steps
                .iter()
                .filter(|s| !answers.contains_key(&s.id) && deps_present(s, &answers))
                .collect();

            if runnable.is_empty() {
                if plan.steps.iter().all(|s| answers.contains_key(&s.id)) {
                    self.store.set_job_status(job_id, JOB_STATUS_DONE)?;
                    return Ok(());
                }
                // Nothing ready + not all resolved ⇒ a step's deps never
                // became present. `Plan::validate` rules out the cycle
                // that would cause this; guard defensively.
                let stuck = plan
                    .steps
                    .iter()
                    .filter(|s| !answers.contains_key(&s.id))
                    .map(|s| s.id.clone())
                    .collect();
                return Err(DurableError::Stalled(stuck));
            }

            // ── Execute this batch; persist each answer (unlocks deps). ──
            let prior = completed_messages(&answers);
            let mut futs = FuturesUnordered::new();
            for step in runnable {
                futs.push(self.drive_step(job_id, &plan, step, &prior));
            }
            while let Some(res) = futs.next().await {
                let answer = res?;
                self.store.commit_step(&answer, ResultEventKind::Completed, None)?;
            }
        }
    }

    /// Persist a skip decision (answer + `Skipped` event, one tx) and
    /// mirror it into the in-memory `answers` map so the fixpoint sees it.
    fn record_skip(
        &self,
        job_id: &str,
        step: &PlanStep,
        reason: &str,
        answers: &mut HashMap<String, StepAnswer>,
    ) -> Result<(), DurableError> {
        let ans = StepAnswer::skipped(job_id, &step.id, reason);
        self.store.commit_step(&ans, ResultEventKind::Skipped, Some(reason))?;
        answers.insert(step.id.clone(), ans);
        Ok(())
    }

    /// Run one step through the `Worker::run` seam and return its
    /// checkpoint-ready answer. Does NOT persist — the caller commits so
    /// the store write stays on the driver.
    async fn drive_step(
        &self,
        job_id: &str,
        plan: &Plan,
        step: &PlanStep,
        prior: &HashMap<String, AgentMessage>,
    ) -> Result<StepAnswer, DurableError> {
        let worker: Arc<dyn Worker> = self
            .workers
            .find(&step.worker_role)
            .expect("pre-flight ensured every worker_role resolves");
        let ctx = WorkerContext {
            runtime: self.runtime.clone(),
            // One trajectory per job for the worker's own (off-able) event
            // emission; the durable checkpoint is separate.
            trajectory_id: TrajectoryId::new(job_id.to_string()),
            cancel: CancellationToken::new(),
            refinements: Vec::new(),
            shared: self.shared.clone(),
            sandbox: self.sandbox.clone(),
        };
        let output = worker
            .run(plan, step, prior, ctx)
            .await
            .map_err(|source| DurableError::Worker {
                step_id: step.id.clone(),
                source: Box::new(source),
            })?;
        if !matches!(output.message, AgentMessage::PartialResult { .. }) {
            return Err(DurableError::UnexpectedOutput {
                step_id: step.id.clone(),
                got: format!("{:?}", output.message),
            });
        }
        Ok(StepAnswer::completed(job_id, &step.id, output.message, output.usage, output.created))
    }
}

/// True iff every one of `step`'s deps has an answer present.
fn deps_present(step: &PlanStep, answers: &HashMap<String, StepAnswer>) -> bool {
    step.depends_on.iter().all(|d| answers.contains_key(d))
}

/// The completed-step messages (skipped steps excluded) — the
/// `prior_results` / condition-input map, matching `run_plan`'s
/// `completed_shared` (which only ever holds `Completed` results).
fn completed_messages(answers: &HashMap<String, StepAnswer>) -> HashMap<String, AgentMessage> {
    answers
        .iter()
        .filter(|(_, a)| !a.is_skipped())
        .map(|(id, a)| (id.clone(), a.message.clone()))
        .collect()
}
