//! Integration tests for the [`Tars`] handle (Doc 06 M2/M3).
//!
//! All tests share one process-global config (installed once, first-wins)
//! with a single in-process `mock1` provider, so the global registry builds
//! deterministically regardless of test order / parallelism.

use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;

use tars_config::{Config, ProviderConfig, ProvidersConfig};
use tars_handle::{StoreScope, Tars, WorkspaceHandles};
use tars_types::ProviderId;

/// Install the shared test config once (idempotent — first writer wins).
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

/// Bootstrap a fake workspace `<dir>/.arc/config.toml` with the given body.
fn workspace_with_config(body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let arc_dir = tmp.path().join(".arc");
    fs::create_dir_all(&arc_dir).unwrap();
    fs::write(arc_dir.join("config.toml"), body).unwrap();
    tmp
}

#[test]
fn role_resolves_through_both_layers() {
    ensure_config();
    // Workspace layer: default tier → the global `mock1` provider.
    let ws = workspace_with_config("[roles.tiers]\ndefault = [\"mock1\"]\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open workspace");

    // (a) role names the tier → workspace roles → global registry.
    tars.provider("default").expect("default tier resolves");
    // (b) role is a literal provider id.
    tars.provider("mock1").expect("literal provider id resolves");
    // (c) unknown role falls through to the `default` tier candidate.
    tars.provider("whatever").expect("falls back to default tier");
}

#[test]
fn for_workspace_bootstraps_and_scopes_to_workspace() {
    ensure_config();
    let ws = workspace_with_config("[roles.tiers]\ndefault = [\"mock1\"]\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open");

    let store_dir = ws.path().canonicalize().unwrap().join(".arc").join("tars");
    assert_eq!(
        tars.store_scope(),
        &StoreScope::Workspace(store_dir.clone())
    );
    assert!(store_dir.is_dir(), "store dir bootstrapped on first use");
    // pipeline() wires the sink and builds the canonical chain.
    let pipe = tars.pipeline("default").expect("pipeline builds");
    assert!(pipe.layer_names().contains(&"event_emitter"));
}

#[test]
fn store_off_opts_out_of_persistence() {
    ensure_config();
    let ws = workspace_with_config(
        "[roles.tiers]\ndefault = [\"mock1\"]\n[store]\nenabled = false\n",
    );
    let tars = Tars::for_workspace("arc", ws.path()).expect("open");
    assert_eq!(tars.store_scope(), &StoreScope::Off);
    // Provider + pipeline still work; the pipeline just has no emitter layer.
    let pipe = tars.pipeline("default").expect("pipeline builds without store");
    assert!(!pipe.layer_names().contains(&"event_emitter"));
}

#[test]
fn lifecycle_open_switch_close_keeps_registry_shared() {
    ensure_config();
    let a = workspace_with_config("[roles.tiers]\ndefault = [\"mock1\"]\n");
    let b = workspace_with_config("[roles.tiers]\ndefault = [\"mock1\"]\n");
    let handles: WorkspaceHandles = Mutex::new(HashMap::new());

    // The global registry is built once and never rebuilt across handles.
    let reg1 = tars_provider::ProviderRegistry::global().unwrap();

    // open A, then B (switch): both cached, registry not rebuilt.
    let root_a = a.path().canonicalize().unwrap();
    let root_b = b.path().canonicalize().unwrap();
    {
        let mut map = handles.lock().unwrap();
        map.entry(root_a.clone())
            .or_insert_with(|| Tars::for_workspace("arc", &root_a).unwrap());
        map.entry(root_b.clone())
            .or_insert_with(|| Tars::for_workspace("arc", &root_b).unwrap());
        assert_eq!(map.len(), 2);
    }

    let reg2 = tars_provider::ProviderRegistry::global().unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&reg1, &reg2),
        "registry must be shared, not rebuilt on switch"
    );

    // close A: removed + cancelled; B survives.
    {
        let mut map = handles.lock().unwrap();
        if let Some(t) = map.remove(&root_a) {
            t.cancel();
        }
        assert!(!map.contains_key(&root_a));
        assert!(map.contains_key(&root_b));
    }
}

#[tokio::test]
async fn runtime_is_usable_for_run_plan() {
    ensure_config();
    let ws = workspace_with_config("[roles.tiers]\ndefault = [\"mock1\"]\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open");
    let runtime = tars.runtime();
    // Exercise the runtime end-to-end against its scoped event store.
    let traj = runtime
        .create_trajectory(None, "handle-test")
        .await
        .expect("runtime writes to the scoped store");
    assert!(!traj.to_string().is_empty());
}

#[test]
fn cancel_before_drop_is_idempotent() {
    ensure_config();
    let ws = workspace_with_config("[roles.tiers]\ndefault = [\"mock1\"]\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open");
    let token = tars.cancel_token();
    assert!(!token.is_cancelled());
    tars.cancel();
    assert!(token.is_cancelled());
    drop(tars); // Drop calls cancel() again — must not panic.
    assert!(token.is_cancelled());
}
