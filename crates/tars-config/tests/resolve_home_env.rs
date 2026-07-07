//! `resolve_home` precedence: `--tars_home` flag > `$TARS_HOME` > `~/.tars`.
//! The env-over-default branch needs `$TARS_HOME` set, but the workspace forbids
//! `unsafe` and `std::env::set_var` is `unsafe` in Rust 2024 — so (like
//! `tars-provider`'s `security_delegate_cli` test) the parent re-execs this test
//! binary with `$TARS_HOME` set on the child `Command` (safe, process-isolated).
//! The in-crate unit test `resolve_home_falls_back_to_dot_tars` covers the
//! default branch (only when `$TARS_HOME` is unset); this covers the env branch.

use std::path::PathBuf;
use std::process::Command as StdCommand;

use tars_config::resolve_home;

const CHILD_MARKER: &str = "TARS_RESOLVE_HOME_ENV_CHILD";
const TEST_NAME: &str = "env_beats_default_and_flag_beats_env";
const ENV_HOME: &str = "/env/tars/home";

#[test]
fn env_beats_default_and_flag_beats_env() {
    if std::env::var(CHILD_MARKER).is_ok() {
        // Child: `$TARS_HOME` is set by the parent's re-exec below.
        // No flag → `$TARS_HOME` wins over the `~/.tars` default.
        assert_eq!(resolve_home(None), Some(PathBuf::from(ENV_HOME)));
        // An explicit flag still beats the env var.
        let flag = PathBuf::from("/explicit/tars/home");
        assert_eq!(resolve_home(Some(flag.clone())), Some(flag));
        return;
    }

    // Parent: re-exec THIS test with `$TARS_HOME` set on the child `Command`
    // (safe, unlike `env::set_var`; mutation isolated to the child process).
    let exe = std::env::current_exe().expect("locate current test binary");
    let status = StdCommand::new(exe)
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env("TARS_HOME", ENV_HOME)
        .env(CHILD_MARKER, "1")
        .status()
        .expect("re-exec child");
    assert!(status.success(), "child assertions failed");
}
