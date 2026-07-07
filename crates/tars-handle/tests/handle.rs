//! Integration tests for the [`Tars`] handle (Doc 06 M2/M3).
//!
//! All tests share one process-global config (installed once, first-wins)
//! with two in-process mock providers (`mock1`, `mock2`) and a `default` tier
//! → `mock1`, so the global registry builds deterministically regardless of
//! test order / parallelism. Two providers (not one) means the sole-provider
//! fallback never fires, so each resolution path is exercised genuinely.
//!
//! `UnknownRole` needs a *different* global config (no `default` tier, >1
//! provider) and the config singleton is first-wins per process, so that case
//! lives in its own integration binary (`tests/unknown_role.rs`).

use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;

use futures::StreamExt;
use tars_config::{Config, ProviderConfig, ProvidersConfig, RoutingConfig};
use tars_handle::{StoreScope, Tars, WorkspaceHandles};
use tars_types::{ChatEvent, ChatRequest, ModelHint, ModelTier, ProviderId, RequestContext};

/// Install the shared test config once (idempotent — first writer wins).
fn ensure_config() {
    if Config::is_loaded() {
        return;
    }
    let mut map: HashMap<ProviderId, ProviderConfig> = HashMap::new();
    for name in ["mock1", "mock2"] {
        map.insert(
            ProviderId::new(name),
            ProviderConfig::Mock {
                canned_response: "hi".to_string(),
            },
        );
    }
    // Tier routing now lives in the GLOBAL config, not the workspace `[roles]`.
    let mut tiers = HashMap::new();
    tiers.insert(ModelTier::Default, vec![ProviderId::new("mock1")]);
    let cfg = Config {
        providers: ProvidersConfig::from_map(map),
        routing: RoutingConfig { tiers },
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
fn flat_roles_map_wins_over_the_fallback_chain() {
    ensure_config();
    // Workspace `[roles]` is a FLAT name → provider id map (arc/concer shape).
    // `critic` maps to `mock2` even though the `default` tier is `mock1`, so
    // resolving `critic` to `mock2` proves the flat map takes priority.
    let ws = workspace_with_config("[roles]\ncritic = \"mock2\"\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open workspace");

    let critic = tars.provider("critic").expect("flat [roles] entry resolves");
    assert_eq!(
        critic.id(),
        &ProviderId::new("mock2"),
        "critic must resolve to its mapped provider, not the default tier",
    );
}

#[test]
fn role_resolves_through_the_fallback_chain() {
    ensure_config();
    // No workspace `[roles]` — exercise the tier / literal / default fallbacks.
    let ws = workspace_with_config("");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open workspace");

    // (a) role names a tier → global routing → registry.
    assert_eq!(
        tars.provider("default").expect("default tier resolves").id(),
        &ProviderId::new("mock1"),
    );
    // (b) role is a literal provider id.
    assert_eq!(
        tars.provider("mock2").expect("literal provider id resolves").id(),
        &ProviderId::new("mock2"),
    );
    // (c) unknown role falls through to the `default` tier candidate.
    assert_eq!(
        tars.provider("whatever").expect("falls back to default tier").id(),
        &ProviderId::new("mock1"),
    );
}

#[test]
fn for_workspace_bootstraps_and_scopes_to_workspace() {
    ensure_config();
    let ws = workspace_with_config("");
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
        "[store]
enabled = false
",
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
    let a = workspace_with_config("");
    let b = workspace_with_config("");
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

#[test]
fn lifecycle_switch_back_close_and_reconstruct() {
    // The full consumer lifecycle (Doc 06 §10 / CUJ-5): open A, switch to B,
    // switch back to A (cache hit — NOT rebuilt), close A (B survives), then
    // reconstruct A from its on-disk state.
    ensure_config();
    let a = workspace_with_config("[roles]\ncritic = \"mock1\"\n");
    let b = workspace_with_config("");
    let handles: WorkspaceHandles = Mutex::new(HashMap::new());
    let root_a = a.path().canonicalize().unwrap();
    let root_b = b.path().canonicalize().unwrap();

    // `built` counts real handle constructions: a cache hit must not bump it.
    let mut built = 0usize;
    let store_a;
    {
        let mut map = handles.lock().unwrap();
        store_a = map
            .entry(root_a.clone())
            .or_insert_with(|| {
                built += 1;
                Tars::for_workspace("arc", &root_a).unwrap()
            })
            .store_scope()
            .clone();
        // switch to B.
        map.entry(root_b.clone()).or_insert_with(|| {
            built += 1;
            Tars::for_workspace("arc", &root_b).unwrap()
        });
        // switch BACK to A — same root, must be a cache hit (no rebuild).
        map.entry(root_a.clone()).or_insert_with(|| {
            built += 1;
            Tars::for_workspace("arc", &root_a).unwrap()
        });
        assert_eq!(built, 2, "A + B built once each; switch-back-to-A is cached");
        // The cached A still resolves its flat `[roles]` entry.
        assert_eq!(
            map.get(&root_a).unwrap().provider("critic").unwrap().id(),
            &ProviderId::new("mock1"),
        );
    }

    // close A: cancel + evict. B is untouched and still resolves.
    {
        let mut map = handles.lock().unwrap();
        map.remove(&root_a).expect("A was open").cancel();
        assert!(!map.contains_key(&root_a));
        assert!(
            map.get(&root_b).unwrap().provider("default").is_ok(),
            "B still resolves after A is closed",
        );
    }

    // reconstruct: reopen A's root — lands on the SAME on-disk store and
    // resolves against the `[roles]` it persisted.
    let reopened = Tars::for_workspace("arc", &root_a).expect("reopen A");
    assert_eq!(
        reopened.store_scope(),
        &store_a,
        "reopened A maps to the same on-disk store dir",
    );
    assert_eq!(
        reopened.provider("critic").unwrap().id(),
        &ProviderId::new("mock1"),
        "reopened A resolves against its persisted [roles]",
    );
}

#[tokio::test]
async fn pipeline_completes_end_to_end_over_the_mock() {
    // Load a config with a `[roles]` + a mock provider → for_workspace →
    // pipeline(role) → the call completes and the mock's canned response flows
    // back through the whole middleware chain (no network).
    ensure_config();
    let ws = workspace_with_config("[roles]\ncritic = \"mock1\"\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open");

    let pipe = tars.pipeline("critic").expect("pipeline builds for the mapped role");
    let req = ChatRequest::user(ModelHint::Explicit("mock-model".into()), "review this");
    let mut stream = std::sync::Arc::new(pipe)
        .call(req, RequestContext::test_default())
        .await
        .expect("pipeline call opens a stream over the mock");

    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        if let ChatEvent::Delta { text: t } = ev.expect("event") {
            text.push_str(&t);
        }
    }
    assert_eq!(
        text, "hi",
        "the mock's canned response completes back through the pipeline",
    );
}

#[tokio::test]
async fn runtime_is_usable_for_run_plan() {
    ensure_config();
    let ws = workspace_with_config("");
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
    let ws = workspace_with_config("");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open");
    let token = tars.cancel_token();
    assert!(!token.is_cancelled());
    tars.cancel();
    assert!(token.is_cancelled());
    drop(tars); // Drop calls cancel() again — must not panic.
    assert!(token.is_cancelled());
}
