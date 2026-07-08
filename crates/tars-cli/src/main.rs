//! `tars` — the TARS Runtime CLI.
//!
//! M1 scope (Doc 14 §7.2 acceptance script): single `run` subcommand
//! that loads config, builds a pipeline, fires one prompt, streams text
//! to stdout, prints token+cost summary at end. Other subcommands
//! (`chat`, `dash`, `task ...`, `config validate/show`, completions)
//! land in M5.
//!
//! Layered cleanly so the actual work lives in modules — `main.rs` is
//! just clap routing + error → exit-code translation. Keeps the
//! testable surface (`run::execute`) free of clap types.

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tars_melt::{TelemetryConfig, TelemetryFormat};

mod bench;
mod config_loader;
mod dispatch;
mod eval;
mod event_store;
mod events;
mod init;
mod model_library;
mod model_query;
mod models;
mod plan;
mod probe;
mod providers_cmd;
mod run;
mod run_report;
mod run_task;
mod trajectory;

#[derive(Parser, Debug)]
#[command(
    name = "tars",
    version,
    about = "TARS Runtime — multi-provider LLM gateway + agent runtime",
    long_about = None,
)]
struct Cli {
    /// Path to the config file. Defaults to `$XDG_CONFIG_HOME/tars/config.toml`
    /// (typically `~/.config/tars/config.toml`).
    #[arg(short, long, global = true, env = "TARS_CONFIG")]
    config: Option<std::path::PathBuf>,

    /// Verbosity. `-v` → info, `-vv` → debug, `-vvv` → trace.
    /// Overridden by `RUST_LOG` if set.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Stderr log format. `pretty` is human-readable (default); `json`
    /// emits one JSON record per event, suitable for piping into a log
    /// aggregator (Datadog / Loki / ELK). Overrides `TARS_LOG_FORMAT`
    /// if both are set.
    #[arg(long, global = true, value_enum, env = "TARS_LOG_FORMAT_FLAG")]
    log_format: Option<LogFormat>,

    /// OS sandbox confinement for the agent's filesystem/bash tools (D5/D6),
    /// codex-consistent. OVERRIDES the `[sandbox].mode` in the TOML config when
    /// present. Absent here AND in the config ⇒ `danger-full-access` (today's
    /// unconfined behaviour) — confinement is strictly opt-in.
    ///
    ///   - `read-only`         — no writes anywhere (reviewer agents)
    ///   - `workspace-write`   — write only the worktree (the fixer default)
    ///   - `danger-full-access`— no confinement (explicit escape hatch)
    #[arg(long, global = true, value_enum)]
    sandbox: Option<SandboxModeArg>,

    #[command(subcommand)]
    command: Command,
}

/// CLI mirror of [`tars_tools::SandboxMode`]. clap renders the variants
/// kebab-case (`read-only` / `workspace-write` / `danger-full-access`), matching
/// codex + the `[sandbox].mode` TOML values.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum SandboxModeArg {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl From<SandboxModeArg> for tars_tools::SandboxMode {
    fn from(m: SandboxModeArg) -> Self {
        match m {
            SandboxModeArg::ReadOnly => tars_tools::SandboxMode::ReadOnly,
            SandboxModeArg::WorkspaceWrite => tars_tools::SandboxMode::WorkspaceWrite,
            SandboxModeArg::DangerFullAccess => tars_tools::SandboxMode::DangerFullAccess,
        }
    }
}

/// Local mirror of [`tars_melt::TelemetryFormat`]. Kept here (not on
/// the `tars-melt` enum itself) so the observability crate doesn't pick
/// up a `clap` dependency just to satisfy a CLI flag.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

impl From<LogFormat> for TelemetryFormat {
    fn from(f: LogFormat) -> Self {
        match f {
            LogFormat::Pretty => TelemetryFormat::Pretty,
            LogFormat::Json => TelemetryFormat::Json,
        }
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Send a single prompt to a provider and stream the response.
    Run(run::RunArgs),
    /// Drive an OrchestratorAgent: turn a goal into a typed Plan.
    Plan(plan::PlanArgs),
    /// Drive the multi-step Orchestrator → Worker → Critic loop end-to-end.
    RunTask(run_task::RunTaskArgs),
    /// Sanity-check a CLI provider (`claude_cli` / `gemini_cli` / `codex_cli`) — sends
    /// a fixed "say hi" prompt and dumps every event so you can see what the
    /// subprocess actually returns.
    Probe(probe::ProbeArgs),
    /// Benchmark a provider — N iterations, reports TTFB / total / decode tok/s
    /// as mean / p50 / p99. Useful for comparing local model throughput.
    Bench(bench::BenchArgs),
    /// Inspect the trajectory event log written by `tars run` / `tars plan` / `tars run-task`.
    Trajectory(trajectory::TrajectoryArgs),
    /// Aggregate one trajectory's events into a per-run summary
    /// (status, wall clock, llm calls, token totals, per-agent /
    /// per-provider breakdown, errors). See
    /// `docs/eval-and-arc-llm-roadmap.md §1.1`.
    RunReport(run_report::RunReportArgs),
    /// Eval subcommands: corpus replay, judge, and diff (all shipped).
    /// See `docs/eval-and-arc-llm-roadmap.md §1.3`.
    #[command(subcommand_value_name = "COMMAND")]
    Eval(eval::EvalArgs),
    /// Discover provider models over a persisted model library.
    /// `tars models` reads the library (fast/offline); `tars models update`
    /// refreshes it from the live provider APIs and flags stale defaults.
    Models(models::ModelsArgs),
    /// List configured providers with key-env health and optional
    /// (`--check`) reachability probing.
    Providers(providers_cmd::ProvidersArgs),
    /// Bootstrap a starter user-level config at `~/.tars/config.toml`.
    /// Idempotent (`--force` to overwrite). New users run this first.
    Init(init::InitArgs),
    /// Inspect the **pipeline event store** (LLM call records written
    /// by `EventEmitterMiddleware`). Distinct from `tars trajectory`,
    /// which reads agent-decision events.
    #[command(subcommand_value_name = "COMMAND")]
    Events(events::EventsArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Telemetry init goes through tars-melt so every binary in the
    // workspace lands the same formatter / env-filter / span shape.
    // The guard is bound to `_telemetry` so `Drop` runs at process
    // exit (M1 no-op; M5 will flush the OTel exporter).
    //
    // Use the fallible `init()` (not `init_or_warn`) so that if the
    // subscriber fails to install, the user sees it — otherwise
    // `-v/-vv/-vvv` would silently have no effect.
    let mut tcfg = TelemetryConfig::from_verbosity(cli.verbose);
    tcfg.service = "tars-cli".into();
    // `--log-format` (or the matching env var) wins over the
    // `TARS_LOG_FORMAT` consulted by `from_verbosity`. Spelled out as
    // override-after-construct so the precedence is obvious to readers.
    if let Some(f) = cli.log_format {
        tcfg.format = f.into();
    }
    let _telemetry = match tars_melt::init(tcfg) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!(
                "error: failed to initialize telemetry: {e}\n\
                 verbose flags (-v/-vv/-vvv) will not take effect"
            );
            return ExitCode::from(1);
        }
    };

    let cmd_name = match &cli.command {
        Command::Run(_) => "run",
        Command::Plan(_) => "plan",
        Command::RunTask(_) => "run-task",
        Command::Probe(_) => "probe",
        Command::Bench(_) => "bench",
        Command::Trajectory(_) => "trajectory",
        Command::RunReport(_) => "run-report",
        Command::Eval(_) => "eval",
        Command::Models(_) => "models",
        Command::Providers(_) => "providers",
        Command::Init(_) => "init",
        Command::Events(_) => "events",
    };

    let result: Result<()> = match cli.command {
        Command::Run(args) => run::execute(args, cli.config).await,
        Command::Plan(args) => plan::execute(args, cli.config).await,
        Command::RunTask(args) => {
            run_task::execute(args, cli.config, cli.sandbox.map(Into::into)).await
        }
        Command::Probe(args) => probe::execute(args, cli.config).await,
        Command::Bench(args) => bench::execute(args, cli.config).await,
        Command::Trajectory(args) => {
            // `--config` is global on the parser for ergonomics, but
            // trajectory operates only on the event-store sqlite file
            // (see `--events-path`). Surface that mismatch instead of
            // silently ignoring the flag.
            if cli.config.is_some() {
                tracing::warn!(
                    "--config is ignored by `tars trajectory` \
                     (use --events-path / TARS_EVENTS_PATH instead)"
                );
            }
            trajectory::execute(args).await
        }
        Command::RunReport(args) => {
            if cli.config.is_some() {
                tracing::warn!(
                    "--config is ignored by `tars run-report` \
                     (use --events-path / TARS_EVENTS_PATH instead)"
                );
            }
            run_report::execute(args).await
        }
        Command::Eval(args) => eval::execute(args, cli.config).await,
        Command::Models(args) => models::execute(args, cli.config).await,
        Command::Providers(args) => providers_cmd::execute(args, cli.config).await,
        Command::Init(args) => {
            // `--config` is a global flag for ergonomics on other
            // subcommands; `init` writes its own target so it never
            // reads the global one. `--path` on InitArgs is the
            // subcommand-local override.
            if cli.config.is_some() {
                tracing::warn!(
                    "--config is ignored by `tars init` (use --path to redirect output)"
                );
            }
            init::execute(args).await
        }
        Command::Events(args) => {
            // `--config` is unused — `events` operates on the pipeline
            // event store sqlite file (see `--store-dir` /
            // TARS_EVENT_STORE_DIR), not on a TARS config.
            if cli.config.is_some() {
                tracing::warn!(
                    "--config is ignored by `tars events` \
                     (use --store-dir / TARS_EVENT_STORE_DIR instead)"
                );
            }
            events::execute(args).await
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // anyhow's Debug renders the chain (caused by ...).
            eprintln!("error in `tars {cmd_name}`: {e:?}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_maps_one_to_one_to_telemetry_format() {
        assert!(matches!(
            TelemetryFormat::from(LogFormat::Pretty),
            TelemetryFormat::Pretty
        ));
        assert!(matches!(
            TelemetryFormat::from(LogFormat::Json),
            TelemetryFormat::Json
        ));
    }

    #[test]
    fn cli_parses_log_format_flag() {
        // Sanity: clap accepts the documented values without ambiguity.
        let cli =
            Cli::try_parse_from(["tars", "--log-format", "json", "events", "list"]).expect("parse");
        assert!(matches!(cli.log_format, Some(LogFormat::Json)));

        let cli = Cli::try_parse_from(["tars", "--log-format", "pretty", "events", "list"])
            .expect("parse");
        assert!(matches!(cli.log_format, Some(LogFormat::Pretty)));

        // Omitted → None → fall through to TARS_LOG_FORMAT / Pretty.
        let cli = Cli::try_parse_from(["tars", "events", "list"]).expect("parse");
        assert!(cli.log_format.is_none());

        // Unknown value → clap rejects (we never silently default).
        let err = Cli::try_parse_from(["tars", "--log-format", "yaml", "events", "list"]);
        assert!(err.is_err(), "clap must reject unsupported values");
    }
}
