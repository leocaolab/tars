//! `tars eval` — the CLI surface for the corpus replay + judge + diff
//! engine. The engine itself lives in `tars_eval::eval` (stage 2 of
//! the harness split); this module is only the clap arg definitions plus
//! a thin dispatch that resolves config → registry → provider and maps
//! the parsed flags onto the engine's plain config structs.
//!
//! See `docs/eval-and-arc-llm-roadmap.md §1.3` and `tars_eval::eval`
//! for the design intent + corpus/output layout.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use tars_eval::eval::{
    EvalBlessConfig, EvalDiffConfig, EvalJudgeConfig, EvalRunConfig, run_bless, run_diff,
    run_eval, run_judge, run_migrate_checks,
};

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

pub async fn execute(args: EvalArgs, config_path: Option<PathBuf>) -> Result<()> {
    match args.command {
        EvalCommand::Run(a) => {
            // Resolve config → registry → provider (same path `tars run` uses),
            // then hand the engine plain values.
            let cfg = config_loader::load(config_path)?;
            let registry = build_registry_with_breaker(&cfg, /* breaker */ true)?;
            let provider_id = pick_provider(&cfg, a.provider.as_deref())?;
            run_eval(EvalRunConfig {
                registry,
                provider_id,
                model: a.model,
                corpus: a.corpus,
                output: a.output,
                max_output_tokens: a.max_output_tokens,
                checks: a.checks,
                agent: a.agent,
                tools: a.tools,
                agent_max_iterations: a.agent_max_iterations,
                judge_provider: a.judge_provider,
                judge_model: a.judge_model,
            })
            .await
        }
        EvalCommand::Diff(a) => run_diff(EvalDiffConfig {
            baseline: a.baseline,
            candidate: a.candidate,
            json: a.json,
            trajectory: a.trajectory,
            trajectory_mode: a.trajectory_mode,
        }),
        EvalCommand::MigrateChecks(a) => run_migrate_checks(&a.dir),
        EvalCommand::Judge(a) => {
            let cfg = config_loader::load(config_path)?;
            let registry = build_registry_with_breaker(&cfg, /* breaker */ true)?;
            run_judge(EvalJudgeConfig {
                registry,
                run: a.run,
                judge: a.judge,
                judge_model: a.judge_model,
            })
            .await
        }
        EvalCommand::Bless(a) => run_bless(EvalBlessConfig {
            run: a.run,
            select: a.select,
            accept: a.accept,
        }),
    }
}
