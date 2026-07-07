//! `UnknownRole` needs a global config with NO `default` tier and more than one
//! provider (so neither the tier nor the sole-provider fallback can absorb an
//! unmapped role). The config singleton is process-global first-wins, so this
//! case gets its own integration binary rather than sharing `tests/handle.rs`.

use std::collections::HashMap;
use std::fs;

use tars_config::{Config, ProviderConfig, ProvidersConfig};
use tars_handle::{Tars, TarsError};
use tars_types::ProviderId;

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
    // No routing tiers → no `default` tier fallback; two providers → no
    // sole-provider fallback. An unmapped role has nowhere to land.
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

#[test]
fn unmapped_role_errors_unknown_role() {
    ensure_config();
    let ws = workspace_with_config("[roles]\ncritic = \"mock1\"\n");
    let tars = Tars::for_workspace("arc", ws.path()).expect("open workspace");

    // The mapped role still resolves through the flat map.
    assert_eq!(
        tars.provider("critic").expect("mapped role resolves").id(),
        &ProviderId::new("mock1"),
    );

    // An unmapped role — not in `[roles]`, not a tier, not a literal provider
    // id, no `default` tier, not a sole-provider registry — is UnknownRole.
    // (`Arc<dyn LlmProvider>` isn't `Debug`, so match rather than `expect_err`.)
    match tars.provider("nonexistent_role") {
        Ok(_) => panic!("unmapped role must not resolve"),
        Err(TarsError::UnknownRole { role, .. }) => assert_eq!(role, "nonexistent_role"),
        Err(other) => panic!("expected UnknownRole, got {other:?}"),
    }
}
