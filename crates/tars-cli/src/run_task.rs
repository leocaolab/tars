//! `tars run-task <goal>` — drive the multi-step agent triad
//! ([`OrchestratorAgent`] → [`WorkerAgent`] → [`CriticAgent`]) from
//! the CLI.
//!
//! The user-facing M3 entry point. Inputs a goal string, hands it to
//! [`tars_runtime::run_task`], prints the [`TaskOutcome`] (plan + per-step
//! results) — or the typed error and trajectory id on failure so the
//! user can `tars trajectory show <ID>` to debug.
//!
//! ## Why a separate subcommand from `tars plan`
//!
//! `tars plan` is "show me what the Orchestrator would propose"; it
//! makes ONE LLM call. `tars run-task` is "actually execute the plan
//! end-to-end": one Orchestrator call + per step (Worker + Critic),
//! with Critic-driven Refine retries up to `--max-refinements`.
//! Different cost profile, different output shape — keeping them
//! separate keeps the menu honest.
//!
//! ## Trajectory
//!
//! [`tars_runtime::run_task`] manages its own trajectory lifecycle
//! (creates `TrajectoryStarted` at the top, closes with
//! `TrajectoryCompleted` on success or `TrajectoryAbandoned` on any
//! failure). All this CLI does is hand it a runtime backed by either
//! the persistent SQLite event store (default) or an in-memory store
//! (`--no-trajectory`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use tars_cache::CacheKeyFactory;
use tars_pipeline::{
    CacheLookupMiddleware, LlmService, Pipeline, RetryMiddleware, TelemetryMiddleware,
};
use tars_runtime::{
    run_task, AgentMessage, CriticAgent, LocalRuntime, OrchestratorAgent, Runtime, RunTaskConfig,
    RunTaskError, TaskOutcome, VerdictKind, WorkerAgent,
};
use tars_storage::{EventStore, SqliteEventStore};
use tars_tools::{builtins::ReadFileTool, ToolRegistry};
use tars_types::AgentId;
use tokio_util::sync::CancellationToken;

use crate::config_loader;
use crate::dispatch::{
    build_cache, build_dispatch, build_registry_with_breaker, DispatchArgs,
};
use crate::event_store;

#[derive(Args, Debug)]
pub struct RunTaskArgs {
    /// Common dispatch flags (provider/tier/model/cache/breaker/trajectory).
    #[command(flatten)]
    pub dispatch: DispatchArgs,

    /// What to do. The Orchestrator turns this into a multi-step plan;
    /// each step gets a Worker → Critic pass.
    #[arg(short, long)]
    pub goal: String,

    /// Cap on Critic-driven Refine retries per plan step. `0` means
    /// "Worker gets exactly one chance, any Refine verdict fails the
    /// task". Default: 2 (matches `RunTaskConfig::default`).
    #[arg(long, default_value_t = 2)]
    pub max_refinements: u32,

    /// Free-form Worker domain label (`summarise`, `code_review`, …).
    /// Surfaces in `AgentRole::Worker { domain }` and the trajectory log;
    /// today's stub Worker has no domain-specific behaviour, so it's
    /// purely diagnostic. Defaults to `general`.
    #[arg(long, default_value = "general")]
    pub worker_domain: String,

    /// Print the full TaskOutcome as JSON to stdout instead of the
    /// human-readable format. Useful for piping into `jq` or feeding
    /// into another tool.
    #[arg(long)]
    pub json: bool,

    /// Enable the default safe tool set on the WorkerAgent. Today
    /// that's `fs.read_file` only — read-only, jailed to
    /// `--tools-root` (default: current working directory). Without
    /// this flag the Worker is the no-tools stub flavour.
    ///
    /// As more built-in tools land (`fs.list_dir`, `git.fetch_pr_diff`,
    /// etc.) they'll join this default set. Tools with externally-
    /// visible side effects (`fs.write_file`, `shell.exec`) won't —
    /// they need explicit opt-in flags so the safe baseline stays
    /// safe.
    #[arg(long)]
    pub tools: bool,

    /// Directory the Worker's filesystem tools are jailed to. Only
    /// consulted when `--tools` is set. Default: process cwd. Symlinks
    /// resolving outside this root are also rejected.
    #[arg(long, value_name = "PATH")]
    pub tools_root: Option<PathBuf>,
}

pub async fn execute(args: RunTaskArgs, config_path: Option<PathBuf>) -> Result<()> {
    let cfg = config_loader::load(config_path)?;
    let registry = build_registry_with_breaker(&cfg, args.dispatch.breaker)?;
    let dispatch = build_dispatch(&cfg, &registry, &args.dispatch)?;

    // Same pipeline shape `tars plan` uses — Telemetry + Cache + Retry
    // pay off here too (cache especially: temperature=0 across all 3
    // agents means deterministic re-runs hit the cache cleanly).
    let cache_registry = build_cache(args.dispatch.cache_path.as_deref());
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

    // Runtime: persistent SQLite by default; in-memory for `--no-trajectory`.
    // run_task needs a real Runtime regardless — even in --no-trajectory mode
    // it creates events, they just go to a throwaway store.
    let runtime = build_runtime(&args).await?;

    // Same model across all 3 agents for MVP. Differentiated routing
    // (planner=opus, worker=sonnet, critic=haiku) is a future flag —
    // every agent currently calls through the same Pipeline so the
    // tier flag already gives some leverage.
    let model = dispatch.model_label.clone();
    let orchestrator = OrchestratorAgent::new(AgentId::new("orchestrator"), model.clone());
    let worker = build_worker(&args, model.clone())?;
    let critic = CriticAgent::new(AgentId::new("critic"), model);

    let config = RunTaskConfig { max_refinements_per_step: args.max_refinements };
    let cancel = CancellationToken::new();

    let outcome = run_task(
        runtime,
        llm,
        orchestrator,
        worker,
        critic,
        &args.goal,
        config,
        cancel,
    )
    .await;

    match outcome {
        Ok(o) => {
            if args.json {
                print_outcome_json(&o)?;
            } else {
                print_outcome_human(&o);
            }
            eprintln!("── trajectory: {}", o.trajectory_id);
            Ok(())
        }
        Err(e) => {
            // Always print the trajectory id so the user can run
            // `tars trajectory show <ID>` to inspect what happened.
            eprintln!("── trajectory: {}", e.trajectory_id());
            Err(into_anyhow(e))
        }
    }
}

/// Construct the WorkerAgent based on `--tools` / `--tools-root`. When
/// `--tools` is unset returns the no-tools stub flavour (preserves the
/// behaviour we shipped in `959be20`); when set, registers the default
/// safe tool set jailed to `--tools-root` (or cwd).
fn build_worker(args: &RunTaskArgs, model: String) -> Result<Arc<WorkerAgent>> {
    if !args.tools {
        return Ok(WorkerAgent::new(
            AgentId::new("worker"),
            model,
            args.worker_domain.clone(),
        ));
    }

    let root = match &args.tools_root {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("resolve cwd for --tools jail root")?,
    };

    let read_file = ReadFileTool::with_root(&root).ok_or_else(|| {
        anyhow::anyhow!(
            "tools root `{}` does not exist or cannot be canonicalized",
            root.display(),
        )
    })?;

    let mut registry = ToolRegistry::new();
    registry
        .register_owned(read_file)
        .context("register fs.read_file in tool registry")?;

    eprintln!(
        "── tools enabled: {} (jailed to {})",
        registry.names().join(", "),
        root.display(),
    );

    Ok(WorkerAgent::with_tools(
        AgentId::new("worker"),
        model,
        args.worker_domain.clone(),
        Arc::new(registry),
    ))
}

async fn build_runtime(args: &RunTaskArgs) -> Result<Arc<dyn Runtime>> {
    if args.dispatch.no_trajectory {
        let store: Arc<dyn EventStore> = SqliteEventStore::in_memory()
            .context("opening in-memory event store for --no-trajectory mode")?;
        let rt = LocalRuntime::new(store);
        return Ok(rt as Arc<dyn Runtime>);
    }
    let store: Arc<dyn EventStore> = match event_store::open(args.dispatch.events_path.as_deref())? {
        Some(s) => s,
        None => {
            // Fall back to in-memory rather than refusing to run — same
            // posture as `tars run` / `tars plan` use, but routed through
            // a real runtime since run_task requires one.
            tracing::warn!(
                "trajectory: no XDG data dir on this platform; using in-memory event store",
            );
            SqliteEventStore::in_memory()
                .context("opening in-memory event store fallback")?
        }
    };
    let rt = LocalRuntime::new(store);
    Ok(rt as Arc<dyn Runtime>)
}

fn print_outcome_human(o: &TaskOutcome) {
    println!("plan: {} ({} step(s))", o.plan.plan_id, o.plan.steps.len());
    println!("goal: {}", o.plan.goal);
    println!();
    for (i, step) in o.steps.iter().enumerate() {
        let plan_step = o.plan.steps.iter().find(|s| s.id == step.step_id);
        let role = plan_step.map(|s| s.worker_role.as_str()).unwrap_or("?");
        println!(
            "[{}/{}] step `{}` (worker_role={role}, attempts={}+1)",
            i + 1,
            o.steps.len(),
            step.step_id,
            step.refinement_attempts,
        );
        if let AgentMessage::PartialResult { summary, confidence, .. } = &step.result {
            println!("    summary    : {summary}");
            println!("    confidence : {confidence:.2}");
        }
        if let AgentMessage::Verdict { verdict, .. } = &step.verdict {
            let kind = match verdict {
                VerdictKind::Approve => "approve",
                VerdictKind::Reject { .. } => "reject",
                VerdictKind::Refine { .. } => "refine",
            };
            println!("    verdict    : {kind}");
        }
        println!();
    }
}

fn print_outcome_json(o: &TaskOutcome) -> Result<()> {
    // TaskOutcome / StepOutcome aren't `Serialize` themselves (they
    // hold owned AgentMessage fields that are). Project to a small
    // local shape so the JSON is stable / consumer-friendly.
    let json = serde_json::json!({
        "trajectory_id": o.trajectory_id.as_ref(),
        "plan": &o.plan,
        "steps": o.steps.iter().map(|s| serde_json::json!({
            "step_id": s.step_id,
            "refinement_attempts": s.refinement_attempts,
            "result": &s.result,
            "verdict": &s.verdict,
        })).collect::<Vec<_>>(),
    });
    let s = serde_json::to_string_pretty(&json).context("encode TaskOutcome as JSON")?;
    println!("{s}");
    Ok(())
}

/// Map [`RunTaskError`] to an `anyhow::Error`, preserving the chain
/// so `eprintln!("{:?}", err)` from main.rs renders the cause.
/// Trajectory id is already printed separately by the caller.
fn into_anyhow(e: RunTaskError) -> anyhow::Error {
    anyhow::Error::new(e).context("run_task failed")
}
