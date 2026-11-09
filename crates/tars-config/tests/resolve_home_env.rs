//! Checks that `$TARS_HOME` beats `~/.tars`, an explicit home beats `$TARS_HOME`, and the default
//! config file is the one in `$TARS_HOME`.
//!
//! `std::env::set_var` is `unsafe` in Rust 2024 and the workspace forbids `unsafe`, so the test
//! re-execs its own binary with `$TARS_HOME` set on the child `Command`.

use std::path::PathBuf;
use std::process::Command as StdCommand;

use tars_config::{default_config_path, resolve_home};

const CHILD_MARKER: &str = "TARS_RESOLVE_HOME_ENV_CHILD";
const TEST_NAME: &str = "env_beats_default_and_flag_beats_env_and_the_config_is_under_it";
const ENV_HOME: &str = "/env/tars/home";

#[test]
fn env_beats_default_and_flag_beats_env_and_the_config_is_under_it() {
    if std::env::var(CHILD_MARKER).is_ok() {
        assert_eq!(resolve_home(None), Some(PathBuf::from(ENV_HOME)));
        let flag = PathBuf::from("/explicit/tars/home");
        assert_eq!(resolve_home(Some(flag.clone())), Some(flag));
        assert_eq!(
            default_config_path(),
            Some(PathBuf::from(ENV_HOME).join("config.toml"))
        );
        return;
    }

    let exe = std::env::current_exe().expect("locate current test binary");
    let status = StdCommand::new(exe)
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env("TARS_HOME", ENV_HOME)
        .env(CHILD_MARKER, "1")
        .status()
        .expect("re-exec child");
    assert!(status.success(), "child assertions failed");
}
