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
use clap::{Parser, Subcommand};
use tars_melt::TelemetryConfig;

mod bench;
mod config_loader;
mod dispatch;
mod event_store;
mod init;
mod plan;
mod probe;
mod run;
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

    #[command(subcommand)]
    command: Command,
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
    /// Bootstrap a starter user-level config at `~/.tars/config.toml`.
    /// Idempotent (`--force` to overwrite). New users run this first.
    Init(init::InitArgs),
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
        Command::Init(_) => "init",
    };

    let result: Result<()> = match cli.command {
        Command::Run(args) => run::execute(args, cli.config).await,
        Command::Plan(args) => plan::execute(args, cli.config).await,
        Command::RunTask(args) => run_task::execute(args, cli.config).await,
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
