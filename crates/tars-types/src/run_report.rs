//! `RunReport` — aggregated view of one trajectory's work.
//!
//! Produced by replaying a trajectory's [`crate::TrajectoryId`] event
//! log and rolling up the [`crate::Usage`] / step / error counts into
//! a single summary. Consumed by `tars run-report` (human / JSON output),
//! by arc's `RunBenchmark` migration, and by future eval-replay tooling.
//!
//! See `docs/eval-and-arc-llm-roadmap.md §1.1` for the design intent —
//! per-run aggregation is what production agent serving actually needs
//! (vs Doc 16 §7.1's per-call deterministic dimension scoring, which
//! arc's year of experience showed to be the wrong shape).
//!
//! # V1 scope and known gap
//!
//! V1 aggregates **only** from `AgentEvent` (the trajectory event log).
//! `LlmCallFinished` pipeline events are not yet joined in: the worker
//! does not currently set `RequestContext.trace_id = trajectory_id`,
//! so there's no natural join key.
//!
//! Consequence: cache hits, retry counts, and validation outcomes —
//! which only live in `LlmCallFinished` — are **absent** from V1
//! RunReports. Token totals + step counts + per-agent breakdown +
//! errors are all present (they come from `AgentEvent::LlmCallCaptured`
//! and friends).
//!
//! Linking via trace_id is a small worker.rs fix; tracked as a follow-up.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::TrajectoryId;
use crate::usage::Usage;

/// Terminal disposition of the run. `Active` means the trajectory has
/// no terminal event yet (still running, or crashed mid-run).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Active,
    Completed,
    Suspended,
    Abandoned,
}

/// One-shot summary of a single trajectory's execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub trajectory_id: TrajectoryId,
    pub status: RunStatus,
    /// Free-form summary string from the terminal event
    /// (TrajectoryCompleted.summary / TrajectorySuspended.reason /
    /// TrajectoryAbandoned.cause). `None` for `Active`.
    pub summary: Option<String>,

    // ── wall clock (from EventRecord.timestamp_ms) ──────────────────
    /// Wall-clock at TrajectoryStarted.
    pub started_at_ms: i64,
    /// Wall-clock at terminal event, if any.
    pub ended_at_ms: Option<i64>,
    /// `ended_at_ms - started_at_ms` when both known; otherwise the
    /// time from `started_at` to the last observed event.
    ///
    /// **Invariant:** producers MUST clamp the (signed) timestamp
    /// difference at zero before casting to `u64` — event timestamps
    /// can arrive out of order (clock skew, crash-mid-run), so a naive
    /// `(ended - started) as u64` would wrap to a huge bogus duration.
    /// The canonical builder in `tars-runtime::run_report` uses
    /// `(ended - started).max(0) as u64`; any other producer must do
    /// the same.
    pub wall_clock_ms: u64,

    // ── step accounting ────────────────────────────────────────────
    pub step_count: u32,
    pub failed_step_count: u32,
    /// Steps that were skipped at runtime — either their
    /// `StepCondition` evaluated false, or a transitively-depended-on
    /// step was itself skipped (cascade). Tracked separately from
    /// `step_count` so consumers can distinguish "this plan had 5
    /// steps, 3 ran and 2 were skipped" from "this plan had 5 steps,
    /// all ran". Skipped steps don't burn LLM budget — they appear
    /// only as [`AgentEvent::StepSkipped`] (no `StepStarted` /
    /// `StepCompleted` pair).
    ///
    /// Defaulted on deserialise so reports persisted before this
    /// field existed continue to read.
    #[serde(default)]
    pub skipped_step_count: u32,

    // ── LLM call accounting (from LlmCallCaptured.usage) ───────────
    pub llm_call_count: u32,
    /// Sum of every captured `Usage`. Matches what consumers report
    /// today as "input_tokens / output_tokens / cached_input_tokens
    /// / thinking_tokens" without per-call breakdown.
    pub tokens: Usage,

    // ── breakdowns ─────────────────────────────────────────────────
    /// Per-provider rollup, keyed by `ProviderId.as_str()`.
    pub by_provider: BTreeMap<String, ProviderBreakdown>,
    /// Per-agent rollup, keyed by `StepStarted.agent` (free-form
    /// today; e.g. `"orchestrator"`, `"worker:code_review"`).
    pub by_agent: BTreeMap<String, AgentBreakdown>,

    // ── errors ─────────────────────────────────────────────────────
    pub errors: Vec<RunErrorSummary>,
}

/// Reason a [`RunReport`] failed its internal-consistency check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunReportError {
    /// `failed_step_count > step_count` at the report level.
    FailedExceedsTotal { failed: u32, total: u32 },
    /// `failed_step_count > step_count` inside a per-agent breakdown.
    AgentFailedExceedsTotal {
        agent: String,
        failed: u32,
        total: u32,
    },
}

impl std::fmt::Display for RunReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FailedExceedsTotal { failed, total } => write!(
                f,
                "failed_step_count ({failed}) exceeds step_count ({total})"
            ),
            Self::AgentFailedExceedsTotal {
                agent,
                failed,
                total,
            } => write!(
                f,
                "agent {agent:?}: failed_step_count ({failed}) exceeds step_count ({total})"
            ),
        }
    }
}

impl std::error::Error for RunReportError {}

impl RunReport {
    /// Reject internally inconsistent reports: `failed_step_count` must
    /// never exceed `step_count`, at the top level or in any per-agent
    /// breakdown. Public fields make a malformed report constructible,
    /// so consumers that care (eval comparison, dashboards) can call
    /// this defensively before trusting the ratios.
    pub fn validate(&self) -> Result<(), RunReportError> {
        if self.failed_step_count > self.step_count {
            return Err(RunReportError::FailedExceedsTotal {
                failed: self.failed_step_count,
                total: self.step_count,
            });
        }
        for (agent, b) in &self.by_agent {
            if b.failed_step_count > b.step_count {
                return Err(RunReportError::AgentFailedExceedsTotal {
                    agent: agent.clone(),
                    failed: b.failed_step_count,
                    total: b.step_count,
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderBreakdown {
    pub llm_calls: u32,
    pub tokens: Usage,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentBreakdown {
    pub step_count: u32,
    pub failed_step_count: u32,
    /// See [`RunReport::skipped_step_count`] — same semantics, scoped
    /// to this agent. Defaulted on deserialise for backward compat.
    #[serde(default)]
    pub skipped_step_count: u32,
    pub llm_calls: u32,
    pub tokens: Usage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunErrorSummary {
    pub step_seq: u32,
    /// `"retriable" / "permanent" / …` from `StepFailed.classification`.
    pub classification: String,
    /// `StepFailed.error` string (sometimes long; consumers may truncate).
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serde_round_trip() {
        for st in [
            RunStatus::Active,
            RunStatus::Completed,
            RunStatus::Suspended,
            RunStatus::Abandoned,
        ] {
            let v = serde_json::to_value(st).unwrap();
            let back: RunStatus = serde_json::from_value(v).unwrap();
            assert_eq!(st, back);
        }
    }

    #[test]
    fn empty_report_serializes() {
        // Sanity: defaultable parts default; top-level Report still
        // serializes when there's no LLM activity.
        let report = RunReport {
            trajectory_id: TrajectoryId::new("traj-empty"),
            status: RunStatus::Active,
            summary: None,
            started_at_ms: 0,
            ended_at_ms: None,
            wall_clock_ms: 0,
            step_count: 0,
            failed_step_count: 0,
            skipped_step_count: 0,
            llm_call_count: 0,
            tokens: Usage::default(),
            by_provider: BTreeMap::new(),
            by_agent: BTreeMap::new(),
            errors: Vec::new(),
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["status"], "active");
        assert_eq!(v["llm_call_count"], 0);
    }
}
