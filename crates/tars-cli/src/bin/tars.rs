use std::process::ExitCode;

use clap::Parser;
use tars_cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    tars_cli::run(Cli::parse()).await
}
