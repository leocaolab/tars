//! Runs the `tars` binary, because what is checked is how the process ends on a bad argument.

use std::process::Command;

/// The exit status clap gives a command line it refuses.
const USAGE_ERROR: i32 = 2;

#[test]
fn an_empty_provider_id_is_a_usage_error_not_a_panic() {
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["probe", "", "--tenant", "t"])
        .output()
        .expect("the tars binary runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(USAGE_ERROR), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(stderr.contains("ProviderId cannot be empty"), "{stderr}");
}
