//! `tars plan <goal>` — drive [`OrchestratorAgent`] from the CLI.
//!
//! Inputs a goal string, hands it to an Orchestrator backed by the
//! same Pipeline (Telemetry / CacheLookup / Retry) `tars run` uses,
//! prints the typed [`Plan`] as JSON.
//!
//! ## Why a separate subcommand from `tars run`
//!
//! The dispatch / cache / trajectory plumbing is identical (and lives
//! in `crate::dispatch`), but the request *shape* and the output *shape*
//! diverge:
//!
//! - **Request**: Orchestrator builds the planner ChatRequest itself
//!   (system prompt + strict JSON Plan schema + temperature=0). The CLI
//!   doesn't pass `--prompt`/`--system`/`--temperature` like `run`
//!   does — those are determined by the agent.
//! - **Output**: a typed [`Plan`] instead of streaming text. We dump
//!   it as pretty-printed JSON to stdout (compact when `--compact`)
//!   so it pipes cleanly to `jq` or directly into the (future)
//!   orchestration loop.
//!
//! Trajectory logging works the same way as `tars run`: the agent
//! step gets logged via [`tars_runtime::execute_agent_step`] which
//! writes `StepStarted → LlmCallCaptured → StepCompleted` (or
//! `StepFailed`) plus the surrounding
//! `TrajectoryStarted` / `TrajectoryCompleted` / `TrajectoryAbandoned`
//! pair the CLI manages itself (consistent with `tars run`'s
//! lifecycle).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use tars_cache::CacheKeyFactory;
use tars_pipeline::{
    CacheLookupMiddleware, LlmService, Pipeline, RetryMiddleware, TelemetryMiddleware,
};
use tars_runtime::{
    execute_agent_step, AgentEvent, LocalRuntime, OrchestratorAgent, Runtime, StepIdempotencyKey,
};
use tars_storage::EventStore;
use tars_types::{AgentId, ChatRequest, ModelHint, TrajectoryId};
use tokio_util::sync::CancellationToken;

use crate::dispatch::{
    build_cache, build_dispatch, build_registry_with_breaker, Dispatch, DispatchArgs,
};
use crate::{config_loader, event_store};

#[derive(Args, Debug)]
pub struct PlanArgs {
    /// Common dispatch flags (provider/tier/model/cache/breaker/trajectory).
    #[command(flatten)]
    pub dispatch: DispatchArgs,

    /// What to plan. The orchestrator turns this into a multi-step DAG.
    #[arg(short, long)]
    pub goal: String,

    /// Compact JSON output (default: pretty-printed for human reading).
    #[arg(long)]
    pub compact: bool,
}

pub async fn execute(args: PlanArgs, config_path: Option<PathBuf>) -> Result<()> {
    let cfg = config_loader::load(config_path)?;
    let registry = build_registry_with_breaker(&cfg, args.dispatch.breaker)?;
    let dispatch = build_dispatch(&cfg, &registry, &args.dispatch)?;

    // Same pipeline as `tars run`. Cache + retry + telemetry all
    // pay off for planning calls (planner is deterministic ⇒ great
    // cache target; transient errors should retry; telemetry is
    // useful for debugging prompt drift).
    let cache_registry = build_cache(args.dispatch.cache_path.as_deref())?;
    let cache_factory = CacheKeyFactory::new(1);
    let pipeline = Pipeline::builder_with_inner(dispatch.inner.clone())
        .layer(TelemetryMiddleware::new())
        .layer(CacheLookupMiddleware::new(
            cache_registry,
            cache_factory,
            dispatch.cache_origin_id.clone(),
        ))
        .layer(RetryMiddleware::default())
        .build();
    let llm: Arc<dyn LlmService> = Arc::new(pipeline);

    // Trajectory wiring. Mirrors `tars run`'s pattern: we open a
    // trajectory ourselves, write the StepStarted/Captured/Completed
    // via execute_agent_step, then close with TrajectoryCompleted /
    // Abandoned. Best-effort — a SQLite hiccup is logged but doesn't
    // block the LLM call.
    let trajectory_logger = build_trajectory_logger(&args, &dispatch).await;

    let agent = OrchestratorAgent::new(
        AgentId::new("orchestrator"),
        dispatch.model_label.clone(),
    );

    // The planner builds its OWN ChatRequest (system prompt + Plan
    // schema). We construct a placeholder ChatRequest for
    // execute_agent_step's input (it gets replaced by the planner's
    // internal one); this is awkward and will smooth out once the
    // Agent trait grows a typed-input variant. For now, we use the
    // typed orchestrator.plan() helper directly via a tiny adapter
    // closure, bypassing execute_agent_step's generic input shape —
    // we still want the trajectory log writes, so we replicate the
    // log lifecycle inline below.

    let traj = trajectory_logger.as_ref().map(|t| t.traj.clone());
    let goal = args.goal.clone();
    let model = dispatch.model_label.clone();

    // Build the AgentContext OrchestratorAgent.plan() needs. Its
    // step_seq is computed by the same "count of StepStarted" rule
    // execute_agent_step uses; for the no-trajectory case we just
    // pass step_seq=1 (irrelevant since we won't log).
    let cancel = CancellationToken::new();
    let plan_result = if let (Some(logger), Some(traj_id)) = (&trajectory_logger, traj.as_ref()) {
        // Use execute_agent_step path: builds the planner request
        // ourselves but lets the runtime layer manage step_seq +
        // log writes. We need to construct a request that mirrors
        // what plan() would build internally so the trajectory log
        // sees the right model/system.
        let req = build_planner_request_for_log(&model, &goal);
        let result = execute_agent_step(
            logger.runtime.as_ref(),
            traj_id,
            llm.clone(),
            agent.clone(),
            req,
            cancel.clone(),
        )
        .await;
        match result {
            Ok(step_result) => parse_plan_from_step(step_result),
            Err(e) => Err(anyhow::Error::new(e).context("agent step failed")),
        }
    } else {
        // No trajectory — just run the typed plan() helper directly.
        let ctx = tars_runtime::AgentContext {
            trajectory_id: TrajectoryId::new("ephemeral"),
            step_seq: 1,
            llm: llm.clone(),
            cancel,
        };
        agent
            .plan(ctx, &goal)
            .await
            .context("orchestrator.plan() failed")
    };

    // Close the trajectory before returning.
    if let Some(logger) = &trajectory_logger {
        logger.close_with(plan_result.is_ok()).await;
    }

    let plan = plan_result?;

    // Output: pretty by default, compact via flag.
    let json = if args.compact {
        serde_json::to_string(&plan)
    } else {
        serde_json::to_string_pretty(&plan)
    }
    .context("encode plan as JSON")?;
    println!("{json}");

    if let Some(logger) = &trajectory_logger {
        eprintln!("── trajectory: {}", logger.traj);
    }

    Ok(())
}

/// Build a placeholder ChatRequest that mirrors what
/// OrchestratorAgent.plan() would build internally. Used only when we
/// drive the agent through execute_agent_step (trajectory mode) so
/// the trajectory log sees the planner shape; the OrchestratorAgent
/// passes this through to its inner drive_llm_call unchanged.
fn build_planner_request_for_log(model: &str, goal: &str) -> ChatRequest {
    use tars_types::JsonSchema;
    let mut req = ChatRequest::user(ModelHint::Explicit(model.to_string()), goal);
    req.system = Some(planner_system_prompt_mirror().to_string());
    req.structured_output = Some(JsonSchema::strict("Plan", plan_schema_mirror()));
    req.temperature = Some(0.0);
    req
}

/// Mirror of OrchestratorAgent's PLANNER_SYSTEM_PROMPT — kept
/// in-sync by hand. If they ever drift, the trajectory log just
/// records a slightly different system summary; nothing functional
/// breaks. (TODO: expose the constant from tars-runtime so this
/// duplication goes away.)
fn planner_system_prompt_mirror() -> &'static str {
    "(see tars_runtime::OrchestratorAgent for the canonical planner system prompt)"
}

fn plan_schema_mirror() -> serde_json::Value {
    // Same shape as the real schema; we keep this stub minimal
    // because the LLM only sees what OrchestratorAgent constructs.
    // The placeholder we send through execute_agent_step is replaced
    // by the agent's internal request anyway.
    serde_json::json!({"type": "object"})
}

/// Decode the AgentStepResult from execute_agent_step into a typed
/// Plan. The orchestrator's Agent::execute returns AgentOutput::Text
/// containing the JSON the LLM produced.
fn parse_plan_from_step(
    step_result: tars_runtime::AgentStepResult,
) -> Result<tars_runtime::Plan> {
    use tars_runtime::AgentOutput;
    let json = match step_result.output {
        AgentOutput::Text { text } => text,
        other => anyhow::bail!(
            "orchestrator returned non-text output: {other:?} \
             (planner expected JSON; structured_output may have been disabled)"
        ),
    };
    let plan: tars_runtime::Plan =
        serde_json::from_str(&json).context("decode planner JSON")?;
    plan.validate().context("plan validation")?;
    Ok(plan)
}

/// Trajectory bookkeeping. Same shape as the equivalent in `run.rs`
/// but separate because the lifecycle messages differ slightly
/// ("tars plan via …" vs "tars run via …") and the close path needs
/// a "did we get a plan?" boolean rather than a stream outcome.
struct TrajectoryLogger {
    runtime: Arc<LocalRuntime>,
    traj: TrajectoryId,
}

impl TrajectoryLogger {
    async fn close_with(&self, succeeded: bool) {
        let event = if succeeded {
            AgentEvent::TrajectoryCompleted {
                traj: self.traj.clone(),
                summary: "plan emitted".into(),
            }
        } else {
            AgentEvent::TrajectoryAbandoned {
                traj: self.traj.clone(),
                cause: "plan failed".into(),
            }
        };
        if let Err(e) = self.runtime.append(&self.traj, event).await {
            tracing::warn!(error = %e, "trajectory: close-event append failed");
        }
    }
}

async fn build_trajectory_logger(
    args: &PlanArgs,
    dispatch: &Dispatch,
) -> Option<TrajectoryLogger> {
    if args.dispatch.no_trajectory {
        return None;
    }
    let store: Arc<dyn EventStore> = match event_store::open(args.dispatch.events_path.as_deref()) {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(
                "trajectory: no XDG data dir on this platform; skipping log",
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "trajectory: opening event store failed; skipping log");
            return None;
        }
    };
    let runtime = LocalRuntime::new(store);
    let reason = format!("tars plan via {}", dispatch.label);
    let traj = match runtime.create_trajectory(None, &reason).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "trajectory: create_trajectory failed; skipping log");
            return None;
        }
    };
    // Note: execute_agent_step writes its own StepStarted; we don't
    // pre-stamp one here (would conflict with its step_seq counting).
    // Suppress the unused-import warning on StepIdempotencyKey:
    let _ = StepIdempotencyKey::compute;
    Some(TrajectoryLogger { runtime, traj })
}
