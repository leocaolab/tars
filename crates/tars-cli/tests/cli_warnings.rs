//! Runs the `tars` binary, because what is checked is where its startup warnings end up.

use std::process::Command;

#[test]
fn a_telemetry_warning_reaches_stderr_whatever_rust_log_filters() {
    let store = tempfile::tempdir().expect("a temporary directory");
    for rust_log in ["", "error", "tars_cli=debug"] {
        let out = Command::new(env!("CARGO_BIN_EXE_tars"))
            .args(["events", "list", "--tenant", "t", "--store-dir"])
            .arg(store.path())
            .env("TARS_LOG_FORMAT", "jsno")
            .env("RUST_LOG", rust_log)
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env_remove("OTEL_TRACES_SAMPLER_ARG")
            .output()
            .expect("the tars binary runs");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(r#"warning: TARS_LOG_FORMAT="jsno" is not `pretty` or `json`"#),
            "RUST_LOG={rust_log:?}: {stderr}"
        );
    }
}

#[test]
fn the_log_format_is_read_from_tars_log_format_and_no_other_variable() {
    let store = tempfile::tempdir().expect("a temporary directory");
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["events", "list", "--tenant", "t", "--store-dir"])
        .arg(store.path())
        .env("TARS_LOG_FORMAT", "jsno")
        .env("TARS_LOG_FORMAT_FLAG", "json")
        .env_remove("RUST_LOG")
        .output()
        .expect("the tars binary runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(r#"warning: TARS_LOG_FORMAT="jsno" is not `pretty` or `json`"#),
        "{stderr}"
    );
}
