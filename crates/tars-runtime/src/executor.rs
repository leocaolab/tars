//! Caller-supplied DAG executor — runs a [`Plan`] without forcing
//! an LLM Orchestrator (caller provides the plan) or Critic (caller
//! may pass `None`).
//!
//! ## When to use this directly vs [`crate::run_task`]
//!
//! - **[`run_plan`]** (this module): you already know the plan shape
//!   (e.g. arc auto's `scan → fix-fanout → merge_sweep → test →
//!   coverage` chain). No LLM planning round, no forced critic;
//!   workers can be subprocesses / pure Rust functions / mocks —
//!   only those that want LLM features impl [`Worker`] backed by an
//!   LLM call.
//!
//! - **[`crate::run_task`]**: free-form goal string + full LLM agent
//!   loop (plan + execute + critic-judge + replan-on-reject).
//!   Internally builds an [`LlmWorker`](crate::LlmWorker) +
//!   [`LlmCritic`](crate::LlmCritic) over the existing
//!   [`WorkerAgent`](crate::WorkerAgent) /
//!   [`CriticAgent`](crate::CriticAgent) and calls `run_plan` inside
//!   its replan loop — so the DAG primitives (cancel-on-reject,
//!   conditional skip, skip-cascade, FuturesUnordered level batching,
//!   trajectory event log) live HERE and `run_task` is a thin
//!   LLM-flavored shell on top.
//!
//! ## Worker contract
//!
//! Workers return a [`WorkerOutput`] carrying:
//!   - `message` — must be [`AgentMessage::PartialResult`]; the
//!     envelope itself is LLM-agnostic (see `message.rs`).
//!   - `usage` — token counts. LLM workers fill real numbers;
//!     non-LLM workers pass [`Usage::default()`] so
//!     [`RunReport.llm_call_count`](tars_types::RunReport) /
//!     `tokens` read as honest zeros, not fake values.
//!
//! ## Event responsibility split
//!
//! - **Executor** emits [`AgentEvent::StepSkipped`] (its job — the
//!   worker never gets called for a skipped step).
//! - **Worker** emits its own `StepStarted` / `StepCompleted` /
//!   `StepFailed` (and `LlmCallCaptured` for LLM workers). LLM
//!   workers do this via [`crate::execute_agent_step`]; non-LLM
//!   workers either call the runtime directly or use the
//!   convenience helper [`emit_step_lifecycle`] in this module.
//!
//! This split lets LLM workers reuse the existing
//! [`execute_agent_step`](crate::execute_agent_step) path unchanged
//! (no double-emission), while non-LLM workers get a single helper
//! to wrap their work without re-implementing the event shape.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use tars_types::{TrajectoryId, Usage};

use crate::agent::StepError;
use crate::critic::CriticError;
use crate::event::AgentEvent;
use crate::message::{AgentMessage, VerdictKind};
use crate::orchestrator::{Plan, PlanStep};
use crate::runtime::{AgentExecutionError, Runtime};
use crate::worker::WorkerError;

// ─── Public types ──────────────────────────────────────────────────────

/// What a [`Worker::run`] returns on success.
///
/// `message` MUST be the [`AgentMessage::PartialResult`] variant —
/// the executor's downstream wiring (`prior_results` threading,
/// trajectory log shape, [`Critic`] input) assumes that shape. The
/// executor surfaces a non-conforming return as
/// [`RunPlanError::Worker`] with a `WorkerError::UnexpectedOutput`
/// chain so the bug fails loud rather than corrupting the event log.
#[derive(Clone, Debug)]
pub struct WorkerOutput {
    /// Must be [`AgentMessage::PartialResult`]. Other variants are
    /// rejected by the executor.
    pub message: AgentMessage,
    /// Token usage for this step. LLM workers fill real counts;
    /// non-LLM workers pass [`Usage::default()`].
    pub usage: Usage,
    /// Unix-seconds when this step's LLM response was finalized
    /// ([`ChatResponse::created`]) — the DISCOVERY time. LLM workers carry the
    /// real value up alongside `usage`; non-LLM workers pass `0`.
    pub created: i64,
}

/// Per-invocation context handed to a [`Worker`]. The worker is
/// responsible for emitting its own `StepStarted` / `StepCompleted`
/// / `StepFailed` (and `LlmCallCaptured` for LLM workers) via
/// `runtime`. LLM workers reuse [`crate::execute_agent_step`] which
/// does that automatically; non-LLM workers can call
/// [`emit_step_lifecycle`] to wrap their body without writing the
/// event-emission boilerplate themselves.
pub struct WorkerContext {
    pub runtime: Arc<dyn Runtime>,
    pub trajectory_id: TrajectoryId,
    pub cancel: CancellationToken,
    /// Critic suggestions from the previous refinement attempt.
    /// Empty on the first attempt for each step. LLM workers thread
    /// these into the next prompt; non-LLM workers ignore them
    /// (their Critic — if any — is whatever they encode internally).
    pub refinements: Vec<String>,
    /// The run-scoped blackboard (Doc 19 §4.1), injected so a worker can
    /// `view`/`commit` domain state. Type-erased because `Worker` is a trait
    /// object and the blackboard is the consumer's concrete type (with its own
    /// `Entity`/`Event`): a blackboard-aware worker downcasts it back to its
    /// own handle (e.g. `Arc<ArcBlackboard>`); workers that don't touch domain
    /// state ignore it. `None` when the caller didn't supply one.
    pub shared: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

/// Per-invocation context handed to a [`Critic`]. Same shape as
/// [`WorkerContext`] minus `refinements` (critics emit them, not
/// consume them).
pub struct CriticContext {
    pub runtime: Arc<dyn Runtime>,
    pub trajectory_id: TrajectoryId,
    pub cancel: CancellationToken,
}

/// Executes one [`PlanStep`]. Trait so callers can plug in:
///   - LLM-backed workers ([`LlmWorker`](crate::LlmWorker)) — wrap a
///     [`WorkerAgent`](crate::WorkerAgent) + `LlmService`
///   - Subprocess workers (spawn an external process, parse stdout)
///   - Pure Rust workers (no I/O — useful for testing or for
///     deterministic glue steps like `merge_sweep` / `cargo_test` /
///     coverage rollup)
///
/// The executor calls `run` after appending `StepStarted`. On `Ok`
/// the executor appends `StepCompleted`; on `Err` it appends
/// `StepFailed` with the error's classification.
#[async_trait]
pub trait Worker: Send + Sync {
    async fn run(
        &self,
        plan: &Plan,
        step: &PlanStep,
        prior_results: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError>;

    /// DECLARED: which blackboard entities this worker reads (Doc 19 §10). A
    /// declaration, not an action — lets a pipeline reason about dataflow. The
    /// worker still calls `ctx.shared` itself inside `run`. Default: reads
    /// nothing domain-specific.
    fn reads(&self) -> tars_storage::Scope {
        tars_storage::Scope::All
    }

    /// DECLARED: which transition kinds (wire strings) this worker MAY commit
    /// (Doc 19 §10) — for pipeline reasoning + an optional emits guard. A
    /// declaration, not the write: the worker commits EXPLICITLY in `run` via
    /// `ctx.shared.commit(..)`. Wire strings (not a typed `Event`) because
    /// `Worker` is a trait object with no consumer `Event` type. Default: none.
    fn emits(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Judges a [`Worker`]'s output and returns a [`VerdictKind`]:
/// `Approve` ends the step; `Refine{suggestions}` re-runs the worker
/// with the suggestions threaded into the next attempt's
/// `WorkerContext.refinements` (up to `RunPlanConfig.max_refinements_per_step`);
/// `Reject{reason}` bubbles up as [`RunPlanError::Rejected`] —
/// callers like [`crate::run_task`] catch it for replan.
///
/// **Optional**. Pass `None` to [`run_plan`] for a no-critic flow —
/// every `Ok` from a worker is auto-approved, no extra LLM call.
/// Use when the worker already encodes its own success criteria
/// (e.g. arc's per-finding critic lives inside the fix-worker loop;
/// no benefit to a second judging pass).
#[async_trait]
pub trait Critic: Send + Sync {
    async fn judge(
        &self,
        plan: &Plan,
        step: &PlanStep,
        worker_output: &AgentMessage,
        ctx: CriticContext,
    ) -> Result<VerdictKind, CriticError>;
}

/// Per-`worker_role` dispatch table. The executor looks up
/// `step.worker_role` in `per_role` first; on miss falls back to
/// `default`. If neither exists for a role, the executor returns
/// [`RunPlanError::NoWorkerForRole`] before scheduling the step.
#[derive(Clone)]
pub struct WorkerRegistry {
    per_role: HashMap<String, Arc<dyn Worker>>,
    default: Option<Arc<dyn Worker>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self {
            per_role: HashMap::new(),
            default: None,
        }
    }

    /// Register an impl for the given `worker_role`. Replaces any
    /// previous entry under that role (last write wins).
    pub fn register(&mut self, role: impl Into<String>, worker: Arc<dyn Worker>) -> &mut Self {
        self.per_role.insert(role.into(), worker);
        self
    }

    /// Fallback for any `step.worker_role` that has no per-role
    /// entry. Useful for an LLM-backed default that handles every
    /// role generically (the [`crate::run_task`] use case).
    pub fn with_default(mut self, worker: Arc<dyn Worker>) -> Self {
        self.default = Some(worker);
        self
    }

    /// Resolve a worker for `role`. Used internally by the executor;
    /// exposed `pub` so callers building unusual flows can introspect.
    pub fn find(&self, role: &str) -> Option<Arc<dyn Worker>> {
        self.per_role
            .get(role)
            .cloned()
            .or_else(|| self.default.clone())
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Tunables for [`run_plan`]. Defaults match
/// [`crate::RunTaskConfig`]'s historical defaults so existing
/// callers see no behavioural change after the executor refactor.
#[derive(Clone)]
pub struct RunPlanConfig {
    /// Run-scoped blackboard (Doc 19 §4.1), type-erased, injected into every
    /// `WorkerContext.shared` on the run. `None` = no blackboard (pre-existing
    /// callers). Type-erased because `Worker` is a trait object; a
    /// blackboard-aware worker downcasts it back to its concrete handle.
    pub shared: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// Maximum number of `Refine` retries per step (NOT counting
    /// the initial attempt). `0` = worker gets exactly one shot per
    /// step; any `Refine` verdict immediately fails the step with
    /// [`RunPlanError::RefineExhausted`].
    pub max_refinements_per_step: u32,
    /// Cap on concurrently-running steps across the WHOLE plan (not
    /// per graph-depth tier). `None` = unbounded — every step whose
    /// deps are satisfied starts immediately, matching the historical
    /// depth-batched loop's peak width. `Some(n)` = at most `n`
    /// workers in flight at once. This matters now that scheduling is
    /// dependency-driven: a wide root tier (e.g. arc's 200+ per-file
    /// scan steps) would otherwise fire hundreds of provider calls
    /// simultaneously and trip upstream rate limits.
    pub max_concurrent: Option<usize>,
    /// Retry policy for *infra* failures — provider rate-limit /
    /// circuit-open / timeout / transient network. Distinct from
    /// `max_refinements_per_step`, which retries on a *critic*
    /// `Refine` verdict (the worker produced output, but it needs
    /// improvement). Infra retry re-runs the SAME attempt because the
    /// transport failed, not the work. Default = no retry (so
    /// pre-existing callers see byte-identical behaviour).
    pub infra_retry: InfraRetryPolicy,
    /// Per-step wall-clock backstop. `None` = no cap (default). When
    /// `Some(d)`, each worker invocation that runs longer than `d` is
    /// aborted and surfaced as [`WorkerError::TimedOut`] — which the
    /// infra-retry classifier treats as transient, so the step retries
    /// under `infra_retry` if budget remains. This is a BACKSTOP for a
    /// hung provider call whose own transport timeout failed to fire;
    /// set it generously (longer than any legitimate step) so it never
    /// trips on healthy work. Applies per-attempt: each infra retry
    /// gets a fresh budget.
    ///
    /// (The v0.7 handoff sketched this as a per-`PlanStep` field; it
    /// landed as one global config knob instead — a uniform backstop
    /// needs no per-step granularity and avoids breaking every
    /// `PlanStep` literal across callers. Per-step budgets can be added
    /// later if a real need appears.)
    pub step_time_budget: Option<Duration>,
}

impl Default for RunPlanConfig {
    fn default() -> Self {
        Self {
            shared: None,
            max_refinements_per_step: 2,
            max_concurrent: None,
            infra_retry: InfraRetryPolicy::default(),
            step_time_budget: None,
        }
    }
}

impl std::fmt::Debug for RunPlanConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `shared` is a type-erased `Arc<dyn Any>` (no `Debug`); show only its
        // presence.
        f.debug_struct("RunPlanConfig")
            .field("shared", &self.shared.is_some())
            .field("max_refinements_per_step", &self.max_refinements_per_step)
            .field("max_concurrent", &self.max_concurrent)
            .field("infra_retry", &self.infra_retry)
            .field("step_time_budget", &self.step_time_budget)
            .finish()
    }
}

/// How [`InfraRetryPolicy`] classifies a [`WorkerError`] — decides
/// whether `run_plan` retries the step or surfaces the failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfraClass {
    /// Provider rate-limit / 429 / "circuit open" / quota exhausted —
    /// retry with backoff (the work is fine; we're being throttled).
    RateLimited,
    /// Transient transport failure — network timeout / connection
    /// drop / 5xx / truncated stream. Retry with backoff.
    Transient,
    /// Not an infra failure — a worker bug, decode error, or a
    /// genuine bad result. Surface immediately; retrying would just
    /// spin on a deterministic failure.
    NotInfra,
}

/// Plan-level retry policy for infra failures. Lifts the
/// rate-limit / circuit-open / timeout retry loop OUT of every
/// [`Worker`] body and into the runtime, so a worker's `run` becomes
/// single-attempt and the policy lives in one place.
#[derive(Clone)]
pub struct InfraRetryPolicy {
    /// Per-step retry budget for infra failures (NOT counting the
    /// first attempt). `0` = no infra retry: any error surfaces
    /// immediately, exactly as before this policy existed.
    pub max_attempts: u32,
    /// Backoff before each retry attempt. If shorter than
    /// `max_attempts`, the last entry is reused for the remaining
    /// attempts; empty = retry with no delay.
    pub backoffs: Vec<Duration>,
    /// Maps a [`WorkerError`] to an [`InfraClass`]. Defaults to
    /// [`default_infra_classifier`], which string-matches common
    /// provider failure wording; override for provider-specific
    /// phrasing (arc passes its own to catch tars-provider
    /// "circuit open").
    pub classify: Arc<dyn Fn(&WorkerError) -> InfraClass + Send + Sync>,
}

impl Default for InfraRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 0,
            backoffs: Vec::new(),
            classify: Arc::new(default_infra_classifier),
        }
    }
}

impl fmt::Debug for InfraRetryPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InfraRetryPolicy")
            .field("max_attempts", &self.max_attempts)
            .field("backoffs", &self.backoffs)
            .field("classify", &"<fn>")
            .finish()
    }
}

/// Default infra classifier — case-insensitively string-matches the
/// error's `Display` for common provider failure modes. Conservative:
/// anything it doesn't recognise is [`InfraClass::NotInfra`] (no
/// retry), so a worker bug never spins the budget.
pub fn default_infra_classifier(e: &WorkerError) -> InfraClass {
    // Variant-matched cases first — more precise than string-sniffing.
    match e {
        // A worker panic is surfaced as Crashed. Treat as transient:
        // retrying isolates flaky panics (a race / OOM / transient FFI
        // hiccup) while a deterministic panic just exhausts the budget
        // and then surfaces — never silently. The win is that the panic
        // became a normal Err instead of unwinding the run_plan task.
        WorkerError::Crashed(_) => return InfraClass::Transient,
        // A step-budget timeout is by definition a transport-ish hang.
        WorkerError::TimedOut(_) => return InfraClass::Transient,
        _ => {}
    }
    let m = e.to_string().to_lowercase();
    if m.contains("rate limit")
        || m.contains("rate_limit")
        || m.contains("429")
        || m.contains("circuit open")
        || m.contains("circuit breaker")
        || m.contains("quota")
        || m.contains("too many requests")
        || m.contains("overloaded")
        || m.contains("resource exhausted")
    {
        return InfraClass::RateLimited;
    }
    if m.contains("timeout")
        || m.contains("timed out")
        || m.contains("connection reset")
        || m.contains("connection closed")
        || m.contains("connection refused")
        || m.contains("broken pipe")
        || m.contains("502")
        || m.contains("503")
        || m.contains("504")
        || m.contains("stream ended")
        || m.contains("eof while")
    {
        return InfraClass::Transient;
    }
    InfraClass::NotInfra
}

/// Extract a human-readable message from a caught panic payload.
/// `panic!("x")` / `panic!("{}", s)` land as `&str` / `String`; other
/// payloads (rare) fall back to a placeholder.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Pick the backoff for the given 0-based retry attempt, clamping to
/// the last configured value (or `0s` if none configured).
fn pick_backoff(backoffs: &[Duration], attempt: u32) -> Duration {
    if backoffs.is_empty() {
        return Duration::ZERO;
    }
    let idx = (attempt as usize).min(backoffs.len() - 1);
    backoffs[idx]
}

/// Per-step record in [`TaskOutcome`].
///
/// A step is either [`Completed`](Self::Completed) (worker ran +
/// critic approved — or no critic was configured) or
/// [`Skipped`](Self::Skipped) (the step's
/// [`StepCondition`](crate::StepCondition) evaluated false, or one
/// of its deps was itself skipped — cascade).
#[derive(Clone, Debug)]
pub enum StepOutcome {
    Completed {
        step_id: String,
        /// The worker's accepted output (always
        /// [`AgentMessage::PartialResult`]).
        result: AgentMessage,
        /// The critic's approving verdict, if a critic was
        /// configured. Synthesised as
        /// [`AgentMessage::Verdict`]`{ verdict: Approve, .. }` when
        /// `critic = None` so callers reading `verdict` don't have
        /// to special-case the no-critic flow.
        verdict: AgentMessage,
        /// How many refinement loops were needed before approval.
        /// `0` = approved on first attempt.
        refinement_attempts: u32,
    },
    Skipped {
        step_id: String,
        /// Human-readable cause — same string written into the
        /// trajectory's [`AgentEvent::StepSkipped::reason`].
        reason: String,
    },
}

impl StepOutcome {
    pub fn step_id(&self) -> &str {
        match self {
            Self::Completed { step_id, .. } | Self::Skipped { step_id, .. } => step_id,
        }
    }

    /// `Some((result, verdict, refinement_attempts))` for completed
    /// steps; `None` for skipped.
    pub fn as_completed(&self) -> Option<(&AgentMessage, &AgentMessage, u32)> {
        match self {
            Self::Completed {
                result,
                verdict,
                refinement_attempts,
                ..
            } => Some((result, verdict, *refinement_attempts)),
            Self::Skipped { .. } => None,
        }
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }
}

/// What [`run_plan`] returns on success — the executed plan plus
/// per-step outcomes in plan-declaration order (so
/// `outcome.steps[i].step_id() == plan.steps[i].id`).
#[derive(Clone, Debug)]
pub struct TaskOutcome {
    pub trajectory_id: TrajectoryId,
    pub plan: Plan,
    pub steps: Vec<StepOutcome>,
}

/// Errors [`run_plan`] surfaces. The trajectory is NOT closed by
/// `run_plan` itself — that's the caller's job (so [`crate::run_task`]
/// can keep one trajectory across multiple plan iterations during
/// replan). Every variant carries `trajectory_id` for forensics.
#[derive(Debug, Error)]
pub enum RunPlanError {
    /// Critic returned `Reject` for some step. Carries the failed
    /// plan + partial results so a higher-level replan loop (see
    /// [`crate::run_task`]) can prompt the orchestrator with the
    /// failed context. `completed` holds the parsed
    /// `PartialResult`s for steps that were approved before the
    /// reject landed; the rejected step itself is NOT in the map.
    #[error("rejected step `{rejected_step_id}`: {reason}")]
    Rejected {
        trajectory_id: TrajectoryId,
        plan: Plan,
        rejected_step_id: String,
        reason: String,
        completed: HashMap<String, AgentMessage>,
    },
    /// Critic kept asking for `Refine` and we hit
    /// `max_refinements_per_step`.
    #[error("refine exhausted (step `{step_id}`) after {attempts} attempts")]
    RefineExhausted {
        trajectory_id: TrajectoryId,
        step_id: String,
        attempts: u32,
    },
    /// A step's `worker_role` had no registered impl AND no
    /// `WorkerRegistry::default` was set. Surfaced before any step
    /// in the offending level starts.
    #[error("no worker registered for role `{role}` (step `{step_id}`)")]
    NoWorkerForRole {
        trajectory_id: TrajectoryId,
        role: String,
        step_id: String,
    },
    /// A worker returned a non-`PartialResult` message variant.
    /// Wraps the worker's own error chain when it raised, or
    /// synthesises one when the variant alone is the violation.
    #[error("worker (step `{step_id}`): {source}")]
    Worker {
        trajectory_id: TrajectoryId,
        step_id: String,
        #[source]
        source: WorkerError,
    },
    /// Critic LLM call failed or its output was malformed.
    #[error("critic (step `{step_id}`): {source}")]
    Critic {
        trajectory_id: TrajectoryId,
        step_id: String,
        #[source]
        source: CriticError,
    },
    /// Underlying agent step failed (LLM provider error, cancel,
    /// internal). Wraps [`AgentExecutionError`] from
    /// [`crate::execute_agent_step`].
    #[error("agent step (step `{step_id}`): {source}")]
    AgentStep {
        trajectory_id: TrajectoryId,
        step_id: String,
        #[source]
        source: AgentExecutionError,
    },
    /// Trajectory event-store write failed.
    #[error("runtime: {source}")]
    Runtime {
        trajectory_id: TrajectoryId,
        #[source]
        source: crate::RuntimeError,
    },
    /// Plan failed [`Plan::validate`]. The executor calls validate
    /// up front so a bad plan fails fast before any step runs.
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
}

impl RunPlanError {
    pub fn trajectory_id(&self) -> Option<&TrajectoryId> {
        match self {
            Self::Rejected { trajectory_id, .. }
            | Self::RefineExhausted { trajectory_id, .. }
            | Self::NoWorkerForRole { trajectory_id, .. }
            | Self::Worker { trajectory_id, .. }
            | Self::Critic { trajectory_id, .. }
            | Self::AgentStep { trajectory_id, .. }
            | Self::Runtime { trajectory_id, .. } => Some(trajectory_id),
            Self::InvalidPlan(_) => None,
        }
    }
}

// ─── run_plan ──────────────────────────────────────────────────────────

/// Internal step outcome — discriminates "approved with worker output"
/// from "rejected by critic" so the level loop can react to either
/// without conflating them with the hard-error path.
enum StepDecision {
    Approved(StepOutcome),
    Rejected { step_id: String, reason: String },
}

/// Execute `plan` against the supplied `workers` + optional `critic`,
/// appending lifecycle events to the existing `trajectory_id`. The
/// caller is responsible for creating + closing the trajectory
/// (e.g. [`Runtime::create_trajectory`] before, `TrajectoryCompleted`
/// / `TrajectoryAbandoned` after).
///
/// ## Scheduling
///
/// - **Dependency-driven, not depth-batched**: a step starts the
///   instant ALL its deps are resolved (completed or skipped), NOT
///   when its whole graph-depth tier finishes. A tier-2 step whose
///   single dep is done runs while unrelated tier-1 steps are still
///   in flight — wall-time collapses from `Σ tiers` toward the
///   longest dependency chain. (The legacy depth-batched grouping
///   still lives on [`Plan::execution_levels`] for callers that want
///   it, but `run_plan` no longer uses it.)
/// - **Concurrency cap**: [`RunPlanConfig::max_concurrent`] bounds the
///   number of simultaneously-running workers (owned-permit
///   semaphore). `None` = unbounded, matching the old per-tier peak.
/// - **Cancel-on-reject**: the first `Reject` (or hard error) fires
///   the run-wide cancel token; in-flight siblings observe
///   `cancel.cancelled()` and bail with [`StepError::Cancelled`],
///   which the scheduler drains + discards.
/// - **Skip-cascade + conditional**: when a step's deps all resolve it
///   is classified before dispatch. A step is skipped if any dep was
///   skipped (cascade, priority) or its
///   [`StepCondition`](crate::StepCondition) evaluates false against
///   the completed results. Skipped steps emit
///   [`AgentEvent::StepSkipped`] and land as
///   [`StepOutcome::Skipped`].
/// - **Infra retry**: [`RunPlanConfig::infra_retry`] re-runs a step
///   whose worker hit a rate-limit / circuit-open / timeout, up to
///   the configured budget (orthogonal to the refinement loop below).
/// - **Per-step refinement loop**: critic returns `Refine` → worker
///   re-runs with suggestions threaded; up to
///   `config.max_refinements_per_step` attempts. `Approve` ends the
///   step; `Reject` bubbles up as [`RunPlanError::Rejected`].
/// - **Plan-order outcome reassembly**: `outcome.steps[i].step_id()`
///   == `plan.steps[i].id`, including skipped entries.
///
/// ## Trajectory event log shape (per non-skipped step)
///
/// ```text
/// StepStarted(step_seq=N, agent="worker:<role>")
///   ↳ possibly LlmCallCaptured(step_seq=N)   # LLM workers only
/// StepCompleted(step_seq=N, usage=<from WorkerOutput>)
/// [if critic configured:]
/// StepStarted(step_seq=N+1, agent="critic:<id>")
///   ↳ possibly LlmCallCaptured(step_seq=N+1)
/// StepCompleted(step_seq=N+1, usage=<from critic>)
/// ```
///
/// (Refinement loops emit a fresh pair of `StepStarted` /
/// `StepCompleted` per attempt — one worker invocation, one critic
/// invocation, all sharing the parent step's `worker_role`.)
pub async fn run_plan(
    runtime: Arc<dyn Runtime>,
    trajectory_id: TrajectoryId,
    plan: Plan,
    workers: WorkerRegistry,
    critic: Option<Arc<dyn Critic>>,
    config: RunPlanConfig,
    cancel: CancellationToken,
) -> Result<TaskOutcome, RunPlanError> {
    // ── Pre-flight: plan validation + worker resolution ──────────────
    // Both checks happen up front so a bad plan fails before we
    // start emitting events. validate() also rules out cycles
    // (transitively, via the "deps point at earlier-declared steps"
    // rule), so the dependency-driven scheduler below always makes
    // progress — every non-skipped step's deps eventually resolve.
    plan.validate()
        .map_err(|e| RunPlanError::InvalidPlan(e.to_string()))?;
    for step in &plan.steps {
        if workers.find(&step.worker_role).is_none() {
            return Err(RunPlanError::NoWorkerForRole {
                trajectory_id: trajectory_id.clone(),
                role: step.worker_role.clone(),
                step_id: step.id.clone(),
            });
        }
    }

    let mut step_outcomes_by_id: HashMap<String, StepOutcome> =
        HashMap::with_capacity(plan.steps.len());
    let mut skipped_ids: HashSet<String> = HashSet::new();
    let mut hit_reject: Option<(String, String)> = None;

    // ── Dependency-driven scheduler ──────────────────────────────────
    // Replaces the historical depth-batched level loop. A step starts
    // the instant ALL its deps are resolved (completed or skipped) —
    // NOT when its whole graph-depth tier finishes. So in arc's
    // scan→fix→merge plan, `fix-50` starts as soon as `scan-50` lands
    // while `scan-99` is still in flight; wall-time collapses from
    // `Σ tiers` toward `longest dependency chain`.
    //
    // Preserved exactly from the level loop: skip-cascade + condition
    // semantics (cascade has priority over condition), cancel-on-reject,
    // first-error abort + drain, and plan-declaration-order reassembly.
    //
    // Concurrency: `completed` lives behind an Arc<Mutex> so the
    // scheduler can record a finished step's result while sibling
    // futures are still in flight (the level loop dodged this by only
    // mutating `completed` at each tier barrier). Each dispatched future
    // snapshots `completed` once before calling its worker — workers see
    // an immutable map, same as before. `max_concurrent` (if set) caps
    // simultaneously-running workers via an owned-permit semaphore, which
    // matters now that a wide root tier could otherwise fire every step
    // at once.
    let run_cancel = cancel.child_token();
    let sem = config
        .max_concurrent
        .map(|n| Arc::new(tokio::sync::Semaphore::new(n.max(1))));
    let completed_shared: Arc<std::sync::Mutex<HashMap<String, AgentMessage>>> = Arc::new(
        std::sync::Mutex::new(HashMap::with_capacity(plan.steps.len())),
    );

    let by_id: HashMap<&str, &PlanStep> = plan.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut pending: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();

    let workers_ref = &workers;
    let critic_ref = critic.as_deref();
    let runtime_ref = runtime.clone();
    let plan_ref = &plan;
    let traj_ref = &trajectory_id;
    let config_ref = &config;

    let mut tasks: FuturesUnordered<_> = FuturesUnordered::new();

    'schedule: loop {
        // ── 1. Promote every step whose deps are all resolved. ───────
        // Loops to a fixpoint because skipping a step resolves it,
        // which may cascade-skip / unlock dependents in the same pass.
        // Dispatched (RUN) steps do NOT resolve synchronously, so they
        // don't unlock dependents here — those wait for completion in
        // step 3. Suppressed entirely once a reject has landed.
        if hit_reject.is_none() {
            loop {
                let ready: Vec<&str> = pending
                    .iter()
                    .copied()
                    .filter(|id| {
                        by_id[id]
                            .depends_on
                            .iter()
                            .all(|d| step_outcomes_by_id.contains_key(d.as_str()))
                    })
                    .collect();
                if ready.is_empty() {
                    break;
                }
                let mut progressed = false;
                for id in ready {
                    let step = by_id[id];
                    // (a) cascade skip — priority over condition.
                    if let Some(dep) = step
                        .depends_on
                        .iter()
                        .find(|d| skipped_ids.contains(d.as_str()))
                    {
                        let reason = format!("dep `{dep}` was skipped");
                        log_skip(&runtime, traj_ref, step, &reason).await?;
                        skipped_ids.insert(step.id.clone());
                        step_outcomes_by_id.insert(
                            step.id.clone(),
                            StepOutcome::Skipped {
                                step_id: step.id.clone(),
                                reason,
                            },
                        );
                        pending.remove(id);
                        progressed = true;
                        continue;
                    }
                    // (b) condition skip — evaluate against completed.
                    let cond_ok = {
                        let g = completed_shared.lock().unwrap();
                        step.condition.matches(&g)
                    };
                    if !cond_ok {
                        let reason = step.condition.skip_reason();
                        log_skip(&runtime, traj_ref, step, &reason).await?;
                        skipped_ids.insert(step.id.clone());
                        step_outcomes_by_id.insert(
                            step.id.clone(),
                            StepOutcome::Skipped {
                                step_id: step.id.clone(),
                                reason,
                            },
                        );
                        pending.remove(id);
                        progressed = true;
                        continue;
                    }
                    // (c) RUN — dispatch into the in-flight set.
                    pending.remove(id);
                    let cancel = run_cancel.clone();
                    let step_id_for_label = step.id.clone();
                    let runtime = runtime_ref.clone();
                    let completed_shared = completed_shared.clone();
                    let sem = sem.clone();
                    tasks.push(async move {
                        let _permit = match sem {
                            Some(s) => Some(s.acquire_owned().await.expect("semaphore not closed")),
                            None => None,
                        };
                        // Snapshot the results map once; the worker sees
                        // an immutable view for its whole run while the
                        // scheduler keeps recording other steps.
                        let snapshot = completed_shared.lock().unwrap().clone();
                        let res = run_one_step(
                            runtime,
                            traj_ref,
                            plan_ref,
                            step,
                            &snapshot,
                            workers_ref,
                            critic_ref,
                            config_ref,
                            cancel,
                        )
                        .await;
                        (step_id_for_label, res)
                    });
                    progressed = true;
                }
                if !progressed {
                    break;
                }
            }
        }

        // ── 2. Done when nothing is in flight. ───────────────────────
        // After a reject we stop promoting (step 1 guarded off) and just
        // drain the in-flight set here until it empties, then break.
        if tasks.is_empty() {
            break 'schedule;
        }

        // ── 3. React to the next completion. ─────────────────────────
        let (step_id, step_res) = tasks.next().await.expect("tasks is non-empty");
        match step_res {
            Ok(StepDecision::Approved(outcome)) => {
                // Suppress commits once a reject has landed — matches the
                // level loop, where in-tier approvals after a reject were
                // discarded. The plan is failing; these results are moot.
                if hit_reject.is_none() {
                    if let StepOutcome::Completed { result, .. } = &outcome {
                        completed_shared
                            .lock()
                            .unwrap()
                            .insert(step_id.clone(), result.clone());
                    }
                    step_outcomes_by_id.insert(step_id, outcome);
                }
            }
            Ok(StepDecision::Rejected {
                step_id: rejected_id,
                reason,
            }) => {
                if hit_reject.is_none() {
                    hit_reject = Some((rejected_id, reason));
                    // Tell still-running siblings to bail; their
                    // Cancelled returns get drained on subsequent passes.
                    run_cancel.cancel();
                }
            }
            Err(e) => {
                if hit_reject.is_some() && is_cancellation_err(&e) {
                    continue;
                }
                run_cancel.cancel();
                while let Some((_, drained_res)) = tasks.next().await {
                    if let Err(drained_err) = &drained_res
                        && is_cancellation_err(drained_err)
                    {
                        continue;
                    }
                    tracing::warn!(
                        trajectory_id = %trajectory_id,
                        "run_plan: discarding drained-sibling result after first-failure cancel (cause: {e})",
                    );
                }
                return Err(e);
            }
        }
    }
    drop(tasks);

    // Reclaim the results map for the Rejected error payload below. All
    // dispatched futures are done (tasks drained), so the scheduler is
    // the sole Arc holder; fall back to a clone if any straggler clone
    // somehow outlives (it shouldn't).
    let completed: HashMap<String, AgentMessage> = Arc::try_unwrap(completed_shared)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone());

    // ── Decide: success or Rejected ──────────────────────────────────
    if let Some((rejected_step_id, reason)) = hit_reject {
        return Err(RunPlanError::Rejected {
            trajectory_id: trajectory_id.clone(),
            plan,
            rejected_step_id,
            reason,
            completed,
        });
    }

    // ── Reassemble in plan-declaration order. ────────────────────────
    let step_outcomes: Vec<StepOutcome> = plan
        .steps
        .iter()
        .map(|s| {
            step_outcomes_by_id.remove(&s.id).expect(
                "every plan step was classified as Completed, Skipped, or surfaced as Reject",
            )
        })
        .collect();

    Ok(TaskOutcome {
        trajectory_id,
        plan,
        steps: step_outcomes,
    })
}

// ─── Per-step inner loop ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_one_step(
    runtime: Arc<dyn Runtime>,
    trajectory_id: &TrajectoryId,
    plan: &Plan,
    step: &PlanStep,
    completed: &HashMap<String, AgentMessage>,
    workers: &WorkerRegistry,
    critic: Option<&(dyn Critic + '_)>,
    config: &RunPlanConfig,
    cancel: CancellationToken,
) -> Result<StepDecision, RunPlanError> {
    let worker = workers
        .find(&step.worker_role)
        .expect("pre-flight ensured every step.worker_role has a registered impl");
    let mut refinements: Vec<String> = Vec::new();
    let mut attempts: u32 = 0;
    loop {
        // ── Worker invocation (with infra retry) ─────────────────────
        // Worker is responsible for its own StepStarted/Completed/Failed
        // events (LLM workers via execute_agent_step, non-LLM via
        // emit_step_lifecycle or manual append). Executor only enforces
        // the WorkerOutput shape invariant + error mapping.
        //
        // Infra failures (rate-limit / circuit-open / timeout) are
        // retried HERE per `config.infra_retry` instead of inside each
        // worker — the policy lives in one place and the worker body
        // stays single-attempt. A `Refine` retry is a different axis
        // (handled by the outer `loop` + `attempts`): that re-runs
        // because the OUTPUT needs work; infra retry re-runs because
        // the TRANSPORT failed.
        let worker_msg = {
            let mut infra_attempts: u32 = 0;
            loop {
                let wctx = WorkerContext {
                    runtime: runtime.clone(),
                    trajectory_id: trajectory_id.clone(),
                    cancel: cancel.clone(),
                    refinements: refinements.clone(),
                    // Injected from RunPlanConfig so every step on this run sees
                    // the same run-scoped blackboard handle.
                    shared: config.shared.clone(),
                };
                // Run the worker under two backstops, both per-attempt:
                //  - catch_unwind (P2): a panic in one worker becomes a
                //    `WorkerError::Crashed` instead of unwinding the whole
                //    run_plan task — crash isolation for a 200-step plan.
                //  - step_time_budget (P3): a hung call is aborted as
                //    `WorkerError::TimedOut`. Both classify Transient, so
                //    infra_retry can retry them.
                let invoke =
                    AssertUnwindSafe(worker.run(plan, step, completed, wctx)).catch_unwind();
                let run_result: Result<WorkerOutput, WorkerError> = match config.step_time_budget {
                    Some(budget) => match tokio::time::timeout(budget, invoke).await {
                        Ok(caught) => {
                            caught.unwrap_or_else(|p| Err(WorkerError::Crashed(panic_message(p))))
                        }
                        Err(_elapsed) => Err(WorkerError::TimedOut(format!(
                            "step `{}` exceeded {:?}",
                            step.id, budget,
                        ))),
                    },
                    None => invoke
                        .await
                        .unwrap_or_else(|p| Err(WorkerError::Crashed(panic_message(p)))),
                };
                match run_result {
                    Ok(out) => {
                        if !matches!(out.message, AgentMessage::PartialResult { .. }) {
                            return Err(RunPlanError::Worker {
                                trajectory_id: trajectory_id.clone(),
                                step_id: step.id.clone(),
                                source: WorkerError::UnexpectedOutput(format!(
                                    "Worker::run must return AgentMessage::PartialResult; got {:?}",
                                    out.message,
                                )),
                            });
                        }
                        // `out.usage` is informational at this layer — the
                        // worker's own StepCompleted event already captured
                        // it for RunReport rollup. We don't re-emit here.
                        let _ = out.usage;
                        break out.message;
                    }
                    Err(e) => {
                        let class = (config.infra_retry.classify)(&e);
                        let retryable =
                            matches!(class, InfraClass::RateLimited | InfraClass::Transient);
                        if retryable && infra_attempts < config.infra_retry.max_attempts {
                            let backoff =
                                pick_backoff(&config.infra_retry.backoffs, infra_attempts);
                            tracing::warn!(
                                trajectory_id = %trajectory_id,
                                step_id = %step.id,
                                class = ?class,
                                attempt = infra_attempts + 1,
                                max = config.infra_retry.max_attempts,
                                backoff_ms = backoff.as_millis() as u64,
                                "run_plan: infra failure, retrying after backoff ({e})",
                            );
                            // Honour cancellation during the backoff so a
                            // sibling's reject/error doesn't wait out the
                            // sleep before this step bails.
                            tokio::select! {
                                _ = cancel.cancelled() => {
                                    return Err(map_worker_error(
                                        trajectory_id,
                                        step.id.clone(),
                                        e,
                                    ));
                                }
                                _ = tokio::time::sleep(backoff) => {}
                            }
                            infra_attempts += 1;
                            continue;
                        }
                        return Err(map_worker_error(trajectory_id, step.id.clone(), e));
                    }
                }
            }
        };

        // ── Critic invocation (optional) ─────────────────────────────
        // Same lifecycle contract as Worker: Critic owns its own
        // StepStarted/Completed events. When `critic = None` we
        // synthesise an Approve verdict without emitting anything
        // (no extra step in the trajectory log either — there's no
        // judging activity to record).
        let verdict_msg = match critic {
            None => AgentMessage::Verdict {
                from_agent: tars_types::AgentId::new("no-critic"),
                target_step_id: Some(step.id.clone()),
                verdict: VerdictKind::Approve,
            },
            Some(c) => {
                let cctx = CriticContext {
                    runtime: runtime.clone(),
                    trajectory_id: trajectory_id.clone(),
                    cancel: cancel.clone(),
                };
                match c.judge(plan, step, &worker_msg, cctx).await {
                    Ok(v) => AgentMessage::Verdict {
                        from_agent: tars_types::AgentId::new("critic"),
                        target_step_id: Some(step.id.clone()),
                        verdict: v,
                    },
                    Err(e) => return Err(map_critic_error(trajectory_id, step.id.clone(), e)),
                }
            }
        };

        // ── Verdict dispatch ─────────────────────────────────────────
        let verdict_kind = match &verdict_msg {
            AgentMessage::Verdict { verdict, .. } => verdict.clone(),
            other => {
                return Err(RunPlanError::Critic {
                    trajectory_id: trajectory_id.clone(),
                    step_id: step.id.clone(),
                    source: CriticError::UnexpectedOutput(format!(
                        "expected Verdict envelope; got {other:?}",
                    )),
                });
            }
        };
        match verdict_kind {
            VerdictKind::Approve => {
                return Ok(StepDecision::Approved(StepOutcome::Completed {
                    step_id: step.id.clone(),
                    result: worker_msg,
                    verdict: verdict_msg,
                    refinement_attempts: attempts,
                }));
            }
            VerdictKind::Reject { reason } => {
                return Ok(StepDecision::Rejected {
                    step_id: step.id.clone(),
                    reason,
                });
            }
            VerdictKind::Refine { suggestions } => {
                if attempts >= config.max_refinements_per_step {
                    return Err(RunPlanError::RefineExhausted {
                        trajectory_id: trajectory_id.clone(),
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

// ─── Trajectory event helpers ──────────────────────────────────────────

/// Convenience for non-LLM [`Worker`] implementations: wraps `body`
/// in a `StepStarted` → ... → `StepCompleted` / `StepFailed` event
/// triple, with `step_seq` allocated via
/// [`Runtime::next_step_seq`].
///
/// LLM workers should NOT call this — they should call
/// [`crate::execute_agent_step`] which produces the same event
/// shape plus `LlmCallCaptured`.
///
/// The body returns `(T, Usage)` on success — the `Usage` is folded
/// into the `StepCompleted` event for cost rollup. Pass
/// `Usage::default()` if the worker does no LLM / token-billable
/// work.
///
/// ## Example
///
/// ```ignore
/// #[async_trait::async_trait]
/// impl Worker for MergeSweepWorker {
///     async fn run(
///         &self,
///         _plan: &Plan,
///         step: &PlanStep,
///         _prior: &HashMap<String, AgentMessage>,
///         ctx: WorkerContext,
///     ) -> Result<WorkerOutput, WorkerError> {
///         let (report, usage) = emit_step_lifecycle(
///             &ctx.runtime,
///             &ctx.trajectory_id,
///             &format!("worker:{}", step.worker_role),
///             format!("merge_sweep for step {}", step.id),
///             |_step_seq| async {
///                 let r = merge_sweep::run(&self.repo).await?;
///                 Ok::<_, MergeSweepError>((r, Usage::default()))
///             },
///         )
///         .await
///         .map_err(|e| WorkerError::InvalidResult(e.to_string()))?;
///         Ok(WorkerOutput {
///             message: AgentMessage::PartialResult {
///                 from_agent: AgentId::new("arc-merge-sweep"),
///                 step_id: Some(step.id.clone()),
///                 summary: format!("{} applied", report.ok),
///                 confidence: 1.0,
///             },
///             usage,
///         })
///     }
/// }
/// ```
pub async fn emit_step_lifecycle<T, E, F, Fut>(
    runtime: &Arc<dyn Runtime>,
    traj: &TrajectoryId,
    agent_label: &str,
    input_summary: impl Into<String>,
    body: F,
) -> Result<(T, Usage), E>
where
    F: FnOnce(u32) -> Fut,
    Fut: std::future::Future<Output = Result<(T, Usage), E>>,
    E: std::fmt::Display,
{
    let input_summary = input_summary.into();
    // Best-effort: a runtime.next_step_seq failure here would
    // mean the event store is broken — we still call `body` with
    // step_seq=0 so the worker can do its work, but the trajectory
    // log will be missing this lifecycle. Same posture as the
    // existing `abandon` / `TrajectoryCompleted` helpers in task.rs:
    // never fail the work over a logging hiccup.
    let step_seq = match runtime.next_step_seq(traj).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "emit_step_lifecycle: next_step_seq failed; using 0");
            0
        }
    };
    let idempotency_key = crate::event::StepIdempotencyKey::compute(traj, step_seq, &input_summary);
    let _ = runtime
        .append(
            traj,
            AgentEvent::StepStarted {
                traj: traj.clone(),
                step_seq,
                agent: agent_label.to_string(),
                idempotency_key,
                input_summary: input_summary.clone(),
            },
        )
        .await;
    let result = body(step_seq).await;
    match &result {
        Ok((_, usage)) => {
            let _ = runtime
                .append(
                    traj,
                    AgentEvent::StepCompleted {
                        traj: traj.clone(),
                        step_seq,
                        output_summary: input_summary,
                        usage: *usage,
                    },
                )
                .await;
        }
        Err(e) => {
            let _ = runtime
                .append(
                    traj,
                    AgentEvent::StepFailed {
                        traj: traj.clone(),
                        step_seq,
                        error: e.to_string(),
                        classification: "permanent".into(),
                    },
                )
                .await;
        }
    }
    result
}

async fn log_skip(
    runtime: &Arc<dyn Runtime>,
    traj: &TrajectoryId,
    step: &PlanStep,
    reason: &str,
) -> Result<(), RunPlanError> {
    let step_seq = runtime
        .next_step_seq(traj)
        .await
        .map_err(|source| RunPlanError::Runtime {
            trajectory_id: traj.clone(),
            source,
        })?;
    runtime
        .append(
            traj,
            AgentEvent::StepSkipped {
                traj: traj.clone(),
                step_seq,
                agent: format!("worker:{}", step.worker_role),
                plan_step_id: step.id.clone(),
                reason: reason.to_string(),
            },
        )
        .await
        .map_err(|source| RunPlanError::Runtime {
            trajectory_id: traj.clone(),
            source,
        })?;
    Ok(())
}

// ─── Error mapping ─────────────────────────────────────────────────────

fn map_worker_error(traj: &TrajectoryId, step_id: String, e: WorkerError) -> RunPlanError {
    // LLM-backed workers bubble `WorkerError::Agent(StepError)`;
    // surface those as `AgentStep` so the cancel-detection helper +
    // existing `RunTaskError::AgentStep` mapping in `run_task`
    // continue to recognise them. Non-LLM workers' decode / shape
    // errors land in the catch-all Worker variant.
    match e {
        WorkerError::Agent(agent_err) => RunPlanError::AgentStep {
            trajectory_id: traj.clone(),
            step_id,
            source: AgentExecutionError::Agent(agent_err),
        },
        other => RunPlanError::Worker {
            trajectory_id: traj.clone(),
            step_id,
            source: other,
        },
    }
}

fn map_critic_error(traj: &TrajectoryId, step_id: String, e: CriticError) -> RunPlanError {
    match e {
        CriticError::Agent(agent_err) => RunPlanError::AgentStep {
            trajectory_id: traj.clone(),
            step_id,
            source: AgentExecutionError::Agent(agent_err),
        },
        other => RunPlanError::Critic {
            trajectory_id: traj.clone(),
            step_id,
            source: other,
        },
    }
}

/// True iff `err` is the synthetic "cancelled" return that
/// post-reject sibling tasks come back with. The level loop
/// discards these to avoid abandoning the trajectory on what is
/// expected drain behaviour.
fn is_cancellation_err(err: &RunPlanError) -> bool {
    matches!(
        err,
        RunPlanError::AgentStep {
            source: AgentExecutionError::Agent(StepError::Cancelled),
            ..
        }
    )
}
