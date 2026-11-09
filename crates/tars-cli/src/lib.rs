//! The `tars` command line. It parses arguments, starts telemetry, hands each command to its
//! module, and turns how the command ended into its exit code: 0 passed, 1 not run, 2 a usage
//! error, 3 ran and found a failure, 128 plus the signal's number when SIGINT or SIGTERM stopped
//! it.
mod bench;
mod config;
mod eval;
mod events;
mod exit_signal;
mod home_env;
mod init;
mod live_models;
mod models;
mod probe;
mod providers;
mod registry;
mod single_attempt;
mod transcript;

use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use exit_signal::{ExitSignal, ExitSignals};
use tars_config::{Config, resolve_policy};
use tars_telemetry::{EnvConfig, TelemetryConfig, TelemetryFormat};
use tars_tools::SandboxMode;
use tars_types::{Caller, TenantId, TraceId};
use tars_utils::display_chain;
use tracing::Instrument as _;

/// The exit code of a command line clap parses but a command refuses, the same code clap gives
/// one it refuses itself.
const USAGE_ERROR: u8 = 2;

/// Says how one `tars` command ended, the fact its exit code reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The command did its work and found nothing wrong.
    Passed,
    /// The command could not do its work: a bad config, an unreadable file, a failed call.
    NotRun,
    /// The command refused what it was asked to do, as clap refuses a command line it cannot
    /// parse.
    Misused,
    /// The command did its work, and what it found is a failure: failed eval cases, a
    /// regression, providers that did not answer, a bench that measured no token.
    Failed,
    /// A signal stopped the command before it ended by itself.
    Stopped(ExitSignal),
}

/// Returns the exit code that reports `ending`.
fn exit_code(ending: Ending) -> u8 {
    match ending {
        Ending::Passed => 0,
        Ending::NotRun => 1,
        Ending::Misused => USAGE_ERROR,
        Ending::Failed => 3,
        // A shell reports a process a signal ended as 128 plus the signal's number.
        Ending::Stopped(signal) => 128 + signal.number(),
    }
}

/// Why a `tars` command did not pass.
#[derive(Debug, thiserror::Error)]
enum CommandError {
    #[error(transparent)]
    Probe(#[from] probe::ProbeError),
    #[error(transparent)]
    Bench(#[from] bench::BenchError),
    #[error(transparent)]
    Eval(#[from] eval::EvalError),
    #[error(transparent)]
    Models(#[from] models::ModelsError),
    #[error(transparent)]
    Providers(#[from] providers::ProvidersError),
    #[error(transparent)]
    Init(#[from] init::InitError),
    #[error(transparent)]
    Events(#[from] events::EventsError),
}

impl CommandError {
    /// Says whether the command did its work before it failed.
    fn ending(&self) -> Ending {
        match self {
            Self::Probe(e) => e.ending(),
            Self::Bench(e) => e.ending(),
            Self::Eval(e) => e.ending(),
            Self::Models(e) => e.ending(),
            Self::Providers(e) => e.ending(),
            Self::Init(_) | Self::Events(_) => Ending::NotRun,
        }
    }
}

/// Returns the context one CLI invocation makes its model calls under: the tenant the operator
/// named, a fresh trace shared by all of the invocation's calls, and the sandbox policy of the
/// config's `[sandbox]` table with `--sandbox` overriding its mode.
pub(crate) fn request_context_for(
    tenant: TenantId,
    cfg: &Config,
    sandbox: Option<SandboxMode>,
) -> Caller {
    let trace = TraceId::from(uuid::Uuid::new_v4());
    Caller::new(tenant, trace).with_sandbox(resolve_policy(cfg.sandbox(), sandbox))
}

#[derive(Parser, Debug)]
#[command(
    name = "tars",
    version,
    about = "TARS Runtime — multi-provider LLM gateway",
    long_about = None,
)]
/// The `tars` command line as clap parses it.
pub struct Cli {
    /// Path to the config file. Defaults to `config.toml` in `$TARS_HOME`, else in `~/.tars`, the
    /// file `tars init` writes.
    #[arg(short, long, global = true, env = "TARS_CONFIG")]
    config: Option<std::path::PathBuf>,

    /// Verbosity. `-v` → info, `-vv` → debug, `-vvv` → trace.
    /// A valid, non-blank `RUST_LOG` overrides it.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Stderr log format. `pretty` is human-readable (default); `json`
    /// emits one JSON record per event. Overrides `TARS_LOG_FORMAT`
    /// if both are set.
    #[arg(long, global = true, value_enum, env = "TARS_LOG_FORMAT_FLAG")]
    log_format: Option<LogFormatArg>,

    /// Sandbox mode for the model calls of `probe`, `bench` and `eval run` / `judge`, which
    /// CLI-delegate providers (claude, codex, opencode, antigravity) run under. It overrides the
    /// mode of the config's `[sandbox]` table and keeps its `network` and `writable_roots`; with
    /// neither, calls run read-only.
    ///
    ///   - `read-only`          — no writes anywhere
    ///   - `workspace-write`    — writes only under the writable roots
    ///   - `danger-full-access` — no confinement; a CLI delegate still runs in a workspace-write
    ///     jail rooted at its working directory
    #[arg(long, global = true, value_enum)]
    sandbox: Option<SandboxModeArg>,

    #[command(subcommand)]
    command: Command,
}

/// Spells [`tars_tools::SandboxMode`] as a flag; clap renders the variants kebab-case, the same
/// words as the `[sandbox].mode` TOML values.
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

/// Spells [`tars_telemetry::TelemetryFormat`] as a flag, so tars-telemetry does not depend on clap.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormatArg {
    Pretty,
    Json,
}

impl From<LogFormatArg> for TelemetryFormat {
    fn from(f: LogFormatArg) -> Self {
        match f {
            LogFormatArg::Pretty => TelemetryFormat::Pretty,
            LogFormatArg::Json => TelemetryFormat::Json,
        }
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Sanity-check a provider: send one short prompt and print every event
    /// it streams back.
    Probe(probe::ProbeArgs),
    /// Benchmark a provider — N iterations, reports TTFB / total / decode tok/s
    /// as mean / p50 / p99. Useful for comparing local model throughput.
    Bench(bench::BenchArgs),
    /// Testing + scoring: corpus replay, judge, diff.
    #[command(subcommand_value_name = "COMMAND")]
    Eval(eval::EvalArgs),
    /// Discover provider models over a persisted model library.
    /// `tars models` reads the library (fast/offline); `tars models update`
    /// refreshes it from the live provider APIs and flags stale defaults.
    Models(models::ModelsArgs),
    /// List configured providers with key-env health and optional
    /// (`--check`) reachability probing.
    Providers(providers::ProvidersArgs),
    /// Bootstrap a starter user-level config at the default config path.
    /// Refuses to overwrite an existing file unless `--force` is given.
    Init(init::InitArgs),
    /// Inspect the call log the pipeline writes: list calls, show one.
    #[command(subcommand_value_name = "COMMAND")]
    Events(events::EventsArgs),
}

/// Runs the command `cli` names and returns the exit code that reports how it ended.
pub async fn run(cli: Cli) -> ExitCode {
    let EnvConfig {
        config: tcfg,
        warnings,
    } = TelemetryConfig::from_env("tars-cli", cli.verbose, cli.log_format.map(Into::into));
    // A warning goes straight to stderr: through the subscriber, the very `RUST_LOG` it may be
    // about could filter it out.
    for warning in &warnings {
        eprintln!("warning: {}", display_chain(warning));
    }
    // A subscriber that fails to install is fatal here, not a warning: `-v` would otherwise
    // silently do nothing. The guard is bound to a name rather than `_`, so it lives until `main`
    // returns.
    let telemetry = match tars_telemetry::init(tcfg) {
        Ok(installed) => {
            for warning in &installed.warnings {
                eprintln!("warning: {}", display_chain(warning));
            }
            installed.guard
        }
        Err(e) => {
            eprintln!(
                "error: failed to initialize telemetry: {}\n\
                 verbose flags (-v/-vv/-vvv) will not take effect",
                display_chain(&e)
            );
            return ExitCode::from(exit_code(Ending::NotRun));
        }
    };

    let cmd_name = match &cli.command {
        Command::Probe(_) => "probe".to_string(),
        Command::Bench(_) => "bench".to_string(),
        Command::Eval(e) => format!("eval {}", e.command.name()),
        Command::Models(_) => "models".to_string(),
        Command::Providers(_) => "providers".to_string(),
        Command::Init(_) => "init".to_string(),
        Command::Events(_) => "events".to_string(),
    };

    // Declared after the guard, the root span closes before the guard flushes the exporters.
    // Without trace export it would only prefix every log line, so none is opened.
    let root = if telemetry.exports_traces() {
        command_span(&cmd_name)
    } else {
        tracing::Span::none()
    };
    let mut signals = match ExitSignals::listen() {
        Ok(signals) => signals,
        Err(e) => {
            eprintln!("error: failed to listen for SIGINT and SIGTERM: {e}");
            return ExitCode::from(exit_code(Ending::NotRun));
        }
    };
    let command = execute(cli.command, cli.config, cli.sandbox, &cmd_name).instrument(root.clone());
    let ending = tokio::select! {
        ending = command => ending,
        signal = signals.recv() => {
            // Dropping the command kills at most the children it still holds, not the processes
            // they started, nor a daemon the provider keeps past the call.
            tars_llm::kill_all_spawned();
            Ending::Stopped(signal)
        }
    };
    let code = exit_code(ending);
    root.record("process.exit.code", i64::from(code));
    ExitCode::from(code)
}

/// Returns the span one run of `tars <command>` happens in. Opened outside any span, it is the
/// root of the run's trace, and every call the command makes hangs under it.
fn command_span(command: &str) -> tracing::Span {
    tracing::info_span!(
        "tars.command",
        otel.name = format!("tars {command}").as_str(),
        process.executable.name = "tars",
        tars.command = command,
        process.exit.code = tracing::field::Empty,
    )
}

/// Runs `command` and returns how it ended.
async fn execute(
    command: Command,
    config: Option<std::path::PathBuf>,
    sandbox: Option<SandboxModeArg>,
    cmd_name: &str,
) -> Ending {
    let sandbox: Option<tars_tools::SandboxMode> = sandbox.map(Into::into);
    if sandbox.is_some() && !calls_models(&command) {
        tracing::warn!("--sandbox is ignored by `tars {cmd_name}`, which calls no model");
    }

    let result: Result<(), CommandError> = match command {
        Command::Probe(args) => probe::execute(args, config, sandbox)
            .await
            .map_err(Into::into),
        Command::Bench(args) => bench::execute(args, config, sandbox)
            .await
            .map_err(Into::into),
        Command::Eval(args) => eval::execute(args, config, sandbox)
            .await
            .map_err(Into::into),
        Command::Models(args) => match args.command() {
            Ok(command) => models::execute(command, config).await.map_err(Into::into),
            Err(misuse) => {
                eprint!(
                    "{}",
                    Cli::command().error(ErrorKind::ArgumentConflict, misuse)
                );
                return Ending::Misused;
            }
        },
        Command::Providers(args) => providers::execute(args, config).await.map_err(Into::into),
        Command::Init(args) => {
            if config.is_some() {
                tracing::warn!(
                    "--config is ignored by `tars init` (use --path to redirect output)"
                );
            }
            init::execute(args).await.map_err(Into::into)
        }
        Command::Events(args) => {
            if config.is_some() {
                tracing::warn!(
                    "--config is ignored by `tars events` \
                     (use --store-dir / TARS_EVENT_STORE_DIR instead)"
                );
            }
            events::execute(args).await.map_err(Into::into)
        }
    };

    match &result {
        Ok(()) => Ending::Passed,
        Err(e) => {
            eprintln!("error in `tars {cmd_name}`: {}", display_chain(e));
            e.ending()
        }
    }
}

/// Returns whether `command` makes model calls, the only calls `--sandbox` applies to.
fn calls_models(command: &Command) -> bool {
    match command {
        Command::Probe(_) | Command::Bench(_) => true,
        Command::Eval(e) => e.command.calls_models(),
        Command::Models(_) | Command::Providers(_) | Command::Init(_) | Command::Events(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_ending_has_its_own_exit_code() {
        assert_eq!(exit_code(Ending::Passed), 0);
        assert_eq!(exit_code(Ending::NotRun), 1);
        assert_eq!(exit_code(Ending::Misused), 2);
        assert_eq!(exit_code(Ending::Failed), 3);
    }

    #[test]
    fn a_stopping_signal_exits_with_128_plus_its_number() {
        assert_eq!(exit_code(Ending::Stopped(ExitSignal::Interrupt)), 130);
        assert_eq!(exit_code(Ending::Stopped(ExitSignal::Terminate)), 143);
    }

    #[test]
    fn without_a_sandbox_table_calls_run_read_only_unless_the_flag_says_otherwise() {
        let cfg = Config::from_toml("[providers]\n").unwrap();
        let tenant = TenantId::new("t");
        assert_eq!(
            request_context_for(tenant.clone(), &cfg, None)
                .sandbox
                .mode(),
            SandboxMode::ReadOnly
        );
        assert_eq!(
            request_context_for(tenant, &cfg, Some(SandboxMode::WorkspaceWrite))
                .sandbox
                .mode(),
            SandboxMode::WorkspaceWrite
        );
    }

    #[test]
    fn the_configs_sandbox_table_reaches_the_calls() {
        let cfg = Config::from_toml("[sandbox]\nmode = \"read-only\"\n").unwrap();
        assert_eq!(
            request_context_for(TenantId::new("t"), &cfg, None)
                .sandbox
                .mode(),
            SandboxMode::ReadOnly
        );
    }

    /// Makes two model calls through the default pipeline, as a command that calls twice does.
    async fn two_calls() {
        for _ in 0..2 {
            let provider = tars_llm::MockProvider::new(
                tars_types::ProviderId::new("p"),
                tars_llm::CannedResponse::text("ok"),
            );
            let svc = tars_llm::LlmService::default_chain(
                provider,
                "m",
                tars_llm::ChainOptions::default(),
            );
            let ctx = Caller::new(TenantId::new("t"), TraceId::from(uuid::Uuid::new_v4()));
            tars_llm::complete_async(svc, tars_llm::ChatRequest::user("x"), ctx)
                .await
                .expect("the mock answers");
        }
    }

    /// Runs the future `run` makes under an OTel trace layer and returns the spans it exported.
    /// The future is made under the layer too, so a span it opens up front is recorded.
    async fn exported_spans<F: std::future::Future<Output = ()>>(
        run: impl FnOnce() -> F,
    ) -> Vec<opentelemetry_sdk::export::trace::SpanData> {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::prelude::*;

        let exporter = opentelemetry_sdk::testing::trace::InMemorySpanExporter::default();
        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        {
            let _default = tracing::subscriber::set_default(subscriber);
            run().await;
        }
        exporter.get_finished_spans().expect("in-memory export")
    }

    fn call_spans(
        spans: &[opentelemetry_sdk::export::trace::SpanData],
    ) -> Vec<&opentelemetry_sdk::export::trace::SpanData> {
        spans
            .iter()
            .filter(|s| s.name.starts_with("chat "))
            .collect()
    }

    #[tokio::test]
    async fn the_calls_of_one_run_share_its_trace_and_hang_under_its_root_span() {
        let spans = exported_spans(|| two_calls().instrument(command_span("eval run"))).await;

        let root = spans
            .iter()
            .find(|s| s.name == "tars eval run")
            .expect("no root span");
        assert_eq!(root.parent_span_id, opentelemetry::trace::SpanId::INVALID);
        let calls = call_spans(&spans);
        assert_eq!(calls.len(), 2, "{spans:?}");
        for call in calls {
            assert_eq!(call.span_context.trace_id(), root.span_context.trace_id());
            assert_eq!(call.parent_span_id, root.span_context.span_id());
        }
    }

    #[tokio::test]
    async fn calls_made_outside_any_span_are_two_traces() {
        let spans = exported_spans(two_calls).await;

        let calls = call_spans(&spans);
        assert_eq!(calls.len(), 2, "{spans:?}");
        assert_ne!(
            calls[0].span_context.trace_id(),
            calls[1].span_context.trace_id()
        );
    }

    #[test]
    fn log_format_maps_one_to_one_to_telemetry_format() {
        assert!(matches!(
            TelemetryFormat::from(LogFormatArg::Pretty),
            TelemetryFormat::Pretty
        ));
        assert!(matches!(
            TelemetryFormat::from(LogFormatArg::Json),
            TelemetryFormat::Json
        ));
    }

    #[test]
    fn cli_parses_log_format_flag() {
        let cli = Cli::try_parse_from([
            "tars",
            "--log-format",
            "json",
            "events",
            "list",
            "--tenant",
            "t",
        ])
        .expect("parse");
        assert!(matches!(cli.log_format, Some(LogFormatArg::Json)));

        let cli = Cli::try_parse_from([
            "tars",
            "--log-format",
            "pretty",
            "events",
            "list",
            "--tenant",
            "t",
        ])
        .expect("parse");
        assert!(matches!(cli.log_format, Some(LogFormatArg::Pretty)));

        let cli = Cli::try_parse_from(["tars", "events", "list", "--tenant", "t"]).expect("parse");
        assert!(cli.log_format.is_none());

        let err = Cli::try_parse_from([
            "tars",
            "--log-format",
            "yaml",
            "events",
            "list",
            "--tenant",
            "t",
        ]);
        assert!(err.is_err(), "clap must reject unsupported values");
    }

    #[test]
    fn model_calling_commands_refuse_to_run_without_a_tenant() {
        let without_tenant: [&[&str]; 4] = [
            &["tars", "probe", "p"],
            &["tars", "bench", "p"],
            &["tars", "eval", "run", "--corpus", "c"],
            &["tars", "eval", "judge", "run"],
        ];
        for argv in without_tenant {
            let Err(err) = Cli::try_parse_from(argv) else {
                panic!("{argv:?} parsed without a tenant");
            };
            assert!(err.to_string().contains("--tenant"), "{argv:?}: {err}");
        }
    }

    #[test]
    fn model_calling_commands_take_the_tenant_they_are_given() {
        let with_tenant: [&[&str]; 4] = [
            &["tars", "probe", "p", "--tenant", "acme"],
            &["tars", "bench", "p", "--tenant", "acme"],
            &["tars", "eval", "run", "--corpus", "c", "--tenant", "acme"],
            &["tars", "eval", "judge", "run", "--tenant", "acme"],
        ];
        for argv in with_tenant {
            let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            let tenant = match cli.command {
                Command::Probe(a) => a.tenant,
                Command::Bench(a) => a.tenant,
                Command::Eval(e) => match e.command {
                    eval::EvalCommand::Run(a) => a.tenant,
                    eval::EvalCommand::Judge(a) => a.tenant,
                    other => panic!("{argv:?} parsed as {other:?}"),
                },
                other => panic!("{argv:?} parsed as {other:?}"),
            };
            assert_eq!(tenant.as_str(), "acme", "{argv:?}");
        }
    }

    #[test]
    fn a_usage_error_exits_2_before_any_command_runs() {
        let err = Cli::try_parse_from(["tars", "bench", "p", "--tenant", "t", "--repeat", "0"])
            .expect_err("a bench of no iteration measures nothing");
        assert_eq!(err.exit_code(), 2);
        assert!(
            Cli::try_parse_from(["tars", "bench", "p", "--tenant", "t", "--repeat", "1"]).is_ok()
        );
    }

    #[test]
    fn the_sandbox_flag_parses_after_the_subcommand_and_reaches_only_model_calls() {
        let cli = Cli::try_parse_from([
            "tars",
            "probe",
            "p",
            "--tenant",
            "t",
            "--sandbox",
            "read-only",
        ])
        .expect("parse");
        assert!(matches!(cli.sandbox, Some(SandboxModeArg::ReadOnly)));
        assert!(calls_models(&cli.command));

        let cli = Cli::try_parse_from(["tars", "--sandbox", "read-only", "init"]).expect("parse");
        assert!(!calls_models(&cli.command));
    }

    fn models_command(argv: &[&str]) -> Result<models::ModelsCommand, models::ModelsUsageError> {
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let Command::Models(args) = cli.command else {
            panic!("{argv:?} is not `tars models`");
        };
        args.command()
    }

    #[test]
    fn models_json_counts_on_either_side_of_update() {
        use models::ModelsCommand;
        for argv in [
            &["tars", "models", "--json", "update", "g"][..],
            &["tars", "models", "update", "g", "--json"][..],
        ] {
            assert_eq!(
                models_command(argv),
                Ok(ModelsCommand::Update {
                    provider: Some(tars_types::ProviderId::new("g")),
                    json: true,
                }),
                "{argv:?}"
            );
        }
        assert_eq!(
            models_command(&["tars", "models", "g", "--live", "--json"]),
            Ok(ModelsCommand::List {
                provider: Some(tars_types::ProviderId::new("g")),
                live: true,
                json: true,
            })
        );
    }

    #[test]
    fn models_refuses_listing_flags_given_with_update() {
        use models::ModelsUsageError;
        assert_eq!(
            models_command(&["tars", "models", "--live", "update"]),
            Err(ModelsUsageError::LiveWithUpdate)
        );
        assert_eq!(
            models_command(&["tars", "models", "g", "update"]),
            Err(ModelsUsageError::ProviderBeforeUpdate {
                provider: tars_types::ProviderId::new("g"),
            })
        );
    }
}
