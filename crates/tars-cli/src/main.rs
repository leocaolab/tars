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
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Telemetry init goes through tars-melt so every binary in the
    // workspace lands the same formatter / env-filter / span shape.
    // The guard is bound to `_telemetry` so `Drop` runs at process
    // exit (M1 no-op; M5 will flush the OTel exporter).
    let mut config = TelemetryConfig::from_verbosity(cli.verbose);
    config.service = "tars-cli".into();
    let _telemetry = tars_melt::init_or_warn(config);

    let result: Result<()> = match cli.command {
        Command::Run(args) => run::execute(args, cli.config).await,
        Command::Plan(args) => plan::execute(args, cli.config).await,
        Command::RunTask(args) => run_task::execute(args, cli.config).await,
        Command::Probe(args) => probe::execute(args, cli.config).await,
        Command::Bench(args) => bench::execute(args, cli.config).await,
        Command::Trajectory(args) => trajectory::execute(args).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // anyhow's Debug renders the chain (caused by ...).
            eprintln!("error: {e:?}");
            ExitCode::from(1)
        }
    }
}
