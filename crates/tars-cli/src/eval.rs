//! `tars eval` — corpus replay + judge + diff.
//!
//! See `docs/eval-and-arc-llm-roadmap.md §1.3` for the design intent:
//! turn "is prompt A better than prompt B?" into a reproducible run +
//! a saved artifact two future runs can be diffed against.
//!
//! Subcommands (all shipped):
//!   `tars eval run --corpus <dir>` — replay corpus through a pipeline
//!   `tars eval judge` — run a Judge over an eval-run's outputs
//!   `tars eval diff`  — compare two eval-runs (operational deltas +
//!                       McNemar on shared `trajectory-match` checks)
//!
//! ## Corpus directory layout
//!
//! ```text
//! corpus/
//!   case_001/
//!     input.txt       (required)
//!     system.txt      (optional system prompt for this case)
//!     expected.txt    (optional gold standard — read by `eval judge`)
//!   case_002/
//!     ...
//! ```
//!
//! Any subdirectory missing `input.txt` is skipped with a warning;
//! any file is welcome inside a case (only the three above are read).
//!
//! ## Output layout
//!
//! ```text
//! benchmarks/runs/eval/<timestamp>/   (gitignored scratch; promote a keeper to benchmarks/baselines/eval/)
//!   manifest.json           — EvalRunManifest (case_count, totals)
//!   case_001/
//!     output.txt            — the agent's response text
//!     report.json           — EvalCaseReport (status, usage, wall clock)
//!   case_002/
//!     ...
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

// ─── fs helpers (ARC-L5-COH-20) ──────────────────────────────────────
//
// `arc scan --judge` flagged scattered `std::fs` usage in this file.
// Batch 7 extracted `write_pretty_json` + `ensure_dir`; this commit
// (Task #18) finishes the consolidation by adding three read helpers
// so every production fs call site goes through a wrapper with a
// uniform error-context wording (`writing <path>`, `reading <path>`,
// `creating dir <path>`, `listing <path>`). Result: the eval-loop
// body shows the *eval logic*, not the I/O-error-context boilerplate.

/// `fs::write` of a serialized pretty-JSON body with a uniform error
/// message (`writing <path>`). Used by every eval artifact write.
fn write_pretty_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let body = serde_json::to_string_pretty(value)?;
    fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

/// Plain `fs::write` of bytes/str with the same `writing <path>`
/// context as [`write_pretty_json`].
fn write_text(path: &Path, body: impl AsRef<[u8]>) -> Result<()> {
    fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

/// `fs::create_dir_all` with a uniform error message (`creating dir
/// <path>`). The eval output layout is the only caller — bundling the
/// context here keeps the eval-loop body terser.
fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating dir {}", path.display()))
}

/// `fs::read_to_string` with a uniform `reading <path>` error
/// context. Used by the corpus loader + manifest reader where a
/// missing file IS an error (the eval invocation must fail loud).
fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// `fs::read_to_string` where the file is **optional**. Returns
/// `Ok(None)` if the path doesn't exist (NotFound), bubbles other I/O
/// errors with a `reading <path>` context. Used by the per-case
/// `checks.json` and similar opt-in artifacts.
fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(anyhow::Error::from(e)).with_context(|| format!("reading {}", path.display()))
        }
    }
}

/// `fs::read_dir` with a uniform `listing <path>` error context.
fn list_dir(path: &Path) -> Result<fs::ReadDir> {
    fs::read_dir(path).with_context(|| format!("listing {}", path.display()))
}

use tars_pipeline::{
    ChainOpts, JsonShapeValidator, LlmService, MaxLengthValidator, NotEmptyValidator,
};
use tars_runtime::trajectory_match::{self, MatchMode, ToolStep};
use tars_runtime::{
    Agent, AgentContext, AgentOutput, ArgEquivalenceJudge, CheckRunner, Invariant,
    ValidatorInvariant, WorkerAgent, args_match_judged, ensure_anti_incest,
};
use tars_tools::builtins::{GlobTool, GrepTool, ListDirTool, ReadFileTool};
use tars_tools::{Tool, ToolRegistry};
use tars_types::{
    ChatRequest, ChatResponse, ChatResponseBuilder, ProviderId, RequestContext,
    TrajectoryId, Usage,
};
use tokio_util::sync::CancellationToken;

use crate::config_loader;
use crate::dispatch::{build_registry_with_breaker, pick_provider};

#[derive(Args, Debug)]
pub struct EvalArgs {
    #[command(subcommand)]
    pub command: EvalCommand,
}

#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// Replay a corpus of cases through a pipeline, writing per-case
    /// outputs + a manifest two runs can be diffed against.
    Run(EvalRunArgs),
    /// Behavioral diff of two eval runs (baseline vs candidate).
    /// Compares operational metrics (errors / tokens / latency) and
    /// per-check violation rates — NOT raw output text. See
    /// `docs/architecture/18-agent-testing.md` §2.
    Diff(EvalDiffArgs),
    /// One-shot migration for eval runs written before `ARC-L5-B-6`.
    /// Walks the given run directory and rewrites any `CaseCheckResult`
    /// blocks in `manifest.json` / `<case>/report.json` from the legacy
    /// `{"name", "passed", "detail"}` shape to the new internally-tagged
    /// `{"outcome": "passed"|"failed", "name", "note"|"reason"}` shape.
    /// Idempotent — files already in the new shape are left untouched.
    MigrateChecks(EvalMigrateChecksArgs),
    /// Run an LLM judge over an eval run's outputs (TP/FP/Unsure per
    /// case), writing `judge_report.json` into the run directory. The
    /// judge is a normal tars provider (default `claude_cli`); anti-
    /// incest refuses a judge whose provider matches the run's. See
    /// Doc 18 §7.
    Judge(EvalJudgeArgs),
    /// Bless an eval run's outputs — the approval loop (Doc 28). With
    /// `--select <jsonpath>` it captures the selected fields of each
    /// case's `output.txt` into `<case>/output.bless.json`; without, it
    /// checks each output against its committed bless and reports drift.
    Bless(EvalBlessArgs),
}

#[derive(Args, Debug)]
pub struct EvalBlessArgs {
    /// Eval-run directory (contains manifest.json + per-case output.txt).
    pub run: PathBuf,
    /// JSONPath-subset field to bless, repeatable (e.g. `--select '$.severity'`).
    /// When present → record mode; when absent → check mode.
    #[arg(long = "select")]
    pub select: Vec<String>,
    /// Write the bless directly (accept). Without it, record mode stages a
    /// `*.bless.json.new` for review; the committed file is never clobbered.
    #[arg(long)]
    pub accept: bool,
}

#[derive(Args, Debug)]
pub struct EvalJudgeArgs {
    /// Eval-run directory (contains manifest.json + per-case output.txt).
    pub run: PathBuf,
    /// Provider id to use as the judge. Default `claude_cli`.
    #[arg(long, default_value = "claude_cli")]
    pub judge: String,
    /// Judge model hint. If unset, the provider's default model.
    #[arg(long)]
    pub judge_model: Option<String>,
}

#[derive(Args, Debug)]
pub struct EvalMigrateChecksArgs {
    /// Eval-run directory containing `manifest.json` and per-case
    /// subdirectories with `report.json`.
    #[arg(value_name = "DIR")]
    pub dir: PathBuf,
}

#[derive(Args, Debug)]
pub struct EvalDiffArgs {
    /// Baseline eval-run directory (contains manifest.json).
    pub baseline: PathBuf,
    /// Candidate eval-run directory (contains manifest.json).
    pub candidate: PathBuf,
    /// Emit the diff as a single JSON object on stdout.
    #[arg(long)]
    pub json: bool,
    /// Add a tool-trajectory section: head-to-head divergence of the tools
    /// each run's model selected (paired by case id, from the persisted
    /// `tool_trajectory`), plus McNemar on any `trajectory-match` check both
    /// runs share. No oracle needed for the divergence. (Doc 26 P2.)
    #[arg(long)]
    pub trajectory: bool,
    /// Similarity mode for the head-to-head divergence: exact | ordered | set.
    #[arg(long = "trajectory-mode", default_value = "ordered")]
    pub trajectory_mode: String,
}

#[derive(Args, Debug)]
pub struct EvalRunArgs {
    /// Corpus directory. Each subdirectory is one case; required
    /// `input.txt`, optional `system.txt` and `expected.txt`.
    #[arg(long)]
    pub corpus: PathBuf,

    /// Provider id from your config. Defaults to the first
    /// user-configured provider (same rule as `tars run`).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model hint. If not set, the provider's default is used.
    #[arg(long)]
    pub model: Option<String>,

    /// Output directory. Default: `./benchmarks/runs/eval/<timestamp>/`.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,

    /// Per-case max output token bound.
    #[arg(long)]
    pub max_output_tokens: Option<u32>,

    /// Built-in invariant checks to run against each output (repeatable).
    /// Recognized: `non-empty`, `valid-json`, `max-length:<N>`, and
    /// `trajectory-match[:<exact|ordered|set|args|args-judge>[:<threshold>]]` — scores the
    /// tools the model selected against each case's `expected_tools.json`
    /// (Doc 26). Custom invariants are a Rust-API feature (Doc 18 §4.1).
    #[arg(long = "check")]
    pub checks: Vec<String>,

    /// Run each case through a tool-using agent loop instead of a single
    /// completion, so multi-step tool trajectories are produced (Doc 26 M2'').
    /// SAFETY: only read-only tools are available and they're jailed to the
    /// case dir — never `bash` / write tools.
    #[arg(long)]
    pub agent: bool,

    /// In `--agent` mode, which read-only tools to expose (repeatable).
    /// Allowed: `read_file`, `grep`, `glob`, `list_dir`. Default: all four.
    #[arg(long = "tool")]
    pub tools: Vec<String>,

    /// In `--agent` mode, cap the tool-loop iterations per case.
    #[arg(long)]
    pub agent_max_iterations: Option<u32>,

    /// Judge provider for `trajectory-match:args-judge` (Doc 26 M3' pt2) —
    /// an LLM decides whether byte-different tool arguments are *semantically*
    /// equivalent. Must differ from `--provider` (anti-incest).
    #[arg(long)]
    pub judge_provider: Option<String>,

    /// Judge model hint for `--judge-provider`. Defaults to that provider's
    /// default model.
    #[arg(long)]
    pub judge_model: Option<String>,
}

/// Resolved `--agent` configuration (the read-only tool set + iteration cap).
struct AgentMode {
    tools: Vec<String>,
    max_iterations: Option<u32>,
}

/// Map a `--check` spec to a built-in invariant. Returns an error for
/// unrecognized specs so a typo fails loudly instead of silently
/// running no check.
fn build_invariant(spec: &str) -> Result<Arc<dyn Invariant>> {
    let inv: Arc<dyn Invariant> = match spec {
        "non-empty" => Arc::new(ValidatorInvariant::new(Arc::new(NotEmptyValidator::new()))),
        "valid-json" => Arc::new(ValidatorInvariant::new(Arc::new(JsonShapeValidator::new()))),
        other if other.starts_with("max-length:") => {
            let n: usize = other
                .trim_start_matches("max-length:")
                .parse()
                .with_context(|| format!("bad max-length spec: {other}"))?;
            Arc::new(ValidatorInvariant::new(Arc::new(
                MaxLengthValidator::truncate_above(n),
            )))
        }
        other => anyhow::bail!(
            "unknown --check `{other}`. Recognized: non-empty, valid-json, max-length:<N>, \
             trajectory-match[:<exact|ordered|set|args|args-judge>[:<threshold>]]"
        ),
    };
    Ok(inv)
}

/// A `trajectory-match` check (Doc 26). Unlike an [`Invariant`], it needs
/// **per-case** reference data (`expected_tools`) that the `(req, resp)`
/// signature can't carry, so it's evaluated in `run_eval`'s case loop rather
/// than through the global `CheckRunner`.
struct TrajectorySpec {
    /// The full `--check` spec string — used as the check name so two runs
    /// with the same spec align under `eval diff`.
    raw: String,
    mode: MatchMode,
    /// Per-case pass threshold on the score (default 1.0 = strict).
    threshold: f64,
    /// `args-judge` mode (Doc 26 M3' pt2): like `args`, but byte-different
    /// arguments are LLM-judged for semantic equivalence (needs a judge).
    judge: bool,
}

impl TrajectorySpec {
    /// Parse `trajectory-match`, `trajectory-match:<mode>`, or
    /// `trajectory-match:<mode>:<threshold>`. Returns `Ok(None)` when `spec`
    /// is not a trajectory-match spec (so the caller falls back to
    /// [`build_invariant`]). `<mode>` may be `args-judge` (= `args` + judge).
    fn parse(spec: &str) -> Result<Option<Self>> {
        let rest = match spec.strip_prefix("trajectory-match") {
            Some(r) => r,
            None => return Ok(None),
        };
        // rest is "" | ":<mode>" | ":<mode>:<thresh>"
        let rest = rest.strip_prefix(':').unwrap_or(rest);
        let mut it = rest.splitn(2, ':');
        let mode_tok = it.next().unwrap_or("");
        let mut judge = false;
        let mode = if mode_tok.is_empty() {
            MatchMode::Ordered // bare `trajectory-match` → ordered
        } else if mode_tok == "args-judge" {
            judge = true;
            MatchMode::Args
        } else {
            MatchMode::parse(mode_tok).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown trajectory-match mode `{mode_tok}`. Recognized: exact, ordered, set, args, args-judge, args-judge"
                )
            })?
        };
        let threshold = match it.next() {
            Some(t) => t
                .parse::<f64>()
                .with_context(|| format!("bad trajectory-match threshold: {t}"))?,
            None => 1.0,
        };
        Ok(Some(Self {
            raw: spec.to_string(),
            mode,
            threshold,
            judge,
        }))
    }

    fn name(&self) -> &str {
        &self.raw
    }

    /// Score one case. `None` = the case has no `expected_tools` and is
    /// skipped (excluded from the check's `evaluated` count).
    fn eval_case(
        &self,
        actual: &[ToolStep],
        expected: Option<&[ToolStep]>,
    ) -> Option<CaseCheckResult> {
        let expected = expected?;
        let s = trajectory_match::score(actual, expected, self.mode);
        let got: Vec<&str> = actual.iter().map(|x| x.name.as_str()).collect();
        if s >= self.threshold {
            Some(CaseCheckResult::Passed {
                name: self.raw.clone(),
                note: Some(format!("score={s:.3} ({}) got={got:?}", self.mode.as_str())),
            })
        } else {
            let want: Vec<&str> = expected.iter().map(|x| x.name.as_str()).collect();
            Some(CaseCheckResult::Failed {
                name: self.raw.clone(),
                reason: format!(
                    "score={s:.3} < {:.3} ({}); want={want:?} got={got:?}",
                    self.threshold,
                    self.mode.as_str()
                ),
            })
        }
    }

    /// `args-judge` variant of [`eval_case`] (Doc 26 M3' pt2): scores via the
    /// LLM arg-equivalence judge. `None` = no `expected_tools` (skipped).
    async fn eval_case_judged(
        &self,
        actual: &[ToolStep],
        expected: Option<&[ToolStep]>,
        judge: &ArgEquivalenceJudge,
    ) -> Option<CaseCheckResult> {
        let expected = expected?;
        let got: Vec<&str> = actual.iter().map(|x| x.name.as_str()).collect();
        let s = match args_match_judged(actual, expected, judge).await {
            Ok(s) => s,
            Err(e) => {
                return Some(CaseCheckResult::Failed {
                    name: self.raw.clone(),
                    reason: format!("arg-judge error: {e}"),
                });
            }
        };
        if s >= self.threshold {
            Some(CaseCheckResult::Passed {
                name: self.raw.clone(),
                note: Some(format!("score={s:.3} (args-judge) got={got:?}")),
            })
        } else {
            let want: Vec<&str> = expected.iter().map(|x| x.name.as_str()).collect();
            Some(CaseCheckResult::Failed {
                name: self.raw.clone(),
                reason: format!(
                    "score={s:.3} < {:.3} (args-judge); want={want:?} got={got:?}",
                    self.threshold
                ),
            })
        }
    }
}

/// Manifest written to `<output>/manifest.json`. Stable schema so
/// `tars eval diff` (future) can pattern-match without ad-hoc parsing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalRunManifest {
    /// Path the user gave for `--corpus` (absolute when possible).
    pub corpus_path: String,
    /// Provider id used.
    pub provider_id: String,
    /// Model hint label sent on each request.
    pub model: String,
    /// UTC millis when the first case started.
    pub started_at_ms: u64,
    /// UTC millis when the last case finished.
    pub ended_at_ms: u64,
    pub case_count: u32,
    pub success_count: u32,
    pub error_count: u32,
    /// Sum of `Usage` across successful cases. Failed cases contribute
    /// nothing (no usage reported on a failed call).
    pub total_usage: Usage,
    /// Invariant check rollups, one per `--check`. Empty if no checks
    /// were requested. This is the "behavioral" part of the report —
    /// `tars eval diff` reads these to show violation-rate deltas.
    #[serde(default)]
    pub checks: Vec<CheckSummary>,
    /// Per-case summary, in run order. Each entry mirrors the
    /// `report.json` written inside that case's directory.
    pub cases: Vec<EvalCaseReport>,
}

/// Aggregate of one invariant across all cases in a run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckSummary {
    pub name: String,
    /// Cases the check ran on (= successful cases; we don't check
    /// errored cases since there's no real output).
    pub evaluated: u32,
    pub violations: u32,
    /// `violations / evaluated`, 0.0 when `evaluated == 0`.
    pub violation_rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalCaseReport {
    pub case_id: String,
    pub status: EvalCaseStatus,
    pub wall_clock_ms: u64,
    pub usage: Usage,
    /// Length of the response in chars. The full text lives at
    /// `<case>/output.txt`; keeping the count here avoids re-reading
    /// the file for summaries.
    pub output_chars: u64,
    /// Truncated error message when `status == "error"`. Empty when OK.
    pub error: Option<String>,
    /// Per-invariant results for this case. Empty if no checks ran.
    #[serde(default)]
    pub checks: Vec<CaseCheckResult>,
    /// Tool names the model selected, in call order (from
    /// `ChatResponse.tool_calls`). Persisted so `eval diff --trajectory`
    /// (Doc 26 P2) can compare runs without re-inferring. Empty when the
    /// case made no tool calls. `#[serde(default)]` keeps older reports
    /// readable.
    #[serde(default)]
    pub tool_trajectory: Vec<String>,
}

/// One invariant check's outcome for one eval case.
///
/// Sum-as-product fix for the original `(name, passed: bool, detail:
/// Option<String>)` shape (`arc scan --judge` findings `ARC-L5-B-8` /
/// `ARC-L5-B-6`). The typed enum makes "fail without reason"
/// unrepresentable while keeping the documented "pass with note" path
/// available (validator-skipped notes, etc).
///
/// ### Wire format
///
/// Internally-tagged serde — `outcome: "passed" | "failed"` is the
/// discriminator, the remaining fields are flattened alongside it:
///
/// ```text
///   {"outcome": "passed", "name": "x", "note": null}
///   {"outcome": "failed", "name": "y", "reason": "bad"}
/// ```
///
/// `ARC-L5-B-6` killed the bespoke `CaseCheckResultWire` adapter that
/// preserved the legacy flat `{"name", "passed", "detail"}` shape:
/// the wire DTO + custom `Serialize`/`Deserialize` were permanent
/// tech debt for a back-compat surface that doesn't apply to new
/// runs. Old `report.json` / `manifest.json` artifacts can be
/// migrated in place via [`migrate_legacy_check_results_in_dir`] (CLI:
/// `tars eval migrate-checks <dir>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CaseCheckResult {
    /// Invariant held. `note` is an optional diagnostic (e.g.
    /// "validator skipped because no oracle was supplied").
    Passed {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Invariant failed. `reason` is required — a failure without a
    /// reason was a constructible-but-meaningless legacy state and is
    /// now structurally unrepresentable at every layer.
    Failed { name: String, reason: String },
}

impl CaseCheckResult {
    pub fn name(&self) -> &str {
        match self {
            Self::Passed { name, .. } | Self::Failed { name, .. } => name,
        }
    }

    pub fn passed(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }

    /// Optional diagnostic string, projected from whichever variant
    /// carries one. `Passed.note` and `Failed.reason` flatten through
    /// here for callers that want the legacy `Option<String>` view.
    #[allow(dead_code)] // public accessor for an eval-report consumer that may not exist yet
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Passed { note, .. } => note.as_deref(),
            Self::Failed { reason, .. } => Some(reason.as_str()),
        }
    }
}

// ─── Legacy-shape migration ────────────────────────────────────────
//
// `CaseCheckResultWire` is gone, but report.json / manifest.json
// files written before ARC-L5-B-6 carry the old flat shape:
//
//   {"name": "x", "passed": true, "detail": null}
//   {"name": "y", "passed": false, "detail": "bad"}
//
// These helpers walk a JSON `Value` and rewrite any such block to
// the new internally-tagged form. Idempotent: blocks already in the
// new shape pass through untouched. Failed-without-detail (the
// previously-rejected illegal state) becomes an explicit error so
// the operator sees what they have rather than silently inventing a
// reason.

/// Migrate one check JSON block in place. Returns `Ok(true)` if the
/// block was rewritten, `Ok(false)` if it was already in the new
/// shape or wasn't recognisably a check object at all.
fn migrate_legacy_check(check: &mut serde_json::Value) -> std::io::Result<bool> {
    let Some(obj) = check.as_object() else {
        return Ok(false);
    };
    if obj.contains_key("outcome") {
        // Already new shape.
        return Ok(false);
    }
    let Some(name) = obj.get("name").and_then(|v| v.as_str()).map(String::from) else {
        // Not a check object — leave alone.
        return Ok(false);
    };
    let Some(passed) = obj.get("passed").and_then(|v| v.as_bool()) else {
        return Ok(false);
    };
    let detail = obj.get("detail").and_then(|v| v.as_str()).map(String::from);
    let new = match (passed, detail) {
        (true, note) => {
            let mut m = serde_json::Map::new();
            m.insert("outcome".into(), serde_json::Value::String("passed".into()));
            m.insert("name".into(), serde_json::Value::String(name));
            if let Some(n) = note {
                m.insert("note".into(), serde_json::Value::String(n));
            }
            serde_json::Value::Object(m)
        }
        (false, Some(reason)) => serde_json::json!({
            "outcome": "failed",
            "name": name,
            "reason": reason,
        }),
        (false, None) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "legacy check `{name}` has passed=false with no detail; \
                     the original code rejected this on read, refusing to \
                     invent a reason on migrate"
                ),
            ));
        }
    };
    *check = new;
    Ok(true)
}

/// Walk a manifest/report JSON value and migrate every check block.
/// Recognises both `manifest.json` (top-level `cases[*].checks[*]`)
/// and per-case `report.json` (top-level `checks[*]`).
fn migrate_legacy_checks_in_value(v: &mut serde_json::Value) -> std::io::Result<usize> {
    let mut rewrites = 0;
    if let Some(checks) = v.get_mut("checks").and_then(|c| c.as_array_mut()) {
        for c in checks {
            if migrate_legacy_check(c)? {
                rewrites += 1;
            }
        }
    }
    if let Some(cases) = v.get_mut("cases").and_then(|c| c.as_array_mut()) {
        for case in cases {
            if let Some(checks) = case.get_mut("checks").and_then(|c| c.as_array_mut()) {
                for c in checks {
                    if migrate_legacy_check(c)? {
                        rewrites += 1;
                    }
                }
            }
        }
    }
    Ok(rewrites)
}

/// Walk an eval-run directory and rewrite every legacy check block
/// in `manifest.json` and `<case>/report.json`. Returns the per-file
/// rewrite counts. Files already in the new shape are left untouched.
pub fn migrate_legacy_check_results_in_dir(dir: &Path) -> Result<Vec<(PathBuf, usize)>> {
    let mut touched = Vec::new();
    let manifest = dir.join("manifest.json");
    if manifest.is_file() {
        if let Some(n) = migrate_one_file(&manifest)? {
            touched.push((manifest, n));
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let report = p.join("report.json");
            if report.is_file()
                && let Some(n) = migrate_one_file(&report)?
            {
                touched.push((report, n));
            }
        }
    }
    Ok(touched)
}

/// Read → migrate → write one JSON file. Returns the rewrite count,
/// or `None` if the file was already entirely in the new shape (in
/// which case we leave the bytes alone — no spurious mtime bump).
fn migrate_one_file(path: &Path) -> Result<Option<usize>> {
    let body = read_text(path)?;
    let mut v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    let n = migrate_legacy_checks_in_value(&mut v)?;
    if n == 0 {
        return Ok(None);
    }
    let out = serde_json::to_string_pretty(&v)
        .with_context(|| format!("re-serializing migrated {}", path.display()))?;
    std::fs::write(path, out).with_context(|| format!("writing migrated {}", path.display()))?;
    Ok(Some(n))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalCaseStatus {
    Ok,
    Error,
}

pub async fn execute(args: EvalArgs, config_path: Option<PathBuf>) -> Result<()> {
    match args.command {
        EvalCommand::Run(a) => run_eval(a, config_path).await,
        EvalCommand::Diff(a) => run_diff(a),
        EvalCommand::MigrateChecks(a) => run_migrate_checks(a),
        EvalCommand::Judge(a) => run_judge(a, config_path).await,
        EvalCommand::Bless(a) => run_bless(a),
    }
}

// ─── eval bless (Doc 28) ──────────────────────────────────────────────

/// `tars eval bless <run>` — the approval loop over an eval run's per-case
/// outputs. With `--select` it *records* (captures the selected fields into
/// `<case>/output.bless.json`); without, it *checks* each case's output against
/// its committed bless and reports drift (the Doc 18 §4.4 golden loop).
fn run_bless(args: EvalBlessArgs) -> Result<()> {
    let manifest = load_manifest(&args.run)?;
    let selectors: Vec<&str> = args.select.iter().map(String::as_str).collect();
    let recording = !selectors.is_empty();
    println!(
        "── eval bless: {} cases in {} ({})",
        manifest.cases.len(),
        args.run.display(),
        if recording { "record" } else { "check" }
    );

    let mut drift_cases = 0usize;
    for case in &manifest.cases {
        let case_dir = args.run.join(&case.case_id);
        let output = read_optional_text(&case_dir.join("output.txt"))?.unwrap_or_default();
        // Tolerant decode: an eval output may be chatty prose around JSON.
        let value: serde_json::Value =
            match tars_utils::decode_json(&output, tars_types::StructuredOutputMode::None) {
                Ok(v) => v,
                Err(e) => {
                    println!("  {} … skipped (not JSON: {e})", case.case_id);
                    continue;
                }
            };
        let bless_path = case_dir.join("output.bless.json");

        if recording {
            let bless = tars_types::Bless::capture(&value, &selectors, None)
                .with_context(|| format!("capturing bless for case {}", case.case_id))?;
            if args.accept {
                bless.save(&bless_path).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("  {} … blessed ({} fields)", case.case_id, bless.asserts.len());
            } else {
                let pending = bless.save_pending(&bless_path).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("  {} … staged {} (review + --accept)", case.case_id, pending.display());
            }
        } else if bless_path.exists() {
            let bless = tars_types::Bless::load(&bless_path).map_err(|e| anyhow::anyhow!("{e}"))?;
            let outcome = bless.check(&value).map_err(|e| anyhow::anyhow!("{e}"))?;
            if outcome.is_pass() {
                println!("  {} … ok", case.case_id);
            } else {
                drift_cases += 1;
                for d in &outcome.drifts {
                    println!(
                        "  {} … DRIFT {} : blessed {} → got {}",
                        case.case_id,
                        d.selector,
                        d.expected,
                        d.actual.as_ref().map(|v| v.to_string()).unwrap_or_else(|| "<missing>".into()),
                    );
                }
            }
        } else {
            println!("  {} … no bless (run with --select to create)", case.case_id);
        }
    }

    if !recording && drift_cases > 0 {
        anyhow::bail!("{drift_cases} case(s) drifted from their bless");
    }
    Ok(())
}

// ─── eval judge ───────────────────────────────────────────────────────

async fn run_judge(args: EvalJudgeArgs, config_path: Option<PathBuf>) -> Result<()> {
    use tars_runtime::{JudgeItem, LlmJudge, ensure_anti_incest, run_judge_pass};
    use tars_types::{ProviderId};

    let manifest = load_manifest(&args.run)?;

    // Anti-incest: the judge's provider must differ from the provider
    // that produced the run (Doc 18 §7 / Panickssery 2024).
    ensure_anti_incest(&args.judge, &[manifest.provider_id.as_str()]).map_err(|e| {
        anyhow::anyhow!(
            "{e}\nthe run was produced by `{}`; pick a different --judge provider",
            manifest.provider_id
        )
    })?;

    // Build the judge pipeline from config.
    let cfg = config_loader::load(config_path)?;
    let registry = build_registry_with_breaker(&cfg, true)?;
    let judge_pid = ProviderId::new(&args.judge);
    let judge_provider = registry
        .get(&judge_pid)
        .ok_or_else(|| anyhow::anyhow!("judge provider `{}` not in config", args.judge))?;
    let judge_pipeline = LlmService::default_chain(
        judge_provider,
        args.judge_model.clone().unwrap_or_default(),
        ChainOpts::new(judge_pid.clone()),
    );
    let judge = LlmJudge::new(
        judge_pipeline,
        format!(
            "{}:{}",
            args.judge,
            args.judge_model.as_deref().unwrap_or("default")
        ),
    );

    // Build JudgeItems from each successful case: input (from corpus),
    // output (output.txt), expected (expected.txt if present).
    let mut items: Vec<JudgeItem> = Vec::new();
    for case in &manifest.cases {
        if case.status != EvalCaseStatus::Ok {
            continue;
        }
        let case_dir = args.run.join(&case.case_id);
        let output = read_optional_text(&case_dir.join("output.txt"))?.unwrap_or_default();
        // input/expected: the corpus isn't copied into the run dir, so
        // we read what we can. output.txt always present; expected may
        // be carried alongside if the run recorded it (future). For now
        // input is the case id as a stand-in label; the judge prompt
        // leans on output + expected.
        items.push(JudgeItem {
            item_id: case.case_id.clone(),
            input: case.case_id.clone(),
            output,
            expected: read_optional_text(&case_dir.join("expected.txt"))?,
            context: None,
        });
    }
    if items.is_empty() {
        anyhow::bail!("no successful cases in {} to judge", args.run.display());
    }

    eprintln!(
        "── judging {} cases with {} (anti-incest vs `{}` OK)",
        items.len(),
        args.judge,
        manifest.provider_id,
    );
    let report = run_judge_pass(items, &judge)
        .await
        .map_err(|e| anyhow::anyhow!("judge pass failed: {e}"))?;

    let report_path = args.run.join("judge_report.json");
    write_pretty_json(&report_path, &report)?;

    let prec = report
        .precision()
        .map(|p| format!("{:.1}%", p * 100.0))
        .unwrap_or_else(|| "n/a".into());
    eprintln!(
        "── done. TP={} FP={} Unsure={}  precision={}  → {}",
        report.true_positives,
        report.false_positives,
        report.unsure,
        prec,
        report_path.display(),
    );
    Ok(())
}

// ─── eval diff ────────────────────────────────────────────────────────

fn run_migrate_checks(args: EvalMigrateChecksArgs) -> Result<()> {
    let touched = migrate_legacy_check_results_in_dir(&args.dir)?;
    if touched.is_empty() {
        println!(
            "eval migrate-checks: nothing to do ({} already in the new \
             shape, or no manifest.json / report.json files found)",
            args.dir.display()
        );
        return Ok(());
    }
    let total: usize = touched.iter().map(|(_, n)| n).sum();
    println!(
        "eval migrate-checks: rewrote {total} legacy check block(s) \
         across {} file(s):",
        touched.len()
    );
    for (path, n) in &touched {
        println!("  {n:>4}  {}", path.display());
    }
    Ok(())
}

fn load_manifest(dir: &Path) -> Result<EvalRunManifest> {
    let path = dir.join("manifest.json");
    let body = read_text(&path)?;
    serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))
}

fn p50(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

/// Head-to-head tool-trajectory comparison of two runs (Doc 26 P2).
struct TrajDiff {
    paired: u32,
    a_only: u32,
    b_only: u32,
    divergent: u32,
    mean_similarity: f64,
    diverging_ids: Vec<String>,
    /// McNemar per `trajectory-match*` check both runs ran.
    mcnemar: Vec<(String, tars_runtime::McNemarResult)>,
}

/// case_id → did this run's `check_name` pass? (only cases that ran it).
fn case_check_passmap(
    m: &EvalRunManifest,
    check_name: &str,
) -> std::collections::BTreeMap<String, bool> {
    m.cases
        .iter()
        .filter_map(|c| {
            c.checks
                .iter()
                .find(|cc| cc.name() == check_name)
                .map(|cc| (c.case_id.clone(), cc.passed()))
        })
        .collect()
}

fn compute_traj_diff(a: &EvalRunManifest, b: &EvalRunManifest, mode: MatchMode) -> TrajDiff {
    use std::collections::{BTreeMap, BTreeSet};
    let amap: BTreeMap<&str, &EvalCaseReport> =
        a.cases.iter().map(|c| (c.case_id.as_str(), c)).collect();
    let bmap: BTreeMap<&str, &EvalCaseReport> =
        b.cases.iter().map(|c| (c.case_id.as_str(), c)).collect();

    let (mut paired, mut divergent, mut sum_sim) = (0u32, 0u32, 0.0f64);
    let mut diverging_ids = Vec::new();
    for (id, ca) in &amap {
        let Some(cb) = bmap.get(id) else { continue };
        paired += 1;
        let an: Vec<&str> = ca.tool_trajectory.iter().map(String::as_str).collect();
        let bn: Vec<&str> = cb.tool_trajectory.iter().map(String::as_str).collect();
        let sim = trajectory_match::score_names(&an, &bn, mode);
        sum_sim += sim;
        if sim < 1.0 {
            divergent += 1;
            diverging_ids.push((*id).to_string());
        }
    }
    let a_only = amap.keys().filter(|k| !bmap.contains_key(*k)).count() as u32;
    let b_only = bmap.keys().filter(|k| !amap.contains_key(*k)).count() as u32;
    let mean_similarity = if paired == 0 { 1.0 } else { sum_sim / paired as f64 };

    // McNemar on every trajectory-match check both runs share.
    let a_traj: BTreeSet<&str> = a
        .checks
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| n.starts_with("trajectory-match"))
        .collect();
    let shared: Vec<String> = b
        .checks
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| n.starts_with("trajectory-match") && a_traj.contains(n))
        .map(str::to_string)
        .collect();
    let mcnemar = shared
        .into_iter()
        .map(|name| {
            let r =
                tars_runtime::mcnemar(&case_check_passmap(a, &name), &case_check_passmap(b, &name));
            (name, r)
        })
        .collect();

    TrajDiff {
        paired,
        a_only,
        b_only,
        divergent,
        mean_similarity,
        diverging_ids,
        mcnemar,
    }
}

fn run_diff(args: EvalDiffArgs) -> Result<()> {
    let a = load_manifest(&args.baseline)?;
    let b = load_manifest(&args.candidate)?;

    // Tool-trajectory section is opt-in (--trajectory). Parse the mode early so
    // a bad value fails before any output.
    let traj_diff = if args.trajectory {
        let mode = MatchMode::parse(&args.trajectory_mode).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown --trajectory-mode `{}`. Recognized: exact, ordered, set, args, args-judge",
                args.trajectory_mode
            )
        })?;
        Some((mode, compute_traj_diff(&a, &b, mode)))
    } else {
        None
    };

    // Operational metrics.
    let a_lat = p50(a.cases.iter().map(|c| c.wall_clock_ms).collect());
    let b_lat = p50(b.cases.iter().map(|c| c.wall_clock_ms).collect());

    if args.json {
        // Machine-readable: emit aligned check deltas + operational deltas.
        let check_deltas: Vec<serde_json::Value> = b
            .checks
            .iter()
            .map(|bc| {
                let base = a.checks.iter().find(|ac| ac.name == bc.name);
                serde_json::json!({
                    "name": bc.name,
                    "baseline_violation_rate": base.map(|x| x.violation_rate),
                    "candidate_violation_rate": bc.violation_rate,
                    "delta": base.map(|x| bc.violation_rate - x.violation_rate),
                })
            })
            .collect();
        let mut out = serde_json::json!({
            "baseline": args.baseline.display().to_string(),
            "candidate": args.candidate.display().to_string(),
            "operational": {
                "case_count": [a.case_count, b.case_count],
                "error_count": [a.error_count, b.error_count],
                "tokens_in": [a.total_usage.input_tokens, b.total_usage.input_tokens],
                "tokens_out": [a.total_usage.output_tokens, b.total_usage.output_tokens],
                "latency_p50_ms": [a_lat, b_lat],
            },
            "checks": check_deltas,
        });
        if let Some((mode, td)) = &traj_diff {
            out["trajectory"] = serde_json::json!({
                "mode": mode.as_str(),
                "paired": td.paired,
                "a_only": td.a_only,
                "b_only": td.b_only,
                "divergent": td.divergent,
                "divergence_rate": if td.paired == 0 { 0.0 } else { td.divergent as f64 / td.paired as f64 },
                "mean_similarity": td.mean_similarity,
                "diverging_ids": td.diverging_ids,
                "mcnemar": td.mcnemar.iter().map(|(name, r)| serde_json::json!({
                    "check": name,
                    "regressed_b": r.b,
                    "improved_c": r.c,
                    "chi_squared": r.chi_squared,
                    "significant_at_05": r.significant_at_05,
                    "significant_at_01": r.significant_at_01,
                })).collect::<Vec<_>>(),
            });
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Human table.
    println!("eval diff");
    println!("  baseline:  {}", args.baseline.display());
    println!("  candidate: {}", args.candidate.display());
    println!();
    println!("operational:");
    println!(
        "  {:<14} {:>10} → {:>10}   {}",
        "cases",
        a.case_count,
        b.case_count,
        delta_i(a.case_count as i64, b.case_count as i64)
    );
    println!(
        "  {:<14} {:>10} → {:>10}   {}",
        "errors",
        a.error_count,
        b.error_count,
        delta_i(a.error_count as i64, b.error_count as i64)
    );
    println!(
        "  {:<14} {:>10} → {:>10}   {}",
        "tokens in",
        a.total_usage.input_tokens,
        b.total_usage.input_tokens,
        delta_pct(
            a.total_usage.input_tokens as f64,
            b.total_usage.input_tokens as f64
        )
    );
    println!(
        "  {:<14} {:>10} → {:>10}   {}",
        "tokens out",
        a.total_usage.output_tokens,
        b.total_usage.output_tokens,
        delta_pct(
            a.total_usage.output_tokens as f64,
            b.total_usage.output_tokens as f64
        )
    );
    println!(
        "  {:<14} {:>9}ms → {:>9}ms   {}",
        "latency p50",
        a_lat,
        b_lat,
        delta_pct(a_lat as f64, b_lat as f64)
    );

    if !a.checks.is_empty() || !b.checks.is_empty() {
        println!();
        println!("checks (violation rate):");
        // Union of check names, candidate order first.
        let mut names: Vec<String> = b.checks.iter().map(|c| c.name.clone()).collect();
        for ac in &a.checks {
            if !names.contains(&ac.name) {
                names.push(ac.name.clone());
            }
        }
        for name in &names {
            let av = a
                .checks
                .iter()
                .find(|c| &c.name == name)
                .map(|c| c.violation_rate);
            let bv = b
                .checks
                .iter()
                .find(|c| &c.name == name)
                .map(|c| c.violation_rate);
            let arrow = match (av, bv) {
                (Some(x), Some(y)) => format!(
                    "{:>6.1}% → {:>6.1}%   {}",
                    x * 100.0,
                    y * 100.0,
                    delta_pp(x, y)
                ),
                (None, Some(y)) => format!("    —   → {:>6.1}%   (new)", y * 100.0),
                (Some(x), None) => format!("{:>6.1}% →     —     (dropped)", x * 100.0),
                (None, None) => "—".into(),
            };
            println!("  {name:<24} {arrow}");
        }
    }

    // Tool-trajectory tier — opt-in via --trajectory.
    if let Some((mode, td)) = &traj_diff {
        println!();
        println!("trajectory (mode={}):", mode.as_str());
        println!(
            "  paired cases:   {} (a-only {}, b-only {})",
            td.paired, td.a_only, td.b_only
        );
        let rate = if td.paired == 0 {
            0.0
        } else {
            td.divergent as f64 / td.paired as f64
        };
        println!(
            "  divergence:     {:>5.1}%  ({}/{} cases differ)   mean similarity {:.2}",
            rate * 100.0,
            td.divergent,
            td.paired,
            td.mean_similarity
        );
        if !td.diverging_ids.is_empty() {
            const CAP: usize = 12;
            let shown: Vec<&str> = td.diverging_ids.iter().take(CAP).map(String::as_str).collect();
            let more = td.diverging_ids.len().saturating_sub(CAP);
            let suffix = if more > 0 { format!(", +{more} more") } else { String::new() };
            println!("  diverging:      {}{}", shown.join(", "), suffix);
        }
        if td.mcnemar.is_empty() {
            println!(
                "  McNemar:        (no shared trajectory-match check — run both with \
                 --check trajectory-match:<mode> for significance)"
            );
        }
        for (name, r) in &td.mcnemar {
            match r.chi_squared {
                None => println!("  McNemar ({name}): no discordant pairs (runs agree)"),
                Some(chi2) => {
                    let verdict = if r.significant_at_01 {
                        "significant at α=0.01"
                    } else if r.significant_at_05 {
                        "significant at α=0.05"
                    } else {
                        "NOT significant at α=0.05"
                    };
                    // b = base-pass/cand-fail (regressed), c = base-fail/cand-pass (improved)
                    println!(
                        "  McNemar ({name}): regressed b={} improved c={} χ²={chi2:.2} → {verdict}",
                        r.b, r.c
                    );
                }
            }
        }
    }

    // Quality tier — only when both runs have a judge_report.json.
    if let (Some(ja), Some(jb)) = (
        load_judge_report(&args.baseline),
        load_judge_report(&args.candidate),
    ) {
        use tars_runtime::{JudgeVerdict, mcnemar};
        println!();
        println!("quality (judge: {} → {}):", ja.judge_id, jb.judge_id);
        let pa = ja.precision().map(|p| p * 100.0);
        let pb = jb.precision().map(|p| p * 100.0);
        match (pa, pb) {
            (Some(x), Some(y)) => println!(
                "  precision   {:>6.1}% → {:>6.1}%   {}",
                x,
                y,
                delta_pp_higher_better(x / 100.0, y / 100.0)
            ),
            _ => println!("  precision   (no decisive verdicts)"),
        }

        // Paired McNemar over shared item ids.
        let to_map = |r: &tars_runtime::JudgeReport| {
            r.verdicts
                .iter()
                .map(|v| {
                    (
                        v.item_id.clone(),
                        matches!(v.verdict, JudgeVerdict::TruePositive),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let m = mcnemar(&to_map(&ja), &to_map(&jb));
        let improved = m.c;
        let regressed = m.b;
        println!("  paired changes: improved (FP→TP)={improved}, regressed (TP→FP)={regressed}");
        match m.chi_squared {
            None => println!("  McNemar: no discordant pairs (runs agree on every item)"),
            Some(chi2) => {
                let verdict = if m.significant_at_01 {
                    "significant at α=0.01"
                } else if m.significant_at_05 {
                    "significant at α=0.05"
                } else {
                    "NOT significant at α=0.05 (need more cases or bigger effect)"
                };
                println!("  McNemar: b={regressed} c={improved} χ²={chi2:.2} → {verdict}");
            }
        }
    }
    Ok(())
}

fn load_judge_report(dir: &Path) -> Option<tars_runtime::JudgeReport> {
    let path = dir.join("judge_report.json");
    let body = read_optional_text(&path).ok().flatten()?;
    serde_json::from_str(&body).ok()
}

/// Integer delta with sign + direction marker.
fn delta_i(a: i64, b: i64) -> String {
    let d = b - a;
    match d.cmp(&0) {
        std::cmp::Ordering::Equal => "(=)".into(),
        std::cmp::Ordering::Greater => format!("(+{d})"),
        std::cmp::Ordering::Less => format!("({d})"),
    }
}

/// Percent change a→b.
fn delta_pct(a: f64, b: f64) -> String {
    if a == 0.0 {
        return if b == 0.0 {
            "(=)".into()
        } else {
            "(new)".into()
        };
    }
    let pct = (b - a) / a * 100.0;
    if pct.abs() < 0.05 {
        "(=)".into()
    } else if pct > 0.0 {
        format!("(+{pct:.0}%)")
    } else {
        format!("({pct:.0}%)")
    }
}

/// Percentage-point delta for a rate where UP is WORSE (violation rate).
fn delta_pp(a: f64, b: f64) -> String {
    let pp = (b - a) * 100.0;
    if pp.abs() < 0.05 {
        "(=)".into()
    } else if pp > 0.0 {
        format!("(+{pp:.1}pp ⚠)") // violation rate UP = worse
    } else {
        format!("({pp:.1}pp ✓)") // violation rate DOWN = better
    }
}

/// Percentage-point delta for a rate where UP is BETTER (precision).
fn delta_pp_higher_better(a: f64, b: f64) -> String {
    let pp = (b - a) * 100.0;
    if pp.abs() < 0.05 {
        "(=)".into()
    } else if pp > 0.0 {
        format!("(+{pp:.1}pp ✓)") // precision UP = better
    } else {
        format!("({pp:.1}pp ⚠)") // precision DOWN = worse
    }
}

async fn run_eval(args: EvalRunArgs, config_path: Option<PathBuf>) -> Result<()> {
    // 1. Config + registry + provider selection — same path tars run uses.
    let cfg = config_loader::load(config_path)?;
    let registry = build_registry_with_breaker(&cfg, /* breaker */ true)?;
    let provider_id = pick_provider(&cfg, args.provider.as_deref())?;
    let provider = registry
        .get(&provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider {provider_id} missing from registry"))?;

    // 2. Build pipeline using the canonical default chain. Cache
    //    namespace is unique-per-run so cases don't unexpectedly hit
    //    each other across runs (and we still get intra-run cache
    //    benefits if cases share prompts — rare but free).
    let pipeline = LlmService::default_chain(
        provider,
        args.model.clone().unwrap_or_default(),
        ChainOpts::new(provider_id.clone()),
    );

    // 2b. Build checks from --check specs. trajectory-match:* specs are
    //     case-parameterized (need per-case expected_tools), so they're split
    //     out from the global invariant CheckRunner and evaluated in the loop.
    let mut invariants: Vec<Arc<dyn Invariant>> = Vec::with_capacity(args.checks.len());
    let mut traj_specs: Vec<TrajectorySpec> = Vec::new();
    for spec in &args.checks {
        if let Some(ts) = TrajectorySpec::parse(spec)? {
            traj_specs.push(ts);
        } else {
            invariants.push(build_invariant(spec)?);
        }
    }
    let check_runner = CheckRunner::new(invariants);
    // Aggregation rolls up every check name — invariants AND trajectory checks.
    let mut check_names: Vec<String> = check_runner.names().iter().map(|s| s.to_string()).collect();
    check_names.extend(traj_specs.iter().map(|t| t.name().to_string()));

    // 2c. Agent mode (Doc 26 M2''): run each case through a read-only,
    //     sandboxed tool-using agent instead of a single completion.
    let agent_mode = if args.agent {
        Some(AgentMode {
            tools: args.tools.clone(),
            max_iterations: args.agent_max_iterations,
        })
    } else {
        if !args.tools.is_empty() || args.agent_max_iterations.is_some() {
            anyhow::bail!("--tool / --agent-max-iterations require --agent");
        }
        None
    };

    // 2d. Arg-equivalence judge (Doc 26 M3' pt2): built only when a
    //     trajectory-match:args-judge check is requested.
    let arg_judge = if traj_specs.iter().any(|t| t.judge) {
        let jp = args.judge_provider.as_deref().ok_or_else(|| {
            anyhow::anyhow!("trajectory-match:args-judge requires --judge-provider")
        })?;
        // Anti-incest: the judge must not be the provider under test.
        ensure_anti_incest(jp, &[provider_id.as_str()]).map_err(|e| {
            anyhow::anyhow!("{e}\nthe run uses provider `{provider_id}`; pick a different --judge-provider")
        })?;
        let jpid = ProviderId::new(jp);
        let jprov = registry
            .get(&jpid)
            .ok_or_else(|| anyhow::anyhow!("judge provider `{jp}` not in config"))?;
        let jpipeline = LlmService::default_chain(
            jprov,
            args.judge_model.clone().unwrap_or_default(),
            ChainOpts::new(jpid.clone()),
        );
        let jid = format!("{}:{}", jp, args.judge_model.as_deref().unwrap_or("default"));
        Some(ArgEquivalenceJudge::new(jpipeline, jid))
    } else {
        if args.judge_provider.is_some() || args.judge_model.is_some() {
            anyhow::bail!(
                "--judge-provider / --judge-model only apply to a \
                 `--check trajectory-match:args-judge` check"
            );
        }
        None
    };

    // 3. Discover cases.
    let cases = load_corpus(&args.corpus)
        .with_context(|| format!("loading corpus from {}", args.corpus.display()))?;
    if cases.is_empty() {
        anyhow::bail!(
            "corpus at {} contains no readable cases (need subdirectories with input.txt)",
            args.corpus.display()
        );
    }
    eprintln!(
        "── eval: {} cases, provider={}, model={}",
        cases.len(),
        provider_id,
        args.model.as_deref().unwrap_or("(provider default)"),
    );

    // 4. Output directory.
    let output_dir = match args.output.clone() {
        Some(p) => p,
        None => PathBuf::from(format!("benchmarks/runs/eval/{}", utc_now_stamp())),
    };
    ensure_dir(&output_dir)?;

    // 5. Per-case loop. Failures are recorded into the report, not
    //    propagated — the value of an eval is seeing the distribution.
    let started_at_ms = utc_now_millis();
    let mut reports: Vec<EvalCaseReport> = Vec::with_capacity(cases.len());
    let mut total_usage = Usage::default();
    let model_label = args
        .model
        .clone()
        .unwrap_or_else(|| "(provider default)".into());

    for case in &cases {
        eprint!("── {} ... ", case.id);
        let case_out_dir = output_dir.join(&case.id);
        ensure_dir(&case_out_dir)?;
        // Agent-mode sandbox = the case's INPUT dir (read-only tools jail here).
        let case_dir = args.corpus.join(&case.id);
        let report = run_one_case(
            pipeline.clone(),
            case,
            &case_dir,
            args.model.as_deref(),
            args.max_output_tokens,
            &check_runner,
            &traj_specs,
            agent_mode.as_ref(),
            arg_judge.as_ref(),
        )
        .await;
        // Persist response text + per-case report regardless of outcome.
        let output_path = case_out_dir.join("output.txt");
        let output_text = report.output_text.clone();
        write_text(&output_path, output_text)?;
        let report_path = case_out_dir.join("report.json");
        write_pretty_json(&report_path, &report.summary)?;

        match &report.summary.status {
            EvalCaseStatus::Ok => {
                eprintln!(
                    "ok ({} tokens in / {} out, {} ms)",
                    report.summary.usage.input_tokens,
                    report.summary.usage.output_tokens,
                    report.summary.wall_clock_ms,
                );
                total_usage = merge_usage(total_usage, &report.summary.usage);
            }
            EvalCaseStatus::Error => {
                eprintln!(
                    "ERR ({} ms): {}",
                    report.summary.wall_clock_ms,
                    report.summary.error.as_deref().unwrap_or("(no detail)"),
                );
            }
        }
        reports.push(report.summary);
    }

    // 6. Aggregate per-check violation rates across all cases.
    let check_summaries: Vec<CheckSummary> = check_names
        .iter()
        .map(|name| {
            let mut evaluated = 0u32;
            let mut violations = 0u32;
            for case in &reports {
                if let Some(c) = case.checks.iter().find(|c| c.name() == name.as_str()) {
                    evaluated += 1;
                    if !c.passed() {
                        violations += 1;
                    }
                }
            }
            CheckSummary {
                name: name.clone(),
                evaluated,
                violations,
                violation_rate: if evaluated == 0 {
                    0.0
                } else {
                    violations as f64 / evaluated as f64
                },
            }
        })
        .collect();

    // 7. Manifest.
    let ended_at_ms = utc_now_millis();
    let manifest = EvalRunManifest {
        corpus_path: args
            .corpus
            .canonicalize()
            .unwrap_or(args.corpus.clone())
            .display()
            .to_string(),
        provider_id: provider_id.as_str().to_string(),
        model: model_label,
        started_at_ms,
        ended_at_ms,
        case_count: reports.len() as u32,
        success_count: reports
            .iter()
            .filter(|r| r.status == EvalCaseStatus::Ok)
            .count() as u32,
        error_count: reports
            .iter()
            .filter(|r| r.status == EvalCaseStatus::Error)
            .count() as u32,
        total_usage,
        checks: check_summaries,
        cases: reports,
    };
    let manifest_path = output_dir.join("manifest.json");
    write_pretty_json(&manifest_path, &manifest)?;

    eprintln!(
        "── done. {} ok, {} error, total {} in / {} out tokens.",
        manifest.success_count,
        manifest.error_count,
        manifest.total_usage.input_tokens,
        manifest.total_usage.output_tokens,
    );
    for c in &manifest.checks {
        eprintln!(
            "── check {}: {:.0}% violation ({}/{} cases)",
            c.name,
            c.violation_rate * 100.0,
            c.violations,
            c.evaluated,
        );
    }
    eprintln!("── manifest: {}", manifest_path.display());
    Ok(())
}

// ─── one case ─────────────────────────────────────────────────────────

/// One case loaded from disk. `expected` is preserved but not used by
/// `eval run` itself — read by `eval judge` later.
struct Case {
    id: String,
    input: String,
    system: Option<String>,
    #[allow(dead_code)]
    expected: Option<String>,
    /// Reference tool trajectory for `--check trajectory-match:*` (Doc 26).
    /// `None` when the case carries no `expected_tools.json` → the check is
    /// skipped for this case (never a silent pass).
    expected_tools: Option<Vec<ToolStep>>,
}

/// One entry of `expected_tools.json` — either a bare tool name or a
/// `{name, args}` object. Args are accepted for forward-compat (Doc 26 P3
/// `args` mode) but unused by P1's name-only scoring.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ExpectedToolEntry {
    Name(String),
    Step {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
}

/// Read `expected_tools.json` if present. Malformed JSON is a hard error
/// (fail-closed, names the file) — never a silent skip.
fn read_expected_tools(path: &Path) -> Result<Option<Vec<ToolStep>>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = read_text(path)?;
    let entries: Vec<ExpectedToolEntry> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} (expect a JSON array of tool names or {{name,args}})", path.display()))?;
    let steps = entries
        .into_iter()
        .map(|e| match e {
            ExpectedToolEntry::Name(name) => ToolStep {
                name,
                args: serde_json::Value::Null,
            },
            ExpectedToolEntry::Step { name, args } => ToolStep { name, args },
        })
        .collect();
    Ok(Some(steps))
}

struct CaseOutcome {
    summary: EvalCaseReport,
    output_text: String,
}

/// Build the base request for a case (shared by both execution modes).
/// Model-agnostic content — the model is bound on the pipeline.
fn build_case_request(case: &Case, max_output_tokens: Option<u32>) -> ChatRequest {
    let mut req = ChatRequest::user(case.input.clone());
    if let Some(sys) = &case.system {
        req = req.with_system(sys.clone());
    }
    if let Some(cap) = max_output_tokens {
        req.max_output_tokens = Some(cap);
    }
    req
}

/// Single-completion execution (the default mode). Returns the response, the
/// tool steps the model requested (full args), and any error.
async fn run_completion_case(
    pipeline: LlmService,
    req: ChatRequest,
) -> (ChatResponse, Vec<ToolStep>, Option<String>) {
    let ctx = RequestContext::test_default();
    let stream_result = pipeline.call(req, ctx).await;
    let mut acc = ChatResponseBuilder::new();
    let mut error: Option<String> = None;
    match stream_result {
        Ok(mut stream) => {
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ev) => acc.apply(ev),
                    Err(e) => {
                        error = Some(truncate(&format!("{e}"), 500));
                        break;
                    }
                }
            }
        }
        Err(e) => error = Some(truncate(&format!("{e}"), 500)),
    }
    let response = acc.finish();
    let tool_steps = trajectory_match::from_tool_calls(&response.tool_calls);
    (response, tool_steps, error)
}

/// Build the read-only, sandboxed tool registry for `--agent` mode (Doc 26
/// §15.2). Only the allow-listed read-only builtins, each jailed to `sandbox`;
/// `bash`/`edit_file`/`write_file` are refused.
fn build_agent_registry(tools: &[String], sandbox: &Path) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    let names: Vec<&str> = if tools.is_empty() {
        vec!["read_file", "grep", "glob", "list_dir"]
    } else {
        tools.iter().map(String::as_str).collect()
    };
    for name in names {
        let jail_err =
            || anyhow::anyhow!("cannot jail `{name}` to sandbox `{}`", sandbox.display());
        let tool: Arc<dyn Tool> = match name {
            "read_file" => Arc::new(ReadFileTool::with_root(sandbox).ok_or_else(jail_err)?),
            "grep" => Arc::new(GrepTool::with_root(sandbox).ok_or_else(jail_err)?),
            "glob" => Arc::new(GlobTool::with_root(sandbox).ok_or_else(jail_err)?),
            "list_dir" => Arc::new(ListDirTool::with_root(sandbox).ok_or_else(jail_err)?),
            "bash" | "edit_file" | "write_file" => anyhow::bail!(
                "--tool `{name}` is refused in eval: only READ-ONLY tools may run \
                 against untrusted corpus cases (Doc 26 §15.2)"
            ),
            other => anyhow::bail!(
                "unknown --tool `{other}`. Allowed (read-only): read_file, grep, glob, list_dir"
            ),
        };
        reg.register(tool)
            .map_err(|e| anyhow::anyhow!("registering tool `{name}`: {e}"))?;
    }
    Ok(reg)
}

/// Synthesize a minimal `ChatResponse` from an agent run's final text, so the
/// invariant checks (which read `resp.text`) work in agent mode too.
fn synth_response(text: String, usage: Usage) -> ChatResponse {
    ChatResponse {
        actual_model: String::new(),
        text,
        thinking: String::new(),
        tool_calls: Vec::new(),
        stop_reason: None,
        usage,
        cache_hit: Default::default(),
        validation_summary: Default::default(),
        created: 0,
    }
}

/// Tool-using agent-loop execution (`--agent`). Runs a `WorkerAgent::with_tools`
/// over a read-only sandbox; returns a synthesized response (final text), the
/// cross-call tool steps (names only — M2), and any error.
async fn run_worker_case(
    pipeline: LlmService,
    mut req: ChatRequest,
    model: Option<&str>,
    sandbox: &Path,
    agent: &AgentMode,
) -> (ChatResponse, Vec<ToolStep>, Option<String>) {
    let registry = match build_agent_registry(&agent.tools, sandbox) {
        Ok(r) => r,
        Err(e) => return (synth_response(String::new(), Usage::default()), Vec::new(), Some(truncate(&format!("{e}"), 500))),
    };
    req.tools = registry.to_tool_specs();

    let mut worker = WorkerAgent::with_tools(
        "eval-worker",
        model.unwrap_or(""),
        "eval",
        Arc::new(registry),
    );
    if let Some(n) = agent.max_iterations {
        worker = worker.with_max_tool_iterations(n);
    }
    let ctx = AgentContext {
        trajectory_id: TrajectoryId::new("eval-agent"),
        step_seq: 1,
        llm: pipeline,
        cancel: CancellationToken::new(),
        cwd: Some(sandbox.to_path_buf()),
        permissions: Default::default(),
        readable_roots: Vec::new(),
        // Eval harness: tools are already jailed to the eval `sandbox` dir via
        // `cwd` + `with_root`; the OS-confinement policy stays unconfined
        // (DangerFullAccess) here, unchanged from before M4.
        sandbox: Default::default(),
        llm_request_ctx: None,
        stream_hooks: None,
    };
    match worker.execute(ctx, req).await {
        Ok(result) => {
            let text = match &result.output {
                AgentOutput::Text { text } | AgentOutput::Mixed { text, .. } => text.clone(),
                AgentOutput::ToolCalls { .. } => String::new(),
            };
            // Cross-call tool sequence with args (M2 names + M3' args).
            let tool_steps: Vec<ToolStep> = result
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, n)| ToolStep {
                    name: n.clone(),
                    args: result
                        .tool_call_args
                        .get(i)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                })
                .collect();
            (synth_response(text, result.usage), tool_steps, None)
        }
        Err(e) => (
            synth_response(String::new(), Usage::default()),
            Vec::new(),
            Some(truncate(&format!("{e}"), 500)),
        ),
    }
}

#[allow(clippy::too_many_arguments)] // each arg is a distinct per-case input; a
// struct would just move the same fields elsewhere without clarifying anything.
async fn run_one_case(
    pipeline: LlmService,
    case: &Case,
    case_dir: &Path,
    model: Option<&str>,
    max_output_tokens: Option<u32>,
    checks: &CheckRunner,
    traj_specs: &[TrajectorySpec],
    agent: Option<&AgentMode>,
    arg_judge: Option<&ArgEquivalenceJudge>,
) -> CaseOutcome {
    let started = Instant::now();
    let req = build_case_request(case, max_output_tokens);
    // Keep a copy for the invariants (execution consumes the original).
    let req_for_checks = req.clone();

    let (response, tool_steps, error) = match agent {
        Some(am) => run_worker_case(pipeline.clone(), req, model, case_dir, am).await,
        None => run_completion_case(pipeline.clone(), req).await,
    };

    let output_text = response.text.clone();
    let status = if error.is_some() {
        EvalCaseStatus::Error
    } else {
        EvalCaseStatus::Ok
    };

    // Tool names the model selected, in call order — recorded regardless of
    // whether a trajectory check was requested (lets `eval diff --trajectory`
    // work without re-inferring).
    let tool_trajectory: Vec<String> = tool_steps.iter().map(|s| s.name.clone()).collect();

    // Run checks only on successful cases — an errored call has no real
    // output to check.
    let mut case_checks: Vec<CaseCheckResult> = if status == EvalCaseStatus::Ok && !checks.is_empty()
    {
        checks
            .run(&req_for_checks, &response)
            .into_iter()
            .map(|(name, r)| {
                if r.passed {
                    CaseCheckResult::Passed {
                        name,
                        note: r.detail,
                    }
                } else {
                    // CheckResult::fail always populates detail; if it
                    // somehow didn't, surface that as the failure
                    // reason rather than swallow it.
                    CaseCheckResult::Failed {
                        name,
                        reason: r
                            .detail
                            .unwrap_or_else(|| "(check failed without a reason)".into()),
                    }
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // Case-parameterized trajectory-match checks (Doc 26): each scores the
    // selected tools against this case's expected_tools. A case with no
    // expected_tools is skipped (eval_case → None), not failed.
    if status == EvalCaseStatus::Ok {
        let expected = case.expected_tools.as_deref();
        for ts in traj_specs {
            let r = if ts.judge {
                match arg_judge {
                    Some(j) => ts.eval_case_judged(&tool_steps, expected, j).await,
                    // args-judge requested but no judge built — fail loud per case.
                    None => expected.map(|_| CaseCheckResult::Failed {
                        name: ts.name().to_string(),
                        reason: "args-judge requires --judge-provider".into(),
                    }),
                }
            } else {
                ts.eval_case(&tool_steps, expected)
            };
            if let Some(r) = r {
                case_checks.push(r);
            }
        }
    }

    CaseOutcome {
        summary: EvalCaseReport {
            case_id: case.id.clone(),
            status,
            wall_clock_ms: started.elapsed().as_millis() as u64,
            usage: response.usage,
            output_chars: output_text.chars().count() as u64,
            error,
            checks: case_checks,
            tool_trajectory,
        },
        output_text,
    }
}

// ─── corpus loader ────────────────────────────────────────────────────

fn load_corpus(root: &Path) -> Result<Vec<Case>> {
    if !root.is_dir() {
        anyhow::bail!("corpus path is not a directory: {}", root.display());
    }
    let mut cases: Vec<Case> = Vec::new();
    for entry in list_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() || id.starts_with('.') {
            continue;
        }
        let input_path = path.join("input.txt");
        if !input_path.exists() {
            tracing::warn!(case = %id, "skipping case: no input.txt");
            continue;
        }
        let input = read_text(&input_path)?;
        let system = read_optional_text(&path.join("system.txt"))?;
        let expected = read_optional_text(&path.join("expected.txt"))?;
        let expected_tools = read_expected_tools(&path.join("expected_tools.json"))?;
        cases.push(Case {
            id,
            input,
            system,
            expected,
            expected_tools,
        });
    }
    // Stable order — caller asks for case_001 < case_002 < case_010 to
    // sort lexically with zero-padding; we don't otherwise re-order.
    cases.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(cases)
}

// ─── helpers ──────────────────────────────────────────────────────────

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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

fn utc_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn utc_now_stamp() -> String {
    // Format that file systems and humans both tolerate: 2026-05-20T15-30-00.
    // Doesn't depend on `chrono` (tars-cli avoids the dep).
    let secs = utc_now_millis() / 1000;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days_since_epoch = secs / 86400;
    let date = civil_from_days(days_since_epoch as i64);
    let (y, mo, d) = (date.year, date.month, date.day);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{m:02}-{s:02}")
}

/// Calendar date — return shape for [`civil_from_days`]. Named fields
/// so callers can't transpose y/m/d on destructure (the specific
/// `arc scan --judge` finding for this fn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CivilDate {
    year: i32,
    month: u32,
    day: u32,
}

/// Days since 1970-01-01 → calendar date. Pure function, no timezone,
/// no leap-second handling — good enough for filenames.
fn civil_from_days(z: i64) -> CivilDate {
    // Howard Hinnant's date algorithm, simplified for >=1970.
    let z = z + 719_468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    CivilDate {
        year: y,
        month: m,
        day: d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn tsteps(names: &[&str]) -> Vec<ToolStep> {
        names
            .iter()
            .map(|n| ToolStep {
                name: (*n).to_string(),
                args: serde_json::Value::Null,
            })
            .collect()
    }

    // ── trajectory-match: spec parsing (Doc 26 FR-6) ──────────────────

    #[test]
    fn trajectory_spec_parses_modes_threshold_and_bare() {
        // non-trajectory spec → None (falls back to build_invariant)
        assert!(TrajectorySpec::parse("valid-json").unwrap().is_none());

        let bare = TrajectorySpec::parse("trajectory-match").unwrap().unwrap();
        assert_eq!(bare.mode, MatchMode::Ordered); // bare defaults to ordered
        assert_eq!(bare.threshold, 1.0);
        assert_eq!(bare.name(), "trajectory-match");

        let exact = TrajectorySpec::parse("trajectory-match:exact").unwrap().unwrap();
        assert_eq!(exact.mode, MatchMode::Exact);

        let thr = TrajectorySpec::parse("trajectory-match:ordered:0.8").unwrap().unwrap();
        assert_eq!(thr.mode, MatchMode::Ordered);
        assert!((thr.threshold - 0.8).abs() < 1e-9);
    }

    #[test]
    fn trajectory_spec_rejects_unknown_mode_and_bad_threshold() {
        assert!(TrajectorySpec::parse("trajectory-match:fuzzy").is_err());
        assert!(TrajectorySpec::parse("trajectory-match:exact:notanum").is_err());
    }

    #[test]
    fn trajectory_spec_args_judge_sets_judge_flag() {
        let s = TrajectorySpec::parse("trajectory-match:args-judge").unwrap().unwrap();
        assert_eq!(s.mode, MatchMode::Args);
        assert!(s.judge, "args-judge must set the judge flag");
        assert_eq!(s.name(), "trajectory-match:args-judge");
        // plain args mode does NOT enable the judge
        let plain = TrajectorySpec::parse("trajectory-match:args").unwrap().unwrap();
        assert!(!plain.judge);
    }

    // ── trajectory-match: per-case scoring (Doc 26 E2E-1/2/5) ─────────

    #[test]
    fn trajectory_eval_case_passes_on_match() {
        let spec = TrajectorySpec::parse("trajectory-match:exact").unwrap().unwrap();
        let expected = tsteps(&["search"]);
        let r = spec.eval_case(&tsteps(&["search"]), Some(&expected)).unwrap();
        assert!(r.passed());
        assert_eq!(r.name(), "trajectory-match:exact");
    }

    #[test]
    fn trajectory_eval_case_fails_on_mismatch_with_reason() {
        let spec = TrajectorySpec::parse("trajectory-match:exact").unwrap().unwrap();
        let expected = tsteps(&["search"]);
        let r = spec.eval_case(&tsteps(&["fetch"]), Some(&expected)).unwrap();
        assert!(!r.passed());
        // reason names both want and got — not a bare failure
        let detail = r.detail().unwrap();
        assert!(detail.contains("search") && detail.contains("fetch"), "detail={detail}");
    }

    // ── agent mode registry (Doc 26 M2'' §15.2 security boundary) ─────

    #[test]
    fn agent_registry_allows_readonly_tools_jailed_to_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        // default (empty) → all four read-only tools
        let reg = build_agent_registry(&[], dir.path()).unwrap();
        assert_eq!(reg.to_tool_specs().len(), 4);
        // explicit subset
        let reg2 =
            build_agent_registry(&["read_file".into(), "grep".into()], dir.path()).unwrap();
        assert_eq!(reg2.to_tool_specs().len(), 2);
    }

    #[test]
    fn agent_registry_refuses_dangerous_and_unknown_tools() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["bash", "edit_file", "write_file"] {
            // `.err().unwrap()` (not `unwrap_err`) — ToolRegistry isn't Debug.
            let e = build_agent_registry(&[bad.to_string()], dir.path())
                .err()
                .unwrap()
                .to_string();
            assert!(
                e.contains("refused") || e.contains("READ-ONLY"),
                "tool `{bad}` must be refused: {e}"
            );
        }
        // unknown tool name
        assert!(build_agent_registry(&["frobnicate".into()], dir.path()).is_err());
    }

    #[test]
    fn trajectory_eval_case_args_mode_checks_arguments() {
        let spec = TrajectorySpec::parse("trajectory-match:args").unwrap().unwrap();
        let s = |n: &str, a: serde_json::Value| ToolStep { name: n.into(), args: a };
        let expected = vec![s("search", serde_json::json!({"q": "x"}))];
        // same name + same args → pass
        assert!(
            spec.eval_case(&[s("search", serde_json::json!({"q": "x"}))], Some(&expected))
                .unwrap()
                .passed()
        );
        // same name, WRONG args → fail (what args mode adds over exact)
        assert!(
            !spec
                .eval_case(&[s("search", serde_json::json!({"q": "y"}))], Some(&expected))
                .unwrap()
                .passed()
        );
    }

    #[test]
    fn trajectory_eval_case_skips_when_no_expected_tools() {
        let spec = TrajectorySpec::parse("trajectory-match:ordered").unwrap().unwrap();
        // None expected → skipped (not a silent pass)
        assert!(spec.eval_case(&tsteps(&["search"]), None).is_none());
    }

    #[test]
    fn trajectory_ordered_threshold_allows_partial_credit() {
        // ordered score for [a,x] vs [a,y] is 0.5; threshold 0.5 passes, 0.6 fails
        let pass = TrajectorySpec::parse("trajectory-match:ordered:0.5").unwrap().unwrap();
        let fail = TrajectorySpec::parse("trajectory-match:ordered:0.6").unwrap().unwrap();
        let expected = tsteps(&["a", "y"]);
        assert!(pass.eval_case(&tsteps(&["a", "x"]), Some(&expected)).unwrap().passed());
        assert!(!fail.eval_case(&tsteps(&["a", "x"]), Some(&expected)).unwrap().passed());
    }

    // ── corpus expected_tools.json (Doc 26 FR-5) ──────────────────────

    #[test]
    fn read_expected_tools_accepts_names_and_objects_and_missing() {
        let dir = TempDir::new().unwrap();
        // missing file → None
        assert!(read_expected_tools(&dir.path().join("nope.json")).unwrap().is_none());

        // bare names
        let names = dir.path().join("names.json");
        write(&names, r#"["search","fetch"]"#);
        let got = read_expected_tools(&names).unwrap().unwrap();
        assert_eq!(got.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["search", "fetch"]);

        // {name,args} objects
        let objs = dir.path().join("objs.json");
        write(&objs, r#"[{"name":"search","args":{"q":"x"}}]"#);
        let got = read_expected_tools(&objs).unwrap().unwrap();
        assert_eq!(got[0].name, "search");
        assert_eq!(got[0].args, serde_json::json!({"q":"x"}));
    }

    #[test]
    fn read_expected_tools_fails_closed_on_malformed() {
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("bad.json");
        write(&bad, "{ not an array");
        assert!(read_expected_tools(&bad).is_err());
    }

    // ── head-to-head eval diff --trajectory (Doc 26 E2E-6) ───────────

    fn ecase(id: &str, traj: &[&str], check: Option<(&str, bool)>) -> EvalCaseReport {
        let checks = match check {
            Some((name, true)) => vec![CaseCheckResult::Passed { name: name.into(), note: None }],
            Some((name, false)) => {
                vec![CaseCheckResult::Failed { name: name.into(), reason: "x".into() }]
            }
            None => vec![],
        };
        EvalCaseReport {
            case_id: id.into(),
            status: EvalCaseStatus::Ok,
            wall_clock_ms: 0,
            usage: Usage::default(),
            output_chars: 0,
            error: None,
            checks,
            tool_trajectory: traj.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn emanifest(cases: Vec<EvalCaseReport>, check_names: &[&str]) -> EvalRunManifest {
        EvalRunManifest {
            corpus_path: String::new(),
            provider_id: String::new(),
            model: String::new(),
            started_at_ms: 0,
            ended_at_ms: 0,
            case_count: cases.len() as u32,
            success_count: cases.len() as u32,
            error_count: 0,
            total_usage: Usage::default(),
            checks: check_names
                .iter()
                .map(|n| CheckSummary {
                    name: (*n).into(),
                    evaluated: 0,
                    violations: 0,
                    violation_rate: 0.0,
                })
                .collect(),
            cases,
        }
    }

    #[test]
    fn compute_traj_diff_counts_divergence_and_mcnemar() {
        let cn = "trajectory-match:exact";
        let a = emanifest(
            vec![
                ecase("c1", &["s"], Some((cn, true))),
                ecase("c2", &["s"], Some((cn, true))),
                ecase("c3", &["s"], Some((cn, false))),
                ecase("c4", &["s"], Some((cn, false))),
            ],
            &[cn],
        );
        let b = emanifest(
            vec![
                ecase("c1", &["s"], Some((cn, true))),  // same traj; A✓ B✓
                ecase("c2", &["x"], Some((cn, false))), // differ; A✓ B✗ → b
                ecase("c3", &["x"], Some((cn, true))),  // differ; A✗ B✓ → c
                ecase("c4", &["s"], Some((cn, false))), // same traj; A✗ B✗
            ],
            &[cn],
        );
        let td = compute_traj_diff(&a, &b, MatchMode::Exact);
        assert_eq!(td.paired, 4);
        assert_eq!(td.divergent, 2);
        assert_eq!(td.diverging_ids, vec!["c2", "c3"]);
        assert_eq!(td.a_only, 0);
        assert_eq!(td.b_only, 0);
        assert_eq!(td.mcnemar.len(), 1);
        let (name, r) = &td.mcnemar[0];
        assert_eq!(name, cn);
        assert_eq!(r.b, 1); // regressed: c2 (A pass, B fail)
        assert_eq!(r.c, 1); // improved:  c3 (A fail, B pass)
    }

    #[test]
    fn compute_traj_diff_reports_unpaired_cases_and_no_shared_check() {
        // a has c1,c2; b has c2,c3 → 1 paired, 1 a-only, 1 b-only.
        // No trajectory-match check anywhere → empty mcnemar (graceful).
        let a = emanifest(vec![ecase("c1", &["s"], None), ecase("c2", &["s"], None)], &[]);
        let b = emanifest(vec![ecase("c2", &["s"], None), ecase("c3", &["s"], None)], &[]);
        let td = compute_traj_diff(&a, &b, MatchMode::Ordered);
        assert_eq!(td.paired, 1);
        assert_eq!(td.a_only, 1);
        assert_eq!(td.b_only, 1);
        assert_eq!(td.divergent, 0); // the one paired case (c2) matches
        assert!(td.mcnemar.is_empty());
    }

    #[test]
    fn load_corpus_reads_expected_tools_into_case() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("c1").join("input.txt"), "hi");
        write(&dir.path().join("c1").join("expected_tools.json"), r#"["search"]"#);
        let cases = load_corpus(dir.path()).unwrap();
        let et = cases[0].expected_tools.as_ref().unwrap();
        assert_eq!(et[0].name, "search");
    }

    #[test]
    fn load_corpus_reads_cases_in_lex_order() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("case_002").join("input.txt"), "second");
        write(&dir.path().join("case_001").join("input.txt"), "first");
        write(&dir.path().join("case_001").join("expected.txt"), "exp");
        write(&dir.path().join("case_001").join("system.txt"), "sys");
        let cases = load_corpus(dir.path()).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].id, "case_001");
        assert_eq!(cases[0].input, "first");
        assert_eq!(cases[0].system.as_deref(), Some("sys"));
        assert_eq!(cases[0].expected.as_deref(), Some("exp"));
        assert_eq!(cases[1].id, "case_002");
        assert!(cases[1].system.is_none());
    }

    #[test]
    fn load_corpus_skips_subdirs_without_input() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("good").join("input.txt"), "ok");
        // empty subdir
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        // dot-prefixed (e.g. .DS_Store dir)
        fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        write(&dir.path().join(".hidden").join("input.txt"), "should-skip");
        let cases = load_corpus(dir.path()).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "good");
    }

    #[test]
    fn load_corpus_errors_on_non_dir_path() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("not-a-dir.txt");
        write(&file, "x");
        let err = load_corpus(&file).err().expect("must error");
        assert!(err.to_string().contains("not a directory"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), "abc");
        // truncate at 3 → "abc…" (the "…" is appended, not counted)
        assert_eq!(truncate("abcdef", 3), "abc…");
        // multi-byte safe
        let s = "あいうえお";
        let t = truncate(s, 2);
        assert!(t.starts_with("あい"));
        assert!(t.ends_with('…'));
    }

    #[test]
    fn manifest_serde_round_trip() {
        let m = EvalRunManifest {
            corpus_path: "/tmp/corpus".into(),
            provider_id: "anthropic".into(),
            model: "claude-opus-4-7".into(),
            started_at_ms: 1_000_000,
            ended_at_ms: 1_005_000,
            case_count: 2,
            success_count: 2,
            error_count: 0,
            total_usage: Usage {
                input_tokens: 100,
                output_tokens: 200,
                ..Usage::default()
            },
            checks: vec![CheckSummary {
                name: "non-empty".into(),
                evaluated: 2,
                violations: 0,
                violation_rate: 0.0,
            }],
            cases: vec![EvalCaseReport {
                case_id: "case_001".into(),
                status: EvalCaseStatus::Ok,
                wall_clock_ms: 2500,
                usage: Usage::default(),
                output_chars: 80,
                error: None,
                checks: vec![CaseCheckResult::Passed {
                    name: "non-empty".into(),
                    note: None,
                }],
                tool_trajectory: Vec::new(),
            }],
        };
        let v = serde_json::to_value(&m).unwrap();
        let back: EvalRunManifest = serde_json::from_value(v).unwrap();
        assert_eq!(back.case_count, 2);
        assert_eq!(back.success_count, 2);
        assert_eq!(back.cases[0].status, EvalCaseStatus::Ok);
        assert_eq!(back.checks[0].name, "non-empty");
        assert!(back.cases[0].checks[0].passed());
    }

    #[test]
    fn case_check_result_serialises_as_internally_tagged() {
        // Pin the new wire shape (B-6 cut). Passed.note is omitted
        // when None thanks to skip_serializing_if; Failed.reason
        // is always present.
        let pass = serde_json::to_value(CaseCheckResult::Passed {
            name: "x".into(),
            note: None,
        })
        .unwrap();
        assert_eq!(pass, serde_json::json!({"outcome": "passed", "name": "x"}));

        let pass_with_note = serde_json::to_value(CaseCheckResult::Passed {
            name: "x".into(),
            note: Some("validator skipped".into()),
        })
        .unwrap();
        assert_eq!(
            pass_with_note,
            serde_json::json!({
                "outcome": "passed",
                "name": "x",
                "note": "validator skipped",
            })
        );

        let fail = serde_json::to_value(CaseCheckResult::Failed {
            name: "y".into(),
            reason: "bad".into(),
        })
        .unwrap();
        assert_eq!(
            fail,
            serde_json::json!({"outcome": "failed", "name": "y", "reason": "bad"})
        );
    }

    #[test]
    fn case_check_result_round_trips_through_serde() {
        let cases = vec![
            CaseCheckResult::Passed {
                name: "p".into(),
                note: None,
            },
            CaseCheckResult::Passed {
                name: "p".into(),
                note: Some("n".into()),
            },
            CaseCheckResult::Failed {
                name: "f".into(),
                reason: "because".into(),
            },
        ];
        for c in cases {
            let json = serde_json::to_value(&c).unwrap();
            let back: CaseCheckResult = serde_json::from_value(json).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn migrate_legacy_check_rewrites_passed() {
        let mut v = serde_json::json!({"name": "x", "passed": true});
        let rewrote = migrate_legacy_check(&mut v).unwrap();
        assert!(rewrote);
        assert_eq!(v, serde_json::json!({"outcome": "passed", "name": "x"}));
    }

    #[test]
    fn migrate_legacy_check_carries_pass_note() {
        let mut v = serde_json::json!({
            "name": "x",
            "passed": true,
            "detail": "validator skipped"
        });
        let rewrote = migrate_legacy_check(&mut v).unwrap();
        assert!(rewrote);
        assert_eq!(
            v,
            serde_json::json!({
                "outcome": "passed",
                "name": "x",
                "note": "validator skipped",
            })
        );
    }

    #[test]
    fn migrate_legacy_check_rewrites_failed_with_reason() {
        let mut v = serde_json::json!({
            "name": "y",
            "passed": false,
            "detail": "bad"
        });
        let rewrote = migrate_legacy_check(&mut v).unwrap();
        assert!(rewrote);
        assert_eq!(
            v,
            serde_json::json!({"outcome": "failed", "name": "y", "reason": "bad"})
        );
    }

    #[test]
    fn migrate_legacy_check_is_idempotent_on_new_shape() {
        let mut v = serde_json::json!({
            "outcome": "passed",
            "name": "x",
            "note": null
        });
        let before = v.clone();
        let rewrote = migrate_legacy_check(&mut v).unwrap();
        assert!(!rewrote);
        assert_eq!(v, before);
    }

    #[test]
    fn migrate_legacy_check_refuses_failed_without_reason() {
        // The previously-rejected illegal state — surface it loud
        // rather than invent a reason during the migrate.
        let mut v = serde_json::json!({"name": "z", "passed": false});
        let err = migrate_legacy_check(&mut v).unwrap_err();
        assert!(err.to_string().contains("z"));
        assert!(err.to_string().contains("no detail"));
    }

    #[test]
    fn case_check_result_rejects_failed_without_reason() {
        // Now enforced by the type system: there is no `reason`
        // field available to serde when deserializing a `failed`
        // outcome without one. Picking up an old-shape document
        // skips this path entirely (it goes through the migrate).
        let bad = serde_json::json!({"outcome": "failed", "name": "z"});
        let err = serde_json::from_value::<CaseCheckResult>(bad).unwrap_err();
        assert!(err.to_string().contains("reason"));
    }

    #[test]
    fn civil_from_days_anchor_2026_05_20() {
        // 56 years × 365 + 14 leap years + (31+28+31+30+19) day-of-year
        // = 20454 + 139 = 20593 days from 1970-01-01 to 2026-05-20.
        let d = civil_from_days(20_593);
        assert_eq!((d.year, d.month, d.day), (2026, 5, 20));
    }
}
