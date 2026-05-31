//! `tars run-report <trajectory_id> [--json]` — per-run summary.
//!
//! Reads the trajectory event log + rolls it up into a [`RunReport`].
//! Pretty output by default (human-readable table); `--json` emits the
//! full report as a single JSON object on stdout for jq / scripting.
//!
//! See `docs/eval-and-arc-llm-roadmap.md §1.1` for the design intent.
//! V1 aggregates from the trajectory store only; LlmCallFinished
//! pipeline events are not yet joined in (worker doesn't propagate
//! `trajectory_id` into `RequestContext.trace_id`).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use tars_runtime::{RuntimeError, build_run_report};
use tars_storage::EventStore;
use tars_types::{RunReport, RunStatus, TrajectoryId};

use crate::event_store as event_store_path;

#[derive(Args, Debug)]
pub struct RunReportArgs {
    /// Trajectory id (full UUID-simple form, as printed by `tars run`'s
    /// "── trajectory:" footer or by `tars trajectory list`).
    pub id: String,

    /// Emit the full RunReport as a single JSON object on stdout.
    /// Default output is a human-readable table.
    #[arg(long)]
    pub json: bool,

    /// Override the event store path. Default:
    /// `$XDG_DATA_HOME/tars/events.sqlite`.
    #[arg(long, env = "TARS_EVENTS_PATH")]
    pub events_path: Option<PathBuf>,
}

pub async fn execute(args: RunReportArgs) -> Result<()> {
    let store_arc = event_store_path::open(args.events_path.as_deref())?.context(
        "no event store available — pass --events-path or run on a platform with an XDG data dir",
    )?;
    let store: Arc<dyn EventStore> = store_arc;
    let traj_id = TrajectoryId::new(args.id);

    let report = match build_run_report(&*store, &traj_id).await {
        Ok(r) => r,
        Err(RuntimeError::TrajectoryNotFound(id)) => {
            anyhow::bail!(
                "trajectory not found: {id}\n\
                 hint: `tars trajectory list` shows known ids"
            );
        }
        Err(e) => return Err(e.into()),
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if args.json {
        let s = serde_json::to_string(&report).context("serialize RunReport")?;
        writeln!(out, "{s}").context("stdout write")?;
    } else {
        render_human(&report, &mut out).context("stdout write")?;
    }
    // Explicit flush so a write error (e.g. broken pipe to `head`)
    // surfaces as a non-zero exit instead of being silently dropped
    // when the locked stdout handle is dropped.
    out.flush().context("flush stdout")?;
    Ok(())
}

fn render_human(r: &RunReport, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "trajectory:  {}", r.trajectory_id)?;
    writeln!(out, "status:      {}", status_label(r.status))?;
    if let Some(s) = &r.summary {
        writeln!(out, "summary:     {s}")?;
    }
    writeln!(
        out,
        "wall clock:  {:.3} s   ({} ms)",
        r.wall_clock_ms as f64 / 1000.0,
        r.wall_clock_ms
    )?;
    writeln!(out)?;

    writeln!(out, "steps:       {} ({} failed)", r.step_count, r.failed_step_count)?;
    writeln!(out, "llm calls:   {}", r.llm_call_count)?;
    writeln!(
        out,
        "tokens:      in={:<8} out={:<8} cached={:<6} thinking={}",
        r.tokens.input_tokens,
        r.tokens.output_tokens,
        r.tokens.cached_input_tokens,
        r.tokens.thinking_tokens,
    )?;

    if !r.by_provider.is_empty() {
        writeln!(out)?;
        writeln!(out, "by provider:")?;
        writeln!(out, "  {:<24}  {:>6}  {:>10}  {:>10}", "provider", "calls", "in_tok", "out_tok")?;
        writeln!(out, "  {}", "-".repeat(56))?;
        for (provider, b) in &r.by_provider {
            writeln!(
                out,
                "  {:<24}  {:>6}  {:>10}  {:>10}",
                provider, b.llm_calls, b.tokens.input_tokens, b.tokens.output_tokens
            )?;
        }
    }

    if !r.by_agent.is_empty() {
        writeln!(out)?;
        writeln!(out, "by agent:")?;
        writeln!(
            out,
            "  {:<24}  {:>5}  {:>6}  {:>6}  {:>10}  {:>10}",
            "agent", "steps", "failed", "calls", "in_tok", "out_tok"
        )?;
        // Match the column block width: 24+2+5+2+6+2+6+2+10+2+10 = 71
        // (same convention as the by-provider separator's 56).
        writeln!(out, "  {}", "-".repeat(71))?;
        for (agent, b) in &r.by_agent {
            writeln!(
                out,
                "  {:<24}  {:>5}  {:>6}  {:>6}  {:>10}  {:>10}",
                agent,
                b.step_count,
                b.failed_step_count,
                b.llm_calls,
                b.tokens.input_tokens,
                b.tokens.output_tokens,
            )?;
        }
    }

    if !r.errors.is_empty() {
        writeln!(out)?;
        writeln!(out, "errors ({}):", r.errors.len())?;
        for e in &r.errors {
            // Truncate by character, not byte: error strings may carry
            // arbitrary Unicode from external systems, and slicing at a
            // byte index that lands mid-codepoint panics.
            let truncated = match e.error.char_indices().nth(80) {
                Some((byte_idx, _)) => format!("{}…", &e.error[..byte_idx]),
                None => e.error.clone(),
            };
            writeln!(
                out,
                "  step={}  class={}  err={}",
                e.step_seq, e.classification, truncated
            )?;
        }
    }
    Ok(())
}

fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Active => "active",
        RunStatus::Completed => "completed",
        RunStatus::Suspended => "suspended",
        RunStatus::Abandoned => "abandoned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tars_types::{AgentBreakdown, ProviderBreakdown, RunReport, RunStatus, Usage};

    fn report(status: RunStatus) -> RunReport {
        let mut by_provider = BTreeMap::new();
        by_provider.insert(
            "anthropic".into(),
            ProviderBreakdown {
                llm_calls: 2,
                tokens: Usage {
                    input_tokens: 100,
                    output_tokens: 200,
                    ..Usage::default()
                },
            },
        );
        let mut by_agent = BTreeMap::new();
        by_agent.insert(
            "orchestrator".into(),
            AgentBreakdown {
                step_count: 1,
                failed_step_count: 0,
                skipped_step_count: 0,
                llm_calls: 1,
                tokens: Usage::default(),
            },
        );
        RunReport {
            trajectory_id: TrajectoryId::new("t1"),
            status,
            summary: Some("ok".into()),
            started_at_ms: 0,
            ended_at_ms: Some(500),
            wall_clock_ms: 500,
            step_count: 1,
            failed_step_count: 0,
            skipped_step_count: 0,
            llm_call_count: 2,
            tokens: Usage {
                input_tokens: 100,
                output_tokens: 200,
                ..Usage::default()
            },
            by_provider,
            by_agent,
            errors: Vec::new(),
        }
    }

    #[test]
    fn render_human_includes_key_sections() {
        let r = report(RunStatus::Completed);
        let mut buf = Vec::new();
        render_human(&r, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("trajectory:  t1"));
        assert!(s.contains("status:      completed"));
        assert!(s.contains("summary:     ok"));
        assert!(s.contains("wall clock:"));
        assert!(s.contains("by provider"));
        assert!(s.contains("anthropic"));
        assert!(s.contains("by agent"));
        assert!(s.contains("orchestrator"));
    }

    #[test]
    fn status_label_covers_all_variants() {
        for st in [
            RunStatus::Active,
            RunStatus::Completed,
            RunStatus::Suspended,
            RunStatus::Abandoned,
        ] {
            assert!(!status_label(st).is_empty());
        }
    }
}
