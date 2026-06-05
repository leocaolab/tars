//! `tars-server` — personal-mode HTTP server over the tars pipeline.
//!
//! ```text
//! tars-server                       # ~/.tars/config.toml, 127.0.0.1:8787
//! tars-server --port 9000
//! tars-server --config ./tars.toml --host 127.0.0.1 --port 8787
//! ```
//!
//! No auth — binds loopback by default and refuses to start on a
//! non-loopback address unless `--insecure-allow-remote` is passed.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "tars-server",
    about = "Personal-mode HTTP server over the tars pipeline"
)]
struct Args {
    /// Config file. Defaults to `~/.tars/config.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Address to bind. Loopback only unless `--insecure-allow-remote`.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,

    /// Port to bind.
    #[arg(long, default_value_t = 8787)]
    port: u16,

    /// Provider used when a request omits `provider`. Without it, a
    /// request must name its provider (config always includes the
    /// built-in provider table, so there's rarely just one).
    #[arg(long)]
    default_provider: Option<String>,

    /// Permit binding a non-loopback address. There is NO auth, so the
    /// model becomes reachable by anyone who can route to the socket —
    /// only do this behind your own trusted network boundary.
    #[arg(long)]
    insecure_allow_remote: bool,

    /// `-v` info, `-vv` debug, `-vvv` trace. `RUST_LOG` overrides.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _telemetry =
        tars_melt::init_or_warn(tars_melt::TelemetryConfig::from_verbosity(args.verbose));

    if !args.host.is_loopback() && !args.insecure_allow_remote {
        anyhow::bail!(
            "refusing to bind non-loopback address {} without auth — tars-server has no \
             authentication; pass --insecure-allow-remote only behind a trusted boundary",
            args.host,
        );
    }

    let config_path = match args.config {
        Some(p) => p,
        None => tars_config::default_config_path()
            .context("could not resolve ~/.tars/config.toml; pass --config")?,
    };
    let config = tars_config::ConfigManager::load_from_file(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let state = tars_server::AppState::from_config(&config, args.default_provider)?;
    let app = tars_server::router(state);

    let addr = SocketAddr::new(args.host, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let bound = listener.local_addr().unwrap_or(addr);

    if !args.host.is_loopback() {
        tracing::warn!(%bound, "tars-server bound a NON-LOOPBACK address with NO auth");
    }
    tracing::info!(%bound, config = %config_path.display(), "tars-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

/// Resolve on Ctrl-C so the runtime drains in-flight requests.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received; draining");
}
