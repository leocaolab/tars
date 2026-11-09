//! Runs in its own process, because it sets the process-global `log` logger before `init`.

use tracing_log::log;

struct OtherLogger;

impl log::Log for OtherLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, _: &log::Record<'_>) {}
    fn flush(&self) {}
}

static OTHER: OtherLogger = OtherLogger;

#[test]
fn a_taken_log_logger_is_a_warning_beside_the_installed_subscriber() {
    log::set_logger(&OTHER).expect("no logger is set in a fresh test process");

    let installed = tars_telemetry::init(
        tars_telemetry::TelemetryConfig::from_env("tars-telemetry", 0, None).config,
    )
    .expect("the tracing subscriber installs");

    assert!(
        installed
            .warnings
            .iter()
            .any(|w| matches!(w, tars_telemetry::TelemetryWarning::LogLoggerTaken(_))),
        "{:?}",
        installed.warnings
    );
    assert!(tracing::dispatcher::has_been_set());
}
