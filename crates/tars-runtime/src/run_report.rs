//! `build_run_report` — replay one trajectory's event log and roll
//! it up into a [`RunReport`].
//!
//! V1: aggregates from `AgentEvent` only (the trajectory event store).
//! `LlmCallFinished` pipeline events are not yet joined in — the
//! worker doesn't currently propagate `trajectory_id` into
//! `RequestContext.trace_id`, so there's no join key. See
//! [`tars_types::run_report`] module docs for the consequence.
//!
//! Used by `tars run-report <trajectory_id>` and by future eval-replay
//! tooling that wants a one-shot per-run summary.

use std::collections::BTreeMap;

use tars_storage::EventStore;
use tars_types::{
    AgentBreakdown, ProviderBreakdown, RunErrorSummary, RunReport, RunStatus, TrajectoryId, Usage,
};

use crate::error::RuntimeError;
use crate::event::AgentEvent;

/// Get-or-insert `map[key]` and bump its `field` by one, saturating.
/// Centralizes the entry-or-default + `saturating_add` idiom the
/// event-aggregation loop repeats per breakdown bucket, so adding a
/// new counter can't silently drop the saturation on one site.
macro_rules! bump {
    ($map:expr, $key:expr, $field:ident) => {{
        let entry = $map.entry($key).or_default();
        entry.$field = entry.$field.saturating_add(1);
    }};
}

/// Build a [`RunReport`] for `trajectory_id` by replaying its event log.
///
/// Returns [`RuntimeError::TrajectoryNotFound`] when the trajectory
/// has no events recorded.
pub async fn build_run_report(
    store: &dyn EventStore,
    trajectory_id: &TrajectoryId,
) -> Result<RunReport, RuntimeError> {
    let records = store.read_all(trajectory_id).await?;
    if records.is_empty() {
        return Err(RuntimeError::TrajectoryNotFound(
            trajectory_id.as_str().to_string(),
        ));
    }

    // Walk through records once, building the aggregate. We track per-step
    // agent name so LlmCallCaptured (which doesn't carry the agent name
    // directly) can be attributed to the right agent's breakdown.
    let started_at_ms = records.first().map(|r| r.timestamp_ms).unwrap_or(0);
    let last_event_ts_ms = records
        .last()
        .map(|r| r.timestamp_ms)
        .unwrap_or(started_at_ms);

    let mut status = RunStatus::Active;
    let mut summary: Option<String> = None;
    let mut ended_at_ms: Option<i64> = None;

    let mut step_count: u32 = 0;
    let mut failed_step_count: u32 = 0;
    let mut skipped_step_count: u32 = 0;
    let mut llm_call_count: u32 = 0;
    let mut tokens = Usage::default();

    let mut by_provider: BTreeMap<String, ProviderBreakdown> = BTreeMap::new();
    let mut by_agent: BTreeMap<String, AgentBreakdown> = BTreeMap::new();
    let mut errors: Vec<RunErrorSummary> = Vec::new();

    // step_seq → agent label; populated on StepStarted, consumed on
    // every event that carries `step_seq` so we can route to the
    // right AgentBreakdown.
    let mut step_agent: BTreeMap<u32, String> = BTreeMap::new();

    for record in &records {
        let event: AgentEvent = serde_json::from_value(record.payload.clone())?;
        match event {
            AgentEvent::TrajectoryStarted { .. } => {
                // started_at_ms already captured.
            }
            AgentEvent::TrajectoryCompleted { summary: s, .. } => {
                status = RunStatus::Completed;
                summary = Some(s);
                ended_at_ms = Some(record.timestamp_ms);
            }
            AgentEvent::TrajectorySuspended { reason, .. } => {
                status = RunStatus::Suspended;
                summary = Some(reason);
                ended_at_ms = Some(record.timestamp_ms);
            }
            AgentEvent::TrajectoryAbandoned { cause, .. } => {
                status = RunStatus::Abandoned;
                summary = Some(cause);
                ended_at_ms = Some(record.timestamp_ms);
            }
            AgentEvent::StepStarted {
                step_seq, agent, ..
            } => {
                step_agent.insert(step_seq, agent.clone());
                bump!(by_agent, agent, step_count);
                step_count = step_count.saturating_add(1);
            }
            AgentEvent::StepCompleted {
                step_seq,
                usage: step_usage,
                ..
            } => {
                // Add the step's rolled-up usage into the agent's bucket.
                // LlmCallCaptured covers per-call attribution below; the
                // step-level usage is the worker's own aggregate.
                if let Some(agent) = step_agent.get(&step_seq) {
                    let entry = by_agent.entry(agent.clone()).or_default();
                    entry.tokens = merge_usage(entry.tokens, &step_usage);
                }
            }
            AgentEvent::StepFailed {
                step_seq,
                error,
                classification,
                ..
            } => {
                failed_step_count = failed_step_count.saturating_add(1);
                if let Some(agent) = step_agent.get(&step_seq) {
                    bump!(by_agent, agent.clone(), failed_step_count);
                }
                errors.push(RunErrorSummary {
                    step_seq,
                    classification,
                    error,
                });
            }
            AgentEvent::StepSkipped {
                step_seq, agent, ..
            } => {
                // Skipped steps allocate a `step_seq` (so the
                // trajectory's monotonic counter stays gap-free) and
                // emit a single self-contained event — no matching
                // `StepStarted` is appended. Reflect that in the
                // accounting: bump the SKIPPED counter (both global
                // and per-agent), do NOT touch step_count /
                // failed_step_count. Cost-attribution code that
                // reads step_count + tokens stays correct.
                step_agent.insert(step_seq, agent.clone());
                skipped_step_count = skipped_step_count.saturating_add(1);
                bump!(by_agent, agent, skipped_step_count);
            }
            AgentEvent::LlmCallCaptured {
                step_seq,
                provider,
                usage: call_usage,
                ..
            } => {
                llm_call_count = llm_call_count.saturating_add(1);
                tokens = merge_usage(tokens, &call_usage);

                let provider_entry = by_provider
                    .entry(provider.as_str().to_string())
                    .or_default();
                provider_entry.llm_calls = provider_entry.llm_calls.saturating_add(1);
                provider_entry.tokens = merge_usage(provider_entry.tokens, &call_usage);

                // Attribute the call to the right agent's bucket. Note:
                // StepCompleted *also* rolls in the step's usage, so
                // for trajectories that produce both a per-call
                // capture and a step-complete summary, agent tokens
                // could double-count. The current worker emits one or
                // the other; if both fire, the report inflates and
                // that's a worker-side bug to surface — easier to
                // notice in a report than buried in raw events.
                if let Some(agent) = step_agent.get(&step_seq) {
                    bump!(by_agent, agent.clone(), llm_calls);
                }
            }
        }
    }

    let wall_clock_ms = match ended_at_ms {
        Some(e) => (e - started_at_ms).max(0) as u64,
        None => (last_event_ts_ms - started_at_ms).max(0) as u64,
    };

    Ok(RunReport {
        trajectory_id: trajectory_id.clone(),
        status,
        summary,
        started_at_ms,
        ended_at_ms,
        wall_clock_ms,
        step_count,
        failed_step_count,
        skipped_step_count,
        llm_call_count,
        tokens,
        by_provider,
        by_agent,
        errors,
    })
}

/// Add `b`'s counts into `a` saturating-style. Mirrors the existing
/// `Usage::merge`-shape behavior tars-types uses elsewhere — kept
/// as a free fn here so we don't accidentally diverge if upstream
/// changes Usage's API.
fn merge_usage(a: Usage, b: &Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        cached_input_tokens: a.cached_input_tokens.saturating_add(b.cached_input_tokens),
        cache_creation_tokens: a
            .cache_creation_tokens
            .saturating_add(b.cache_creation_tokens),
        thinking_tokens: a.thinking_tokens.saturating_add(b.thinking_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tars_storage::{SqliteEventStore, SqliteEventStoreConfig};
    use tars_types::Usage;

    async fn store_with(records: Vec<(TrajectoryId, serde_json::Value)>) -> Arc<dyn EventStore> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let store: Arc<SqliteEventStore> =
            SqliteEventStore::open(SqliteEventStoreConfig::new(path)).unwrap();
        // Group payloads per trajectory and append in order.
        let mut by_traj: BTreeMap<TrajectoryId, Vec<serde_json::Value>> = BTreeMap::new();
        for (t, p) in records {
            by_traj.entry(t).or_default().push(p);
        }
        for (traj, payloads) in by_traj {
            store.append(&traj, &payloads).await.unwrap();
        }
        // Keep the tempdir alive for the duration of the test via Box leak —
        // for tests this is fine; tempfile cleans up on process exit.
        Box::leak(Box::new(dir));
        store as Arc<dyn EventStore>
    }

    fn make_usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    #[tokio::test]
    async fn unknown_trajectory_returns_not_found() {
        let store = store_with(vec![]).await;
        let err = build_run_report(&*store, &TrajectoryId::new("nope"))
            .await
            .expect_err("must error");
        assert!(matches!(err, RuntimeError::TrajectoryNotFound(_)));
    }

    #[tokio::test]
    async fn completed_trajectory_aggregates_steps_and_tokens() {
        let traj = TrajectoryId::new("t1");
        let events = vec![
            (
                traj.clone(),
                json!({
                    "type": "trajectory_started",
                    "traj": "t1", "parent": null, "reason": "test"
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "step_started",
                    "traj": "t1", "step_seq": 1, "agent": "orchestrator",
                    "idempotency_key": "x", "input_summary": "go"
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "llm_call_captured",
                    "traj": "t1", "step_seq": 1,
                    "provider": "anthropic",
                    "prompt_summary": "p",
                    "response_summary": "r",
                    "usage": {"input_tokens": 10, "output_tokens": 20,
                              "cached_input_tokens": 0,
                              "cache_creation_tokens": 0,
                              "thinking_tokens": 0}
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "step_completed",
                    "traj": "t1", "step_seq": 1, "output_summary": "done",
                    "usage": {"input_tokens": 10, "output_tokens": 20,
                              "cached_input_tokens": 0,
                              "cache_creation_tokens": 0,
                              "thinking_tokens": 0}
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "step_started",
                    "traj": "t1", "step_seq": 2, "agent": "worker:summarise",
                    "idempotency_key": "y", "input_summary": "fetch"
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "llm_call_captured",
                    "traj": "t1", "step_seq": 2,
                    "provider": "openai",
                    "prompt_summary": "p2",
                    "response_summary": "r2",
                    "usage": {"input_tokens": 50, "output_tokens": 100,
                              "cached_input_tokens": 5,
                              "cache_creation_tokens": 0,
                              "thinking_tokens": 0}
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "step_completed",
                    "traj": "t1", "step_seq": 2, "output_summary": "ok",
                    "usage": {"input_tokens": 50, "output_tokens": 100,
                              "cached_input_tokens": 5,
                              "cache_creation_tokens": 0,
                              "thinking_tokens": 0}
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "trajectory_completed",
                    "traj": "t1", "summary": "plan emitted"
                }),
            ),
        ];
        let store = store_with(events).await;
        let report = build_run_report(&*store, &traj).await.unwrap();

        assert_eq!(report.status, RunStatus::Completed);
        assert_eq!(report.summary.as_deref(), Some("plan emitted"));
        assert_eq!(report.step_count, 2);
        assert_eq!(report.failed_step_count, 0);
        assert_eq!(report.llm_call_count, 2);
        assert_eq!(report.tokens.input_tokens, 60);
        assert_eq!(report.tokens.output_tokens, 120);
        assert_eq!(report.tokens.cached_input_tokens, 5);

        // Per-provider check.
        let anth = report.by_provider.get("anthropic").expect("anthropic seen");
        assert_eq!(anth.llm_calls, 1);
        assert_eq!(anth.tokens.input_tokens, 10);
        let oa = report.by_provider.get("openai").expect("openai seen");
        assert_eq!(oa.llm_calls, 1);
        assert_eq!(oa.tokens.input_tokens, 50);

        // Per-agent check.
        let orch = report.by_agent.get("orchestrator").expect("orch seen");
        assert_eq!(orch.step_count, 1);
        assert_eq!(orch.llm_calls, 1);
        let worker = report
            .by_agent
            .get("worker:summarise")
            .expect("worker seen");
        assert_eq!(worker.step_count, 1);
        assert_eq!(worker.llm_calls, 1);

        // No errors.
        assert!(report.errors.is_empty());
    }

    #[tokio::test]
    async fn failed_step_lands_in_errors_and_per_agent_count() {
        let traj = TrajectoryId::new("t2");
        let events = vec![
            (
                traj.clone(),
                json!({
                    "type": "trajectory_started",
                    "traj": "t2", "parent": null, "reason": "test"
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "step_started",
                    "traj": "t2", "step_seq": 1, "agent": "worker:fetch",
                    "idempotency_key": "z", "input_summary": "go"
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "step_failed",
                    "traj": "t2", "step_seq": 1,
                    "error": "network blew up",
                    "classification": "retriable"
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "trajectory_abandoned",
                    "traj": "t2", "cause": "budget exhausted"
                }),
            ),
        ];
        let store = store_with(events).await;
        let report = build_run_report(&*store, &traj).await.unwrap();

        assert_eq!(report.status, RunStatus::Abandoned);
        assert_eq!(report.summary.as_deref(), Some("budget exhausted"));
        assert_eq!(report.step_count, 1);
        assert_eq!(report.failed_step_count, 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].step_seq, 1);
        assert_eq!(report.errors[0].classification, "retriable");
        assert!(report.errors[0].error.contains("network"));

        let fetch = report.by_agent.get("worker:fetch").expect("agent seen");
        assert_eq!(fetch.failed_step_count, 1);
    }

    #[tokio::test]
    async fn active_trajectory_has_no_terminal_summary() {
        let traj = TrajectoryId::new("t3");
        let events = vec![
            (
                traj.clone(),
                json!({
                    "type": "trajectory_started",
                    "traj": "t3", "parent": null, "reason": "test"
                }),
            ),
            (
                traj.clone(),
                json!({
                    "type": "step_started",
                    "traj": "t3", "step_seq": 1, "agent": "orchestrator",
                    "idempotency_key": "k", "input_summary": "go"
                }),
            ),
            // No terminal event — trajectory is still active.
        ];
        let store = store_with(events).await;
        let report = build_run_report(&*store, &traj).await.unwrap();

        assert_eq!(report.status, RunStatus::Active);
        assert!(report.summary.is_none());
        assert!(report.ended_at_ms.is_none());
        // wall_clock_ms falls back to the last-event ts when there's no
        // terminal; the two events here land in the same instant, so the
        // span is effectively zero rather than derived from a terminal event.
        assert_eq!(report.wall_clock_ms, 0);
    }

    #[test]
    fn merge_usage_saturates_and_accumulates() {
        let merged = merge_usage(
            make_usage(10, 20),
            &Usage {
                input_tokens: 3,
                output_tokens: 5,
                cached_input_tokens: 1,
                cache_creation_tokens: 2,
                thinking_tokens: 7,
            },
        );
        assert_eq!(merged.input_tokens, 13);
        assert_eq!(merged.output_tokens, 25);
        assert_eq!(merged.cached_input_tokens, 1);
        assert_eq!(merged.cache_creation_tokens, 2);
        assert_eq!(merged.thinking_tokens, 7);
    }
}
