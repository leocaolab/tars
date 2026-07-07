//! The sole-provider fallback (resolve rule 5) needs a registry with EXACTLY
//! one provider and NO `default` tier — otherwise the tier fallback (rule 4)
//! would answer first and mask it. The config singleton is process-global
//! first-wins, so this case gets its own integration binary rather than sharing
//! `tests/handle.rs` (whose global config has two providers + a `default` tier).

use std::collections::HashMap;
use std::fs;

use tars_config::{Config, ProviderConfig, ProvidersConfig};
use tars_handle::Tars;
use tars_types::ProviderId;

fn ensure_config() {
    if Config::is_loaded() {
        return;
    }
    // A single provider and no routing tiers: an unmapped role can't land on
    // `[roles]`, a tier, a literal id, or a `default` tier — only rule 5 (the
    // registry's one unambiguous provider) can answer.
    let mut map: HashMap<ProviderId, ProviderConfig> = HashMap::new();
    map.insert(
        ProviderId::new("only"),
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

#[test]
fn sole_provider_absorbs_any_unmapped_role() {
    ensure_config();
    let ws = workspace_with_config(""); // no `[roles]` overlay
    let tars = Tars::for_workspace("arc", ws.path()).expect("open workspace");

    // An arbitrary, unmapped role resolves to the registry's single provider.
    assert_eq!(
        tars.provider("some_arbitrary_role").expect("sole provider answers").id(),
        &ProviderId::new("only"),
    );
    // A DIFFERENT unmapped role resolves to the same sole provider — the
    // fallback is by registry cardinality, not by any per-role mapping.
    assert_eq!(
        tars.provider("yet_another_role").expect("sole provider answers").id(),
        &ProviderId::new("only"),
    );
}
