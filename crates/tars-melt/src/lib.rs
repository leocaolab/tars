//! tars-melt — telemetry initialization. Doc 08 + Doc 14 M0 / M5.
//!
//! M1 scope (this crate): one-line `tracing` subscriber install with a
//! pretty / JSON formatter switch and an `EnvFilter`. Just enough for
//! every `tars-*` binary (`tars-cli` today, `tars-server` later) to
//! emit consistent structured logs to stderr without each binary
//! re-implementing the same `tracing_subscriber::fmt()` boilerplate.
//!
//! M5 will grow:
//! - OTel `tracing-opentelemetry` layer (composes via `with()`)
//! - Metrics registry (Prometheus exporter etc.)
//! - `SecretField<T>` for per-record redaction (today
//!   `tars_types::SecretString` already covers the only consumer —
//!   API keys / bearer tokens — so the generic version is YAGNI)
//! - Cardinality validator for label sets
//! - Trace head + tail sampling
//!
//! ## Why a `TelemetryGuard` when the work is one-shot
//!
//! `tracing_subscriber` install is a one-shot global; nothing to
//! drain. But once we add the OTel exporter (M5), it'll need a
//! `Drop`-time flush so the last batch of spans actually leaves the
//! process. Returning a `TelemetryGuard` now lets every caller bind
//! it (`let _guard = tars_melt::init(cfg)?`) and stops being a
//! breaking change later. Today the guard is a typed `()`.

use thiserror::Error;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TelemetryFormat {
    /// Human-friendly ANSI-coloured output. Default for interactive
    /// terminals; the wrong choice for log aggregators.
    #[default]
    Pretty,
    /// One JSON object per record on stderr. The right choice for
    /// any deployment that ships logs to Loki / Datadog / CloudWatch.
    Json,
}

impl TelemetryFormat {
    /// Parse from a config string (`"pretty"` / `"json"`). Unknown
    /// values fall back to `Pretty` with no error — observability
    /// shouldn't take down a request path.
    pub fn from_env_string(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" => Self::Json,
            "pretty" | "" => Self::Pretty,
            other => {
                // No `tracing` available yet (we ARE init); use stderr.
                eprintln!(
                    "tars-melt: unknown TARS_LOG_FORMAT={other:?} — defaulting to pretty",
                );
                Self::Pretty
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    /// EnvFilter directive string. Same shape as `RUST_LOG`:
    /// `"warn"`, `"tars=debug,warn"`, `"tars_provider=trace"`, …
    pub level: String,
    pub format: TelemetryFormat,
    /// Service identifier baked into every record (`service=`).
    /// Defaults to the executing crate name; binaries should override.
    pub service: String,
    /// Whether to emit span enter/exit events. `false` keeps logs
    /// small; flip to `true` when debugging a span-shape bug.
    pub include_span_events: bool,
}

impl TelemetryConfig {
    /// Convenience for the common CLI verbosity-flag pattern:
    ///   0 → warn, 1 → tars=info+warn, 2 → tars=debug+info,
    ///   3+ → tars=trace+debug.
    /// `RUST_LOG` overrides the verbosity-derived default if set.
    pub fn from_verbosity(verbose: u8) -> Self {
        let level = match verbose {
            0 => "warn".to_string(),
            1 => "tars=info,warn".to_string(),
            2 => "tars=debug,info".to_string(),
            _ => "tars=trace,debug".to_string(),
        };
        let format = match std::env::var("TARS_LOG_FORMAT") {
            Ok(s) => TelemetryFormat::from_env_string(&s),
            Err(std::env::VarError::NotPresent) => TelemetryFormat::default(),
            Err(e) => {
                // NotUnicode etc. — surface so the operator notices their
                // intent was lost rather than silently getting Pretty.
                eprintln!(
                    "tars-melt: TARS_LOG_FORMAT could not be read: {e} — defaulting to pretty",
                );
                TelemetryFormat::default()
            }
        };
        Self {
            level,
            format,
            service: env!("CARGO_PKG_NAME").to_string(),
            include_span_events: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    /// `try_init` ran AFTER another subscriber was already installed.
    /// Not actually fatal in tests; callers handling this can fall
    /// through to "the existing subscriber wins". Carries the attempted
    /// service+level so an operator debugging "why didn't my config
    /// take" knows what was rejected.
    #[error(
        "a global tracing subscriber is already installed \
         (attempted service={service:?}, level={level:?})"
    )]
    AlreadyInstalled { service: String, level: String },
    /// `config.level` failed `EnvFilter` parsing. We refuse to install
    /// rather than panic: the directive came from a caller and may be
    /// user-supplied.
    #[error("invalid filter directive {directive:?}: {reason}")]
    InvalidFilter { directive: String, reason: String },
}

/// RAII handle for the installed telemetry stack. M1: empty marker.
/// M5: holds the OTel exporter shutdown channel so `Drop` flushes the
/// last batch of spans.
#[must_use = "drop the guard at process exit so future OTel exporters can flush"]
pub struct TelemetryGuard {
    _private: (),
}

impl TelemetryGuard {
    fn new() -> Self {
        Self { _private: () }
    }
}

/// Install the global `tracing` subscriber. Idempotent: if a subscriber
/// is already installed (e.g. another crate ran `init` first), returns
/// `Err(TelemetryError::AlreadyInstalled)` and leaves the existing
/// subscriber alone.
///
/// Output goes to **stderr** so a binary's stdout stays pipeable as
/// pure protocol output (the LLM response in `tars run`).
pub fn init(config: TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    // RUST_LOG always wins over our derived default — operators
    // expect the standard env var to work. If RUST_LOG is set but
    // malformed, fall through to config.level (operator misconfig
    // shouldn't kill the process).
    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => EnvFilter::try_new(&config.level).map_err(|e| {
            TelemetryError::InvalidFilter {
                directive: config.level.clone(),
                reason: e.to_string(),
            }
        })?,
    };

    let span_events = if config.include_span_events {
        FmtSpan::NEW | FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };

    let result = match config.format {
        TelemetryFormat::Pretty => tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .with_target(false)
            .with_span_events(span_events)
            .try_init(),
        TelemetryFormat::Json => tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .with_target(true)
            .with_span_events(span_events)
            .json()
            .flatten_event(true) // top-level fields, not nested under "fields"
            .with_current_span(true)
            .with_span_list(false)
            .try_init(),
    };

    // Sanitize service for stderr/Pretty output: a manually-constructed
    // config with newlines or ANSI escapes in `service` could otherwise
    // forge log records or break log parsing. JSON mode escapes for us
    // but Pretty %Display does not.
    let safe_service = sanitize_service(&config.service);

    if result.is_err() {
        return Err(TelemetryError::AlreadyInstalled {
            service: safe_service,
            level: config.level,
        });
    }

    // Stamp the service identity once via a top-level info!. Logs
    // aggregators key off this for filtering / dashboards.
    tracing::info!(
        service = %safe_service,
        format = ?config.format,
        version = env!("CARGO_PKG_VERSION"),
        "telemetry initialized",
    );
    Ok(TelemetryGuard::new())
}

/// Replace control characters (newlines, ANSI ESC, etc.) in a service
/// identifier so they can't forge fake log records when the Pretty
/// formatter writes the value to stderr verbatim.
fn sanitize_service(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

/// Best-effort init that surfaces every error to stderr but never
/// returns it. Useful when a library wants telemetry on but tolerates
/// "the binary already set one up".
///
/// The match is exhaustive on purpose: adding a new `TelemetryError`
/// variant must force a compile error here so we make a deliberate
/// choice about whether it should be swallowed or escalated, instead
/// of silently disappearing through `.ok()`.
pub fn init_or_warn(config: TelemetryConfig) -> Option<TelemetryGuard> {
    match init(config) {
        Ok(g) => Some(g),
        Err(e @ TelemetryError::AlreadyInstalled { .. }) => {
            // Expected in libraries / nested test binaries — log and move on.
            eprintln!("tars-melt: {e}");
            None
        }
        Err(e @ TelemetryError::InvalidFilter { .. }) => {
            // Operator misconfig — very much worth shouting about,
            // but still not fatal to the whole process.
            eprintln!("tars-melt: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_levels_map_to_filter_strings() {
        // Substring is too loose ("warnings" / "beware" pass) and tells
        // us nothing about whether EnvFilter actually accepts the
        // directive. Pin both: exact string AND a real parse.
        for (verbose, expected) in [
            (0u8, "warn"),
            (1, "tars=info,warn"),
            (2, "tars=debug,info"),
            (3, "tars=trace,debug"),
            (99, "tars=trace,debug"),
        ] {
            let cfg = TelemetryConfig::from_verbosity(verbose);
            assert_eq!(cfg.level, expected, "verbosity={verbose}");
            EnvFilter::try_new(&cfg.level)
                .unwrap_or_else(|e| panic!("verbosity={verbose} produced unparsable filter {:?}: {e}", cfg.level));
        }
    }

    #[test]
    fn from_env_string_parses_known_formats() {
        assert_eq!(TelemetryFormat::from_env_string("json"), TelemetryFormat::Json);
        assert_eq!(TelemetryFormat::from_env_string("JSON"), TelemetryFormat::Json);
        assert_eq!(TelemetryFormat::from_env_string("pretty"), TelemetryFormat::Pretty);
        assert_eq!(TelemetryFormat::from_env_string(""), TelemetryFormat::Pretty);
        // Unknown falls back to Pretty (best-effort, no panic).
        assert_eq!(
            TelemetryFormat::from_env_string("logfmt"),
            TelemetryFormat::Pretty,
        );
    }

    /// The interesting tests — actually installing a subscriber and
    /// asserting on emitted JSON shape — need the global tracing
    /// state, which `cargo test` runs concurrently. Each test would
    /// race against the install. We exercise install via `init_or_warn`
    /// so the second run-through is a benign None.
    ///
    /// For shape verification (does JSON formatter actually emit JSON?)
    /// we'd want a custom MakeWriter that captures bytes. Out of scope
    /// for M1 — the upstream `tracing-subscriber` test suite covers
    /// that. We just need to know *our wiring* doesn't panic.
    #[test]
    fn init_or_warn_does_not_panic_first_or_second_call() {
        // This is the only test in the crate that calls `init_*`, so
        // the first call lands on a fresh global and must succeed;
        // the second hits the `AlreadyInstalled` path. If a future
        // refactor breaks `init_or_warn` so it always returns `None`,
        // the first assertion catches it.
        let g1 = init_or_warn(TelemetryConfig::from_verbosity(0));
        assert!(g1.is_some(), "first install should succeed on a fresh global");
        let g2 = init_or_warn(TelemetryConfig::from_verbosity(0));
        assert!(g2.is_none(), "second install should be skipped");
    }

    // NOTE: `#[must_use]` is a lint, not a runtime property — it cannot
    // be observed at runtime, so a `#[test]` cannot meaningfully verify
    // it. The previous test here just checked `type_name` and would
    // pass even after the attribute was removed; deleting it removes
    // false confidence. The attribute lives on `TelemetryGuard` above
    // and is enforced by the compiler at every call site.

    #[test]
    fn invalid_filter_is_reported_not_panicked() {
        // Sanity-check that a malformed directive surfaces as
        // `InvalidFilter` rather than panicking inside `EnvFilter::new`.
        // We bypass the global subscriber by avoiding `init` (which
        // would race), and just exercise the parse path directly.
        assert!(EnvFilter::try_new("=== not a directive ===").is_err());
    }
}
