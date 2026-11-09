//! Defines the `tars eval` flags, hands each subcommand to [`tars_eval::eval`] and shows what it
//! reports. Only `run` and `judge` call models, so only they read the config;
//! [`EvalCommand::calls_models`] says so to the caller.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Subcommand};

use tars_eval::MatchMode;
use tars_llm::RegistryError;
use tars_tools::SandboxMode;
use tars_types::{ProviderId, TenantId};

use crate::Ending;
use crate::config::{self, LoadConfigError};
use crate::home_env::HomeEnvError;
use crate::registry::{PickProviderError, build_registry_with_breakers, pick_provider};

use tars_eval::eval::{
    CheckRates, CheckSet, CheckSetError, CheckSpec, DiffRequest, FileError, JudgeChoice,
    JudgeRequest, JudgeRunError, Quality, RunDiff, RunEvalError, RunRequest, run_diff, run_eval,
    run_judge,
};

mod render;

#[derive(Args, Debug)]
pub struct EvalArgs {
    #[command(subcommand)]
    pub command: EvalCommand,
}

#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// Replay a corpus of cases through a pipeline, writing per-case
    /// outputs + a manifest two runs can be diffed against.
    Run(RunArgs),
    /// Behavioral diff of two eval runs (baseline vs candidate).
    /// Compares operational metrics (errors / tokens / latency) and
    /// per-check violation rates, not raw output text.
    Diff(DiffArgs),
    /// Run an LLM judge over an eval run's outputs (TP/FP/Unsure per
    /// case), writing `judge_report.json` into the run directory. The
    /// judge is a normal tars provider (default `claude_cli`), and one
    /// that is the run's own provider is refused.
    Judge(JudgeArgs),
}

impl EvalCommand {
    /// Returns the subcommand's name on the command line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Run(_) => "run",
            Self::Diff(_) => "diff",
            Self::Judge(_) => "judge",
        }
    }

    /// Returns whether the subcommand makes model calls, the only calls `--sandbox` applies to.
    pub fn calls_models(&self) -> bool {
        match self {
            Self::Run(_) | Self::Judge(_) => true,
            Self::Diff(_) => false,
        }
    }
}

#[derive(Args, Debug)]
pub struct JudgeArgs {
    /// Eval-run directory (contains manifest.json + per-case output.txt).
    pub run: PathBuf,
    /// The tenant the judge calls are made for.
    #[arg(long, value_parser = TenantId::from_str)]
    pub tenant: TenantId,
    /// Provider id to use as the judge.
    #[arg(long, default_value = "claude_cli", value_parser = ProviderId::from_str)]
    pub judge: ProviderId,
    /// Judge model hint. If unset, the provider's default model.
    #[arg(long)]
    pub judge_model: Option<String>,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
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
    /// runs share. No oracle needed for the divergence.
    #[arg(long)]
    pub trajectory: bool,
    /// Similarity mode for the head-to-head divergence; `ordered` when not given.
    #[arg(long = "trajectory-mode", requires = "trajectory", value_parser = match_mode())]
    pub trajectory_mode: Option<MatchMode>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Corpus directory. Each subdirectory is one case; required
    /// `input.txt`, optional `system.txt` and `expected.txt`.
    #[arg(long)]
    pub corpus: PathBuf,

    /// The tenant the run's model calls (and any judge's) are made for.
    #[arg(long, value_parser = TenantId::from_str)]
    pub tenant: TenantId,

    /// Provider id from your config. Required unless the config declares
    /// exactly one provider.
    #[arg(long, value_parser = ProviderId::from_str)]
    pub provider: Option<ProviderId>,

    /// Model hint. If not set, the provider's default is used.
    #[arg(long)]
    pub model: Option<String>,

    /// Output directory, new or empty; a directory with anything in it is refused (exit 2).
    /// Default: `./benchmarks/runs/eval/<timestamp>/`.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,

    /// Per-case max output token bound.
    #[arg(long)]
    pub max_output_tokens: Option<u32>,

    /// Seconds one case may take, its model call and any arg-judge calls together; a case past
    /// it is recorded as failed and the run goes on. Without it a case waits as long as its
    /// calls take.
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    pub case_timeout: Option<u64>,

    /// Built-in invariant checks to run against each output (repeatable).
    /// Recognized: `non-empty`, `valid-json`, `max-length:<N>`, and
    /// `trajectory-match[:<exact|ordered|set|args|args-judge>[:<threshold>]]`, which scores the
    /// tools the model selected against each case's `expected_tools.json`.
    /// Custom invariants are a Rust-API feature.
    #[arg(long = "check", value_parser = CheckSpec::from_str)]
    pub checks: Vec<CheckSpec>,

    /// Judge provider for `trajectory-match:args-judge`: an LLM decides
    /// whether byte-different tool arguments are equivalent. Must differ
    /// from `--provider`.
    #[arg(long, value_parser = ProviderId::from_str)]
    pub judge_provider: Option<ProviderId>,

    /// Judge model hint for `--judge-provider`. Defaults to that provider's
    /// default model.
    #[arg(long)]
    pub judge_model: Option<String>,
}

/// The `--trajectory-mode` of a `--trajectory` that names none.
const DEFAULT_TRAJECTORY_MODE: MatchMode = MatchMode::Ordered;

/// Parses `--trajectory-mode`, offering the modes tars-eval has.
fn match_mode() -> impl clap::builder::TypedValueParser<Value = MatchMode> {
    use clap::builder::TypedValueParser;
    clap::builder::PossibleValuesParser::new(MatchMode::ALL.map(|m| m.as_str()))
        .try_map(|s| MatchMode::parse(&s).ok_or_else(|| format!("unknown mode `{s}`")))
}

/// Why a `tars eval` subcommand did not finish, or what it found wrong.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error(transparent)]
    Config(#[from] LoadConfigError),
    #[error(transparent)]
    Env(#[from] HomeEnvError),
    #[error("building provider registry from config")]
    Registry(#[source] RegistryError),
    #[error(transparent)]
    Provider(#[from] PickProviderError),
    #[error("--judge-model needs --judge-provider")]
    JudgeModelWithoutProvider,
    #[error("--check with --judge-provider")]
    Checks(#[source] CheckSetError),
    #[error("-o {} is not empty: give `tars eval run` a new directory, or empty this one", path.display())]
    OutputNotEmpty { path: PathBuf },
    #[error(transparent)]
    Run(RunEvalError),
    #[error(transparent)]
    Diff(#[from] FileError),
    #[error("writing the diff as JSON")]
    DiffJson(#[source] serde_json::Error),
    #[error(transparent)]
    Judge(#[from] JudgeRunError),
    #[error("{errors} of {cases} cases failed; the run is at {}", manifest.display())]
    CasesFailed {
        errors: usize,
        cases: usize,
        manifest: PathBuf,
    },
    #[error("the candidate regressed: {}", join(regressions))]
    Regressed { regressions: Vec<Regression> },
}

impl EvalError {
    /// Says whether the subcommand finished: a run whose cases failed and a diff that found a
    /// regression did, and what they found is the failure.
    pub(crate) fn ending(&self) -> Ending {
        match self {
            Self::CasesFailed { .. } | Self::Regressed { .. } => Ending::Failed,
            Self::OutputNotEmpty { .. } => Ending::Misused,
            Self::Config(_)
            | Self::Env(_)
            | Self::Registry(_)
            | Self::Provider(_)
            | Self::JudgeModelWithoutProvider
            | Self::Checks(_)
            | Self::Run(_)
            | Self::Diff(_)
            | Self::DiffJson(_)
            | Self::Judge(_) => Ending::NotRun,
        }
    }
}

/// A way the candidate of an `eval diff` did worse than its baseline.
#[derive(Debug, Clone, PartialEq)]
pub enum Regression {
    Errors {
        baseline: usize,
        candidate: usize,
    },
    Violations {
        check: String,
        baseline: f64,
        candidate: f64,
    },
    Precision {
        baseline: f64,
        candidate: f64,
    },
}

impl std::fmt::Display for Regression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Errors {
                baseline,
                candidate,
            } => write!(f, "errors {baseline} → {candidate}"),
            Self::Violations {
                check,
                baseline,
                candidate,
            } => write!(
                f,
                "check {check} violation rate {:.1}% → {:.1}%",
                baseline * 100.0,
                candidate * 100.0
            ),
            Self::Precision {
                baseline,
                candidate,
            } => write!(
                f,
                "judge precision {:.1}% → {:.1}%",
                baseline * 100.0,
                candidate * 100.0
            ),
        }
    }
}

fn join(regressions: &[Regression]) -> String {
    regressions
        .iter()
        .map(Regression::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// A change of a rate smaller than this many percentage points reads as no change, in the diff's
/// text and in its verdict alike.
const UNCHANGED_BELOW_PP: f64 = 0.05;

/// Returns the percentage-point change from rate `a` to rate `b`, or `None` when it reads as no
/// change.
fn moved_pp(a: f64, b: f64) -> Option<f64> {
    let pp = (b - a) * 100.0;
    (pp.abs() >= UNCHANGED_BELOW_PP).then_some(pp)
}

/// Lists every way the candidate did worse than the baseline: more failed cases, a higher
/// violation rate of a check both ran, a lower judge precision. A check with no rate on one side
/// decided no case there, so it has nothing to compare and is no regression.
fn regressions(d: &RunDiff) -> Vec<Regression> {
    let (a, b) = (&d.baseline.manifest, &d.candidate.manifest);
    let errors = (b.error_count() > a.error_count()).then_some(Regression::Errors {
        baseline: a.error_count(),
        candidate: b.error_count(),
    });
    let violations = d.checks.iter().filter_map(|c| match c.rates {
        CheckRates::Both {
            baseline: Some(baseline),
            candidate: Some(candidate),
        } if moved_pp(baseline, candidate).is_some_and(|pp| pp > 0.0) => {
            Some(Regression::Violations {
                check: c.name.clone(),
                baseline,
                candidate,
            })
        }
        _ => None,
    });
    // Two judges' precisions are not comparable: a change between them may be the judges'.
    let precision = match &d.quality {
        Some(Quality::Compared(q)) => Some(q),
        Some(Quality::JudgesDiffer { .. }) | None => None,
    }
    .and_then(|q| {
        let (x, y) = (q.baseline.precision()?, q.candidate.precision()?);
        moved_pp(x, y)
            .is_some_and(|pp| pp < 0.0)
            .then_some(Regression::Precision {
                baseline: x,
                candidate: y,
            })
    });
    errors
        .into_iter()
        .chain(violations)
        .chain(precision)
        .collect()
}

/// Runs one `tars eval` subcommand; `run` and `judge` load the config at `config_path` and
/// make their calls under its sandbox policy, `sandbox` overriding the mode. Progress and
/// summaries go to stderr; `diff` writes its report to stdout.
///
/// # Errors
///
/// Fails when the config or the home's `.env` cannot be loaded, when the subcommand fails, when a run has a failed case, and when a diff finds a regression.
pub async fn execute(
    args: EvalArgs,
    config_path: Option<PathBuf>,
    sandbox: Option<SandboxMode>,
) -> Result<(), EvalError> {
    match args.command {
        EvalCommand::Run(a) => {
            let checks = check_set(a.checks, a.judge_provider, a.judge_model)?;
            let cfg = config::load(config_path)?;
            let run = run_eval(
                RunRequest {
                    registry: build_registry_with_breakers(
                        &cfg,
                        Arc::new(crate::home_env::load()?),
                    )
                    .map_err(EvalError::Registry)?,
                    provider_id: pick_provider(&cfg, a.provider)?,
                    caller: crate::request_context_for(a.tenant, &cfg, sandbox),
                    model: a.model,
                    corpus: a.corpus,
                    output: a.output.unwrap_or_else(default_run_dir),
                    max_output_tokens: a.max_output_tokens,
                    checks,
                    case_deadline: a.case_timeout.map(Duration::from_secs),
                },
                |event| eprint!("{}", render::progress(event)),
            )
            .await
            .map_err(|e| match e {
                RunEvalError::OutputNotEmpty { path } => EvalError::OutputNotEmpty { path },
                e => EvalError::Run(e),
            })?;
            for line in render::run_done(&run) {
                eprintln!("{line}");
            }
            let m = &run.manifest;
            if m.error_count() > 0 {
                return Err(EvalError::CasesFailed {
                    errors: m.error_count(),
                    cases: m.case_count(),
                    manifest: run.manifest_path,
                });
            }
        }
        EvalCommand::Diff(a) => {
            let trajectory = a
                .trajectory
                .then(|| a.trajectory_mode.unwrap_or(DEFAULT_TRAJECTORY_MODE));
            let diff = run_diff(DiffRequest {
                baseline: a.baseline,
                candidate: a.candidate,
                trajectory,
            })?;
            let view = render::DiffView::of(&diff);
            if a.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).map_err(EvalError::DiffJson)?
                );
            } else {
                for line in render::diff_text(&view) {
                    println!("{line}");
                }
            }
            let regressions = regressions(&diff);
            if !regressions.is_empty() {
                return Err(EvalError::Regressed { regressions });
            }
        }
        EvalCommand::Judge(a) => {
            let cfg = config::load(config_path)?;
            let judge = a.judge.clone();
            let judged = run_judge(
                JudgeRequest {
                    registry: build_registry_with_breakers(
                        &cfg,
                        Arc::new(crate::home_env::load()?),
                    )
                    .map_err(EvalError::Registry)?,
                    caller: crate::request_context_for(a.tenant, &cfg, sandbox),
                    run: a.run,
                    judge: a.judge,
                    judge_model: a.judge_model,
                },
                |started| eprintln!("{}", render::judging(started, judge.as_str())),
            )
            .await?;
            for line in render::judged(&judged) {
                eprintln!("{line}");
            }
        }
    }
    Ok(())
}

/// Returns `benchmarks/runs/eval/<UTC time>`, the run directory when `-o` is not given; the time
/// has no colons, which some file systems refuse.
fn default_run_dir() -> PathBuf {
    PathBuf::from("benchmarks/runs/eval")
        .join(chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S").to_string())
}

/// Pairs the `--check`s with `--judge-provider` / `--judge-model`, which only an `args-judge`
/// check takes.
fn check_set(
    checks: Vec<CheckSpec>,
    judge_provider: Option<ProviderId>,
    judge_model: Option<String>,
) -> Result<CheckSet, EvalError> {
    let judge = match (judge_provider, judge_model) {
        (Some(provider), model) => Some(JudgeChoice { provider, model }),
        (None, None) => None,
        (None, Some(_)) => return Err(EvalError::JudgeModelWithoutProvider),
    };
    CheckSet::new(checks, judge).map_err(EvalError::Checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(clap::Parser)]
    struct Line {
        #[command(subcommand)]
        command: EvalCommand,
    }

    fn parse(args: &[&str]) -> EvalCommand {
        use clap::Parser;
        Line::try_parse_from(std::iter::once("eval").chain(args.iter().copied()))
            .unwrap()
            .command
    }

    #[test]
    fn only_run_and_judge_call_models_and_each_name_is_the_one_typed() {
        let cases: [(&[&str], bool); 3] = [
            (&["run", "--corpus", "c", "--tenant", "t"], true),
            (&["judge", "run", "--tenant", "t"], true),
            (&["diff", "a", "b"], false),
        ];
        for (args, calls_models) in cases {
            let command = parse(args);
            assert_eq!(command.calls_models(), calls_models, "{args:?}");
            assert_eq!(command.name(), args[0]);
        }
    }

    #[tokio::test]
    async fn judge_runs_against_a_config_of_several_providers_without_a_provider_flag() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[providers.a]\ntype = \"mock\"\ncanned_response = \"x\"\n\n\
             [providers.b]\ntype = \"mock\"\ncanned_response = \"y\"\n",
        )
        .unwrap();
        let args = EvalArgs {
            command: EvalCommand::Judge(JudgeArgs {
                run: dir.path().join("no-such-run"),
                tenant: TenantId::new("t"),
                judge: ProviderId::new("a"),
                judge_model: None,
            }),
        };

        let err = execute(args, Some(config), None).await.unwrap_err();
        let msg = tars_utils::display_chain(&err);
        assert!(!msg.contains("multiple providers"), "{msg}");
        assert!(msg.contains("no-such-run"), "{msg}");
    }

    #[test]
    fn trajectory_mode_takes_every_mode_tars_eval_has_and_nothing_else() {
        use clap::Parser;
        for mode in MatchMode::ALL {
            let EvalCommand::Diff(a) = parse(&[
                "diff",
                "a",
                "b",
                "--trajectory",
                "--trajectory-mode",
                mode.as_str(),
            ]) else {
                panic!("expected diff");
            };
            assert_eq!(a.trajectory_mode, Some(mode));
        }
        let bogus = [
            "eval",
            "diff",
            "a",
            "b",
            "--trajectory",
            "--trajectory-mode",
            "bogus",
        ];
        assert!(Line::try_parse_from(bogus).is_err());
    }

    #[test]
    fn a_trajectory_mode_without_trajectory_is_a_usage_error() {
        use clap::Parser;
        let alone = ["eval", "diff", "a", "b", "--trajectory-mode", "set"];
        assert!(Line::try_parse_from(alone).is_err());
        let EvalCommand::Diff(a) = parse(&["diff", "a", "b", "--trajectory"]) else {
            panic!("expected diff");
        };
        assert_eq!(a.trajectory_mode, None);
    }

    #[test]
    fn case_timeout_is_whole_seconds_of_at_least_one() {
        use clap::Parser;
        let EvalCommand::Run(a) = parse(&[
            "run",
            "--corpus",
            "c",
            "--tenant",
            "t",
            "--case-timeout",
            "30",
        ]) else {
            panic!("expected run");
        };
        assert_eq!(a.case_timeout, Some(30));
        let zero = [
            "eval",
            "run",
            "--corpus",
            "c",
            "--tenant",
            "t",
            "--case-timeout",
            "0",
        ];
        assert!(Line::try_parse_from(zero).is_err());
    }
}
