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

mod config_loader;
mod run;

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
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result: Result<()> = match cli.command {
        Command::Run(args) => run::execute(args, cli.config).await,
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

/// Wire up `tracing` so middleware events go to stderr (stdout stays
/// clean for the LLM response). `RUST_LOG` wins; otherwise `--verbose`
/// drives the level. Default = WARN so a casual `tars run` is silent
/// on the diagnostics side.
fn init_tracing(verbose: u8) {
    use tracing_subscriber::filter::EnvFilter;
    let default_level = match verbose {
        0 => "warn",
        1 => "tars=info,warn",
        2 => "tars=debug,info",
        _ => "tars=trace,debug",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
