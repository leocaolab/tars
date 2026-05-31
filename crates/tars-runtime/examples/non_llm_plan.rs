//! Demonstrates running a caller-supplied [`Plan`] against pure-Rust
//! workers — no LLM service, no Orchestrator, no Critic. This is the
//! shape `arc auto` is migrating to in v0.6: a fixed
//! `scan → fix → merge_sweep → test → coverage` chain where every
//! step is a deterministic subprocess / pure Rust function, and the
//! tars DAG primitives (fan-out, conditional, skip-cascade,
//! trajectory event log) compose on top.
//!
//! Run with: `cargo run --example non_llm_plan -p tars-runtime`
//!
//! What this proves at the API surface:
//!
//! 1. [`crate::run_plan`] takes a caller-built [`Plan`] — no LLM
//!    planning round.
//! 2. [`Worker`] impls can be plain async fns. No LLM, no
//!    [`tars_pipeline::LlmService`] required.
//! 3. Workers return [`AgentMessage::PartialResult`] (already
//!    LLM-agnostic — no `model_id` / `prompt_hash`) and
//!    [`Usage::default()`] for honest "zero LLM cost" accounting in
//!    the trajectory log.
//! 4. `critic = None` is accepted — no extra LLM call.
//! 5. Fan-out + skip-cascade work: a 3-level DAG with two leaves and
//!    a merge step runs both leaves in parallel.
//!
//! Output is intentionally chatty — each step prints what it
//! received, what it produced, and the per-step `step_id` so you
//! can compare against `plan.steps[i].id`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use tars_runtime::{
    AgentMessage, LocalRuntime, Plan, PlanStep, RunPlanConfig, Runtime, StepCondition, StepOutcome,
    Worker, WorkerContext, WorkerOutput, WorkerRegistry, emit_step_lifecycle, run_plan,
};
use tars_storage::SqliteEventStore;
use tars_types::{AgentId, Usage};

/// Worker that "summarises" a fake input deterministically. Stands
/// in for arc's `ClaudeFixerWorker` but without the LLM call.
struct EchoSummariser {
    label: &'static str,
}

#[async_trait]
impl Worker for EchoSummariser {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, tars_runtime::WorkerError> {
        let agent_label = format!("worker:{}", step.worker_role);
        let input_summary = format!("echo `{}` for step `{}`", self.label, step.id);
        let summary = self.label;
        let step_id = step.id.clone();
        let label = self.label;

        let ((), usage) = emit_step_lifecycle(
            &ctx.runtime,
            &ctx.trajectory_id,
            &agent_label,
            input_summary,
            |_step_seq| async move {
                println!("  [{step_id}] echo-summariser running, label={label:?}");
                // Real implementation would call out to a subprocess
                // here (arc fix worker, cargo test, merge-sweep, …).
                // We just return immediately.
                Ok::<((), Usage), tars_runtime::WorkerError>(((), Usage::default()))
            },
        )
        .await?;

        Ok(WorkerOutput {
            message: AgentMessage::PartialResult {
                from_agent: AgentId::new(format!("echo-{}", step.worker_role)),
                step_id: Some(step.id.clone()),
                summary: summary.to_string(),
                confidence: 1.0,
            },
            usage,
        })
    }
}

/// Worker that "merges" its deps' outputs into a combined summary.
/// Stands in for arc's serial `MergeSweepWorker` — its job is to
/// reduce the per-finding fix outputs into one consolidated commit
/// log. Reads prior_results to prove the dependency-result threading
/// works for non-LLM workers identically to LLM ones.
struct MergeWorker;

#[async_trait]
impl Worker for MergeWorker {
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        prior: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, tars_runtime::WorkerError> {
        let agent_label = format!("worker:{}", step.worker_role);
        let input_summary = format!("merge {} prior result(s) for step `{}`", prior.len(), step.id);
        let summaries: Vec<String> = step
            .depends_on
            .iter()
            .filter_map(|dep_id| {
                prior.get(dep_id).and_then(|m| match m {
                    AgentMessage::PartialResult { summary, .. } => Some(summary.clone()),
                    _ => None,
                })
            })
            .collect();
        let step_id = step.id.clone();
        let summaries_clone = summaries.clone();

        let ((), usage) = emit_step_lifecycle(
            &ctx.runtime,
            &ctx.trajectory_id,
            &agent_label,
            input_summary,
            |_step_seq| async move {
                println!(
                    "  [{step_id}] merging deps: {:?}",
                    summaries_clone,
                );
                Ok::<((), Usage), tars_runtime::WorkerError>(((), Usage::default()))
            },
        )
        .await?;

        Ok(WorkerOutput {
            message: AgentMessage::PartialResult {
                from_agent: AgentId::new("merge-worker"),
                step_id: Some(step.id.clone()),
                summary: format!("merged: [{}]", summaries.join(", ")),
                confidence: 1.0,
            },
            usage,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = SqliteEventStore::in_memory()?;
    let runtime: Arc<dyn Runtime> = LocalRuntime::new(store);

    let mut registry = WorkerRegistry::new();
    registry.register(
        "scan",
        Arc::new(EchoSummariser {
            label: "scan complete: 2 findings",
        }),
    );
    registry.register(
        "fix",
        Arc::new(EchoSummariser {
            label: "fix applied",
        }),
    );
    registry.register("merge", Arc::new(MergeWorker));

    // 3-level DAG: two parallel "fix" siblings (level 1) gated on a
    // "scan" root (level 0), then a "merge" step (level 2) that
    // consumes both fixes.
    let plan = Plan {
        plan_id: "non-llm-demo".into(),
        goal: "echo demo".into(),
        steps: vec![
            PlanStep {
                id: "scan".into(),
                worker_role: "scan".into(),
                instruction: "scan the repo".into(),
                depends_on: vec![],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "fix-a".into(),
                worker_role: "fix".into(),
                instruction: "fix finding A".into(),
                depends_on: vec!["scan".into()],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "fix-b".into(),
                worker_role: "fix".into(),
                instruction: "fix finding B".into(),
                depends_on: vec!["scan".into()],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "merge".into(),
                worker_role: "merge".into(),
                instruction: "merge the two fixes".into(),
                depends_on: vec!["fix-a".into(), "fix-b".into()],
                condition: StepCondition::Always,
            },
        ],
    };

    let traj = runtime
        .create_trajectory(None, "non-llm DAG demo")
        .await?;

    let outcome = run_plan(
        runtime.clone(),
        traj.clone(),
        plan,
        registry,
        None, // ← no critic. Workers' Approve is implicit.
        RunPlanConfig::default(),
        CancellationToken::new(),
    )
    .await?;

    println!("\n══ outcome ══════════════════════════════════════════════");
    for s in &outcome.steps {
        match s {
            StepOutcome::Completed { step_id, result, .. } => {
                if let AgentMessage::PartialResult { summary, .. } = result {
                    println!("  {step_id:<8} completed — {summary}");
                }
            }
            StepOutcome::Skipped { step_id, reason } => {
                println!("  {step_id:<8} SKIPPED  — {reason}");
            }
        }
    }

    println!("\n══ trajectory event log ═════════════════════════════════");
    let events = runtime.replay(&traj).await?;
    println!("  {} events:", events.len());
    for (i, ev) in events.iter().enumerate() {
        // One-line shape per event, just to show the executor emits
        // the same StepStarted/Completed/LlmCallCaptured pattern an
        // LLM-flavored run does — except LlmCallCaptured never
        // appears here, because no worker called an LLM.
        let kind = match ev {
            tars_runtime::AgentEvent::TrajectoryStarted { .. } => "TrajectoryStarted",
            tars_runtime::AgentEvent::TrajectoryCompleted { .. } => "TrajectoryCompleted",
            tars_runtime::AgentEvent::TrajectorySuspended { .. } => "TrajectorySuspended",
            tars_runtime::AgentEvent::TrajectoryAbandoned { .. } => "TrajectoryAbandoned",
            tars_runtime::AgentEvent::StepStarted { agent, step_seq, .. } => {
                println!("  [{i:>2}] StepStarted    seq={step_seq} agent={agent}");
                continue;
            }
            tars_runtime::AgentEvent::StepCompleted { step_seq, usage, .. } => {
                println!(
                    "  [{i:>2}] StepCompleted  seq={step_seq} usage={}/{}",
                    usage.input_tokens, usage.output_tokens,
                );
                continue;
            }
            tars_runtime::AgentEvent::StepFailed { .. } => "StepFailed",
            tars_runtime::AgentEvent::StepSkipped { .. } => "StepSkipped",
            tars_runtime::AgentEvent::LlmCallCaptured { .. } => "LlmCallCaptured (UNEXPECTED!)",
        };
        println!("  [{i:>2}] {kind}");
    }

    // Honest accounting: no LLM calls were made. Count LlmCallCaptured
    // events directly from the trajectory log (RunReport would need
    // the EventStore handle, which we don't surface from Runtime —
    // the event count from `replay` is just as honest).
    let llm_calls = events
        .iter()
        .filter(|e| matches!(e, tars_runtime::AgentEvent::LlmCallCaptured { .. }))
        .count();
    let total_tokens: u64 = events
        .iter()
        .filter_map(|e| match e {
            tars_runtime::AgentEvent::StepCompleted { usage, .. } => Some(usage),
            _ => None,
        })
        .map(|u| u.input_tokens + u.output_tokens)
        .sum();
    println!("\n══ Cost accounting ══════════════════════════════════════");
    println!("  LlmCallCaptured events = {llm_calls}");
    println!("  total tokens (sum of StepCompleted.usage) = {total_tokens}");
    assert_eq!(llm_calls, 0, "no LLM calls should have happened");
    assert_eq!(total_tokens, 0, "no tokens billed");
    println!("\n  ✓ non-LLM plan completed with 0 LLM calls + 0 tokens");

    Ok(())
}
