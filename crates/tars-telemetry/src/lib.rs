//! Installs the process's `tracing` subscriber: pretty or JSON on stderr, an `EnvFilter`, and
//! OTLP trace and metric export with the `otlp` feature. It stores nothing and reads back nothing;
//! a run's records are the event log's (tars-melt).
//!
//! [`TelemetryConfig::from_env`] reads the `TARS_`, `OTEL_` and `RUST_LOG` variables once and
//! returns the config with the ones it could not use; [`init`] returns an [`Installation`]: a
//! [`TelemetryGuard`] whose drop flushes the OTLP exporters, so a binary keeps it for the life of
//! the process, and the settings `init` could not use. Neither prints them; what to tell the
//! operator is the caller's decision.

#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::ffi::{OsStr, OsString};
use std::io::IsTerminal as _;

use thiserror::Error;
use tracing_log::AsLog as _;
use tracing_subscriber::filter::{Directive, EnvFilter};
use tracing_subscriber::fmt::format::FmtSpan;

#[cfg(feature = "otlp")]
mod integers;
#[cfg(feature = "otlp")]
mod metrics;
#[cfg(feature = "otlp")]
mod otlp;
#[cfg(feature = "otlp")]
mod propagation;
#[cfg(feature = "otlp")]
mod span_events;
#[cfg(feature = "otlp")]
pub use propagation::{continue_trace, trace_headers};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TelemetryFormat {
    /// ANSI-coloured lines for a person at a terminal.
    #[default]
    Pretty,
    /// One JSON object per record, for a deployment that ships logs to an aggregator.
    Json,
}

#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    pub level: FilterDirectives,
    pub format: TelemetryFormat,
    /// The service name stamped on the startup record and the OTLP resource.
    pub service: String,
    /// Whether to log span open and close events.
    pub include_span_events: bool,
    /// Where spans and metrics are exported; `None` exports nothing. Without the `otlp` feature
    /// a set export is ignored with a warning.
    pub otlp: Option<OtlpExport>,
}

/// An OTLP collector to export to, and how often a root trace is exported to it.
#[derive(Clone, Debug)]
pub struct OtlpExport {
    /// The collector endpoint, such as `"http://localhost:4317"`.
    pub endpoint: String,
    /// A span whose parent was sampled is always kept, so distributed traces stay whole.
    pub trace_sample_ratio: SampleRatio,
}

/// The `EnvFilter` directives the subscriber filters records by, such as `tars=info,warn`. Only
/// [`FilterDirectives::parse`] makes one, so `init` never meets a directive that does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterDirectives(Vec<Directive>);

impl FilterDirectives {
    /// Parses comma-separated directives the way `EnvFilter` does, skipping empty ones.
    ///
    /// # Errors
    ///
    /// The `EnvFilter` parse error of the first directive that does not parse.
    pub fn parse(raw: &str) -> Result<Self, tracing_subscriber::filter::ParseError> {
        raw.split(',')
            .filter(|d| !d.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map(Self)
    }

    fn to_env_filter(&self) -> EnvFilter {
        self.0
            .iter()
            .cloned()
            .fold(EnvFilter::default(), EnvFilter::add_directive)
    }
}

impl std::fmt::Display for FilterDirectives {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, directive) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{directive}")?;
        }
        Ok(())
    }
}

/// The probability, in `0.0..=1.0`, that a root trace is exported. Only [`SampleRatio::new`]
/// makes one, so a NaN or out-of-range ratio never reaches the sampler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleRatio(f64);

impl SampleRatio {
    /// Exports every root trace.
    pub const EVERY_TRACE: Self = Self(1.0);

    /// # Errors
    ///
    /// [`SampleRatioRangeError`] when `ratio` is NaN or outside `0.0..=1.0`.
    pub fn new(ratio: f64) -> Result<Self, SampleRatioRangeError> {
        if (0.0..=1.0).contains(&ratio) {
            Ok(Self(ratio))
        } else {
            Err(SampleRatioRangeError { ratio })
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq)]
#[error("sample ratio {ratio} is not in 0.0..=1.0")]
pub struct SampleRatioRangeError {
    pub ratio: f64,
}

/// A [`TelemetryConfig`] read from the environment, and the variables that were set but could
/// not be used; the config goes on without each of them, as the warning says.
#[must_use = "the warnings are settings the operator set and did not get"]
#[derive(Debug)]
pub struct EnvConfig {
    pub config: TelemetryConfig,
    pub warnings: Vec<TelemetryWarning>,
}

impl TelemetryConfig {
    /// Builds the config of `service` from a CLI `-v` count (0 → `warn`, 1 → `tars=info,warn`, 2 →
    /// `tars=debug,info`, 3+ → `tars=trace,debug`) and the environment, whose `TARS_`, `OTEL_`
    /// and `RUST_LOG` variables are read here and nowhere else (`NO_COLOR` is [`init`]'s): a set,
    /// non-empty and valid `RUST_LOG` replaces the level; `format`, the caller's own choice, wins
    /// over `TARS_LOG_FORMAT`; the OTLP endpoint and sample ratio come from
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_TRACES_SAMPLER_ARG`. An empty or blank variable
    /// counts as unset.
    pub fn from_env(
        service: impl Into<String>,
        verbose: u8,
        format: Option<TelemetryFormat>,
    ) -> EnvConfig {
        // `RUST_LOG` and the `OTEL_` variables belong to tracing and OpenTelemetry, not tars, so
        // they are read here directly rather than through `tars_types::env`.
        let (level, rust_log_warning) = level_from(std::env::var("RUST_LOG"), verbose);
        let (format, format_warning) = match format {
            Some(format) => (format, None),
            None => format_from(tars_types::env::log_format_raw()),
        };
        let (otlp_endpoint, endpoint_warning) =
            endpoint_from(std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"));
        let (trace_sample_ratio, ratio_warning) =
            sample_ratio_from(std::env::var("OTEL_TRACES_SAMPLER_ARG"));

        EnvConfig {
            config: Self {
                level,
                format,
                service: service.into(),
                include_span_events: false,
                otlp: otlp_endpoint.map(|endpoint| OtlpExport {
                    endpoint,
                    trace_sample_ratio,
                }),
            },
            warnings: [
                rust_log_warning,
                format_warning,
                endpoint_warning,
                ratio_warning,
            ]
            .into_iter()
            .flatten()
            .collect(),
        }
    }
}

/// Returns the filter directives for a `-v` count.
fn verbosity_level(verbose: u8) -> &'static str {
    match verbose {
        0 => "warn",
        1 => "tars=info,warn",
        2 => "tars=debug,info",
        _ => "tars=trace,debug",
    }
}

/// Returns `RUST_LOG` when it is set and parses as a filter, otherwise the level of `verbose`
/// together with the set `RUST_LOG` it ignored. An empty or blank one counts as unset:
/// `RUST_LOG=` in an env template would otherwise parse as a filter that turns every record off.
fn level_from(
    rust_log: Result<String, std::env::VarError>,
    verbose: u8,
) -> (FilterDirectives, Option<TelemetryWarning>) {
    let ignored = match rust_log {
        Err(std::env::VarError::NotPresent) => None,
        Ok(raw) if raw.trim().is_empty() => None,
        Err(std::env::VarError::NotUnicode(raw)) => {
            Some(TelemetryWarning::RustLogNotUnicode { raw })
        }
        Ok(raw) => match FilterDirectives::parse(&raw) {
            Ok(level) => return (level, None),
            Err(source) => Some(TelemetryWarning::InvalidRustLog { raw, source }),
        },
    };
    (verbosity_filter(verbose), ignored)
}

/// Returns the filter of a `-v` count.
fn verbosity_filter(verbose: u8) -> FilterDirectives {
    FilterDirectives::parse(verbosity_level(verbose)).expect(
        "the -v directives are constants of this file, and \
         verbosity_levels_map_to_filter_strings_that_parse parses every one",
    )
}

/// Returns the format `TARS_LOG_FORMAT` names (`pretty` or `json`, ignoring case and surrounding
/// blanks); unset or blank is `Pretty`, and so is a value that names no format or is not
/// Unicode, which comes back as a warning.
fn format_from(
    raw: Result<String, std::env::VarError>,
) -> (TelemetryFormat, Option<TelemetryWarning>) {
    match raw {
        Err(std::env::VarError::NotPresent) => (TelemetryFormat::Pretty, None),
        Err(std::env::VarError::NotUnicode(raw)) => (
            TelemetryFormat::Pretty,
            Some(TelemetryWarning::LogFormatNotUnicode { raw }),
        ),
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "json" => (TelemetryFormat::Json, None),
            "pretty" | "" => (TelemetryFormat::Pretty, None),
            _ => (
                TelemetryFormat::Pretty,
                Some(TelemetryWarning::UnknownLogFormat { raw }),
            ),
        },
    }
}

/// Returns the trimmed OTLP endpoint; unset or blank exports nothing, and so does a value that
/// is not Unicode, which comes back as a warning.
fn endpoint_from(
    raw: Result<String, std::env::VarError>,
) -> (Option<String>, Option<TelemetryWarning>) {
    match raw {
        Err(std::env::VarError::NotPresent) => (None, None),
        Err(std::env::VarError::NotUnicode(raw)) => {
            (None, Some(TelemetryWarning::OtlpEndpointNotUnicode { raw }))
        }
        Ok(raw) => {
            let endpoint = raw.trim();
            ((!endpoint.is_empty()).then(|| endpoint.to_string()), None)
        }
    }
}

/// Returns the sample ratio `OTEL_TRACES_SAMPLER_ARG` gives as a number in `0.0..=1.0`; unset or
/// blank samples every trace, and so does any other value, which comes back as a warning. It
/// falls back to every trace rather than none, so a misconfiguration costs volume, not traces.
fn sample_ratio_from(
    raw: Result<String, std::env::VarError>,
) -> (SampleRatio, Option<TelemetryWarning>) {
    match raw {
        Err(std::env::VarError::NotPresent) => (SampleRatio::EVERY_TRACE, None),
        Err(std::env::VarError::NotUnicode(raw)) => (
            SampleRatio::EVERY_TRACE,
            Some(TelemetryWarning::SampleRatioNotUnicode { raw }),
        ),
        Ok(raw) if raw.trim().is_empty() => (SampleRatio::EVERY_TRACE, None),
        Ok(raw) => match raw.trim().parse::<f64>().map(SampleRatio::new) {
            Ok(Ok(ratio)) => (ratio, None),
            Ok(Err(_)) | Err(_) => (
                SampleRatio::EVERY_TRACE,
                Some(TelemetryWarning::InvalidSampleRatio { raw }),
            ),
        },
    }
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    /// Another global subscriber was already installed, and it stays. The refused service and
    /// level are carried so an operator can see which config did not take.
    #[error(
        "a global tracing subscriber is already installed \
         (attempted service={service:?}, level=\"{level}\")"
    )]
    AlreadyInstalled {
        service: String,
        level: FilterDirectives,
    },

    /// Building an OTLP exporter failed (a bad endpoint, a transport error).
    #[cfg(feature = "otlp")]
    #[error("OTLP exporter init failed (endpoint={endpoint:?})")]
    OtlpExport {
        endpoint: String,
        #[source]
        source: OtlpBuildError,
    },
}

/// The exporter whose build failed, with the exporter's own error.
#[cfg(feature = "otlp")]
#[derive(Debug, Error)]
pub enum OtlpBuildError {
    #[error("traces")]
    Traces(#[source] opentelemetry::trace::TraceError),
    #[error("metrics")]
    Metrics(#[source] opentelemetry_sdk::metrics::MetricError),
}

/// A setting that was set but could not be used; telemetry goes on without it, as the message
/// says. [`TelemetryConfig::from_env`] returns the environment's, [`init`] its own.
#[derive(Debug, Error)]
pub enum TelemetryWarning {
    #[error("TARS_LOG_FORMAT={raw:?} is not `pretty` or `json`; logging pretty")]
    UnknownLogFormat { raw: String },
    #[error("TARS_LOG_FORMAT={raw:?} is not Unicode; logging pretty")]
    LogFormatNotUnicode { raw: OsString },
    #[error("RUST_LOG={raw:?} is not Unicode; filtering by the configured level")]
    RustLogNotUnicode { raw: OsString },
    #[error("RUST_LOG={raw:?} is invalid; filtering by the configured level")]
    InvalidRustLog {
        raw: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    #[error("OTEL_EXPORTER_OTLP_ENDPOINT={raw:?} is not Unicode; exporting nothing")]
    OtlpEndpointNotUnicode { raw: OsString },
    #[error("OTEL_TRACES_SAMPLER_ARG={raw:?} is not a number in 0.0..=1.0; sampling every trace")]
    InvalidSampleRatio { raw: String },
    #[error("OTEL_TRACES_SAMPLER_ARG={raw:?} is not Unicode; sampling every trace")]
    SampleRatioNotUnicode { raw: OsString },
    /// Only a build without the `otlp` feature returns it.
    #[error(
        "OTLP endpoint {endpoint:?} is set, but this build has no `otlp` feature, \
         so spans and metrics are not exported"
    )]
    OtlpUnavailable { endpoint: String },
    /// Records of crates that log through the `log` crate go to the logger set first.
    #[error(
        "another `log` logger was set first, so records of crates that log through `log` \
         do not reach the tracing subscriber"
    )]
    LogLoggerTaken(#[source] tracing_log::log::SetLoggerError),
}

/// What [`init`] installed: the guard to keep, and the settings `init` itself could not use (an
/// OTLP endpoint without the `otlp` feature, a `log` logger already set).
#[must_use = "keep the guard until process exit so the OTel exporter can flush"]
#[derive(Debug)]
pub struct Installation {
    pub guard: TelemetryGuard,
    pub warnings: Vec<TelemetryWarning>,
}

/// The installed telemetry. With OTLP export on, dropping it shuts the exporters down, which sends
/// the last batch of spans and metrics; otherwise it holds nothing.
#[must_use = "drop the guard at process exit so the OTel exporter can flush"]
#[derive(Debug)]
pub struct TelemetryGuard {
    #[cfg(feature = "otlp")]
    providers: Option<OtlpProviders>,
    #[cfg(not(feature = "otlp"))]
    _private: (),
}

/// The trace and metric providers OTLP export installs; one is never built without the other.
#[cfg(feature = "otlp")]
#[derive(Debug)]
struct OtlpProviders {
    tracer: opentelemetry_sdk::trace::TracerProvider,
    meter: opentelemetry_sdk::metrics::SdkMeterProvider,
}

impl TelemetryGuard {
    fn new() -> Self {
        Self {
            #[cfg(feature = "otlp")]
            providers: None,
            #[cfg(not(feature = "otlp"))]
            _private: (),
        }
    }

    /// Returns whether spans leave the process, which they do only with OTLP export installed.
    pub fn exports_traces(&self) -> bool {
        #[cfg(feature = "otlp")]
        {
            self.providers.is_some()
        }
        #[cfg(not(feature = "otlp"))]
        {
            false
        }
    }

    #[cfg(feature = "otlp")]
    fn with_providers(providers: OtlpProviders) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // A failed shutdown loses the trailing spans and metrics. The global subscriber outlives
        // this guard and still writes to stderr, so the warning reaches whoever reads the logs.
        #[cfg(feature = "otlp")]
        if let Some(OtlpProviders { tracer, meter }) = self.providers.take() {
            if let Err(error) = tracer.shutdown() {
                tracing::warn!(%error, "the OTLP tracer did not shut down cleanly; trailing spans may be lost");
            }
            if let Err(error) = meter.shutdown() {
                tracing::warn!(%error, "the OTLP meter did not shut down cleanly; trailing metrics may be lost");
            }
        }
    }
}

/// Installs the global `tracing` subscriber and routes the `log` crate's records into it. It
/// writes to the process's stderr (fd 2), so a binary's stdout stays its own output (the LLM
/// response of `tars probe`); Pretty output is coloured only when stderr is a terminal and
/// `NO_COLOR` is unset or empty. With an OTLP endpoint it must run inside a Tokio runtime, where
/// the batch exporter spawns its task. It reads no `TARS_`, `OTEL_` or `RUST_LOG` variable; a
/// setting it cannot use is skipped and returned in [`Installation::warnings`], not printed.
///
/// # Errors
///
/// [`TelemetryError::AlreadyInstalled`] when a subscriber is already installed, which stays, and
/// nothing is installed; [`TelemetryError::OtlpExport`] when an OTLP exporter cannot be built.
pub fn init(config: TelemetryConfig) -> Result<Installation, TelemetryError> {
    let filter = config.level.to_env_filter();
    let format = config.format;
    #[cfg(feature = "otlp")]
    let warnings = Vec::new();
    #[cfg(not(feature = "otlp"))]
    let warnings: Vec<TelemetryWarning> = config
        .otlp
        .as_ref()
        .map(|otlp| TelemetryWarning::OtlpUnavailable {
            endpoint: otlp.endpoint.clone(),
        })
        .into_iter()
        .collect();

    let span_events = if config.include_span_events {
        FmtSpan::NEW | FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };

    #[cfg(feature = "otlp")]
    if let Some(export) = &config.otlp {
        return otlp::install(&config, export, filter, span_events, warnings);
    }

    let subscriber: tracing::Dispatch = match format {
        TelemetryFormat::Pretty => tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(stderr_colour())
            .with_env_filter(filter)
            .with_target(false)
            .with_span_events(span_events)
            .finish()
            .into(),
        TelemetryFormat::Json => tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .with_target(true)
            .with_span_events(span_events)
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .finish()
            .into(),
    };
    let installed = install_global(subscriber, TelemetryGuard::new(), &config, warnings)?;

    tracing::info!(
        service = %sanitize_service(&config.service),
        format = ?format,
        version = env!("CARGO_PKG_VERSION"),
        "telemetry initialized",
    );
    Ok(installed)
}

/// Returns whether Pretty output on this process's stderr is coloured.
fn stderr_colour() -> bool {
    colour(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    )
}

/// Decides whether Pretty output is coloured: only on a terminal, and not when `NO_COLOR` is set
/// to a non-empty value (<https://no-color.org>). It is decided here because `with_ansi`
/// overrides the subscriber's own `NO_COLOR` check.
fn colour(is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    is_terminal && no_color.is_none_or(OsStr::is_empty)
}

/// Sets `subscriber` as the global `tracing` dispatcher, then routes the `log` crate's records
/// into it. When the dispatcher is taken nothing is installed and `guard` is dropped, which shuts
/// its exporters down. The dispatcher cannot be unset, so a taken `log` logger is one more of
/// `warnings`, and the guard is returned.
fn install_global(
    subscriber: tracing::Dispatch,
    guard: TelemetryGuard,
    config: &TelemetryConfig,
    mut warnings: Vec<TelemetryWarning>,
) -> Result<Installation, TelemetryError> {
    // The error says only that a dispatcher is already set, which the variant says.
    tracing::dispatcher::set_global_default(subscriber).map_err(|_| {
        TelemetryError::AlreadyInstalled {
            // A newline or ANSI escape in `service` would forge log records in Pretty output,
            // which writes the value verbatim; JSON escapes it.
            service: sanitize_service(&config.service),
            level: config.level.clone(),
        }
    })?;
    // After the dispatcher, so the `log` crate's max level is the installed filter's.
    if let Err(source) = tracing_log::LogTracer::builder()
        .with_max_level(tracing::level_filters::LevelFilter::current().as_log())
        .init()
    {
        warnings.push(TelemetryWarning::LogLoggerTaken(source));
    }
    Ok(Installation { guard, warnings })
}

/// Replaces every control character (newline, ANSI escape) in `s` with `_`.
fn sanitize_service(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::VarError;

    fn set(s: &str) -> Result<String, VarError> {
        Ok(s.to_string())
    }

    #[cfg(unix)]
    fn not_unicode() -> Result<String, VarError> {
        use std::os::unix::ffi::OsStringExt as _;
        Err(VarError::NotUnicode(OsString::from_vec(vec![b'x', 0xff])))
    }

    #[test]
    fn verbosity_levels_map_to_filter_strings_that_parse() {
        for (verbose, expected) in [
            (0u8, "warn"),
            (1, "tars=info,warn"),
            (2, "tars=debug,info"),
            (3, "tars=trace,debug"),
            (99, "tars=trace,debug"),
        ] {
            let (level, ignored) = level_from(Err(VarError::NotPresent), verbose);
            assert_eq!(level.to_string(), expected, "verbosity={verbose}");
            assert!(ignored.is_none());
        }
    }

    #[test]
    fn a_valid_rust_log_replaces_the_level() {
        let (level, ignored) = level_from(set("tars=trace"), 0);
        assert_eq!(level.to_string(), "tars=trace");
        assert!(ignored.is_none());
    }

    #[test]
    fn an_empty_or_blank_rust_log_counts_as_unset() {
        for raw in ["", "  "] {
            let (level, ignored) = level_from(set(raw), 0);
            assert_eq!(level.to_string(), "warn", "RUST_LOG={raw:?}");
            assert!(ignored.is_none(), "RUST_LOG={raw:?}: {ignored:?}");
        }
    }

    #[test]
    fn an_invalid_rust_log_keeps_the_level_and_is_reported_with_its_value() {
        let (level, ignored) = level_from(set("=== not a directive ==="), 1);
        assert_eq!(level.to_string(), "tars=info,warn");
        let ignored = ignored.expect("the invalid RUST_LOG is reported");
        assert!(
            ignored.to_string().contains("=== not a directive ==="),
            "{ignored}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_rust_log_keeps_the_level_and_is_reported() {
        let (level, ignored) = level_from(not_unicode(), 0);
        assert_eq!(level.to_string(), "warn");
        assert!(matches!(
            ignored,
            Some(TelemetryWarning::RustLogNotUnicode { .. })
        ));
    }

    #[test]
    fn tars_log_format_names_a_format_ignoring_case_and_unset_or_blank_is_pretty() {
        let quiet = |raw| {
            let (format, warning) = format_from(raw);
            assert!(warning.is_none(), "{warning:?}");
            format
        };
        assert_eq!(quiet(set("json")), TelemetryFormat::Json);
        assert_eq!(quiet(set(" JSON ")), TelemetryFormat::Json);
        assert_eq!(quiet(set("pretty")), TelemetryFormat::Pretty);
        assert_eq!(quiet(set(" ")), TelemetryFormat::Pretty);
        assert_eq!(quiet(Err(VarError::NotPresent)), TelemetryFormat::Pretty);
    }

    #[test]
    fn an_unknown_tars_log_format_logs_pretty_and_is_reported_with_its_value() {
        let (format, warning) = format_from(set("logfmt"));
        assert_eq!(format, TelemetryFormat::Pretty);
        assert!(
            matches!(&warning, Some(TelemetryWarning::UnknownLogFormat { raw }) if raw == "logfmt"),
            "{warning:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_tars_log_format_logs_pretty_and_is_reported() {
        assert!(matches!(
            format_from(not_unicode()),
            (
                TelemetryFormat::Pretty,
                Some(TelemetryWarning::LogFormatNotUnicode { .. })
            )
        ));
    }

    #[test]
    fn an_otlp_endpoint_is_trimmed_and_a_blank_one_exports_nothing() {
        assert_eq!(
            endpoint_from(set(" http://c:4317 ")).0.as_deref(),
            Some("http://c:4317")
        );
        for raw in [set(""), set("  "), Err(VarError::NotPresent)] {
            let (endpoint, warning) = endpoint_from(raw);
            assert_eq!(endpoint, None);
            assert!(warning.is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_otlp_endpoint_exports_nothing_and_is_reported() {
        assert!(matches!(
            endpoint_from(not_unicode()),
            (None, Some(TelemetryWarning::OtlpEndpointNotUnicode { .. }))
        ));
    }

    #[test]
    fn a_sample_ratio_inside_zero_to_one_is_used_as_given() {
        for (raw, ratio) in [("0", 0.0), (" 0.25 ", 0.25), ("1", 1.0)] {
            let (got, warning) = sample_ratio_from(set(raw));
            assert_eq!(got.get(), ratio, "{raw:?}");
            assert!(warning.is_none(), "{raw:?}: {warning:?}");
        }
    }

    #[test]
    fn an_unset_or_blank_sample_ratio_samples_every_trace_quietly() {
        for raw in [Err(VarError::NotPresent), set(""), set(" ")] {
            let (ratio, warning) = sample_ratio_from(raw);
            assert_eq!(ratio, SampleRatio::EVERY_TRACE);
            assert!(warning.is_none(), "{warning:?}");
        }
    }

    #[test]
    fn a_sample_ratio_outside_zero_to_one_or_not_a_number_samples_every_trace_and_is_reported() {
        for raw in ["-0.01", "1.01", "NaN", "inf", "half"] {
            let (ratio, warning) = sample_ratio_from(set(raw));
            assert_eq!(ratio, SampleRatio::EVERY_TRACE, "{raw:?}");
            assert!(
                matches!(&warning, Some(TelemetryWarning::InvalidSampleRatio { raw: r }) if r == raw),
                "{raw:?}: {warning:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_sample_ratio_samples_every_trace_and_is_reported() {
        let (ratio, warning) = sample_ratio_from(not_unicode());
        assert_eq!(ratio, SampleRatio::EVERY_TRACE);
        assert!(
            matches!(
                warning,
                Some(TelemetryWarning::SampleRatioNotUnicode { .. })
            ),
            "{warning:?}"
        );
    }

    #[test]
    fn a_sample_ratio_is_made_only_inside_zero_to_one() {
        for ratio in [0.0, 1.0] {
            assert_eq!(SampleRatio::new(ratio).map(SampleRatio::get), Ok(ratio));
        }
        for ratio in [-f64::EPSILON, 1.0 + f64::EPSILON, f64::NAN, f64::INFINITY] {
            assert!(SampleRatio::new(ratio).is_err(), "{ratio}");
        }
    }

    #[test]
    fn a_terminal_is_coloured_unless_no_color_is_set_non_empty() {
        assert!(colour(true, None));
        assert!(colour(true, Some(OsStr::new(""))));
        assert!(!colour(true, Some(OsStr::new("1"))));
        assert!(!colour(false, None));
    }

    #[test]
    fn an_invalid_directive_makes_no_filter() {
        assert!(FilterDirectives::parse("=== not a directive ===").is_err());
    }

    /// Installing touches the process-global subscriber, so this is the only unit test whose
    /// `init` installs: its first call lands on a fresh global and the second is refused.
    #[test]
    fn a_second_init_is_refused_as_already_installed() {
        let config =
            || TelemetryConfig::from_env("tars-telemetry", 0, Some(TelemetryFormat::Pretty)).config;
        let first = init(config());
        assert!(first.is_ok(), "first install on a fresh global: {first:?}");
        let second = init(config());
        assert!(
            matches!(second, Err(TelemetryError::AlreadyInstalled { .. })),
            "{second:?}"
        );
    }
}
