//! `StoreScope::TarsHome` via `[store] location = "tars_home"` (Doc 06 §7): a
//! workspace can force its observability store out of `<root>/.<tool>/tars/`
//! and into the `~/.tars/ws/<hash>/` home fallback. Proving it needs a
//! controlled `$TARS_HOME`, but the workspace forbids `unsafe` and
//! `std::env::set_var` is `unsafe` in Rust 2024 — so (like `tars-provider`'s
//! `security_delegate_cli` test) the parent re-execs this binary with
//! `$TARS_HOME` pointed at a throwaway dir on the child `Command`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command as StdCommand;

use tars_config::{Config, ProviderConfig, ProvidersConfig};
use tars_handle::{StoreScope, Tars, tars_home_store_dir};
use tars_types::ProviderId;

const CHILD_MARKER: &str = "TARS_STORE_TARS_HOME_CHILD";
const TEST_NAME: &str = "store_location_tars_home_forces_the_home_fallback";

fn ensure_config() {
    if Config::is_loaded() {
        return;
    }
    let mut map: HashMap<ProviderId, ProviderConfig> = HashMap::new();
    map.insert(
        ProviderId::new("mock1"),
        ProviderConfig::Mock {
            canned_response: "hi".to_string(),
        },
    );
    let cfg = Config {
        providers: ProvidersConfig::from_map(map),
        ..Default::default()
    };
    Config::install_global(cfg);
}

fn workspace_with_config(body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let arc_dir = tmp.path().join(".arc");
    fs::create_dir_all(&arc_dir).unwrap();
    fs::write(arc_dir.join("config.toml"), body).unwrap();
    tmp
}

/// Child body: `$TARS_HOME` is set by the parent's re-exec.
fn run_child() {
    let home = tars_config::resolve_home(None).expect("$TARS_HOME resolves in the child");
    ensure_config();

    let ws = workspace_with_config("[store]\nlocation = \"tars_home\"\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open");

    let canonical_root = ws.path().canonicalize().unwrap();
    let expected = tars_home_store_dir(&home, &canonical_root);
    assert_eq!(
        tars.store_scope(),
        &StoreScope::TarsHome(expected.clone()),
        "location=tars_home routes the store under $TARS_HOME/ws/<hash>, not the workspace",
    );
    assert!(
        expected.is_dir(),
        "home-fallback store dir is bootstrapped on open",
    );
}

#[test]
fn store_location_tars_home_forces_the_home_fallback() {
    if std::env::var(CHILD_MARKER).is_ok() {
        run_child();
        return;
    }

    // Parent: throwaway home + re-exec THIS test with `$TARS_HOME` set on the
    // child `Command` (safe, unlike `env::set_var`). `home` outlives the child
    // (`.status()` blocks), then Drop cleans it up.
    let home = tempfile::tempdir().unwrap();
    let exe: PathBuf = std::env::current_exe().expect("locate current test binary");
    let status = StdCommand::new(exe)
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env("TARS_HOME", home.path())
        .env(CHILD_MARKER, "1")
        .status()
        .expect("re-exec child");
    assert!(status.success(), "child assertions failed");
}
