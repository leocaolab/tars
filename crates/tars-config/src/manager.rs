//! Top-level [`Config`] container + [`ConfigManager`] for loading from
//! a TOML file. v0.1: single-file load, full validation, no hot-reload.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tars_types::ProviderId;

use crate::builtin::merge_builtin_with_user;
use crate::error::{ConfigError, ValidationError};
use crate::providers::ProvidersConfig;
use crate::routing::RoutingConfig;

/// Top-level configuration. Future fields (pipeline, cache, agents,
/// tools, tenants, secrets, observability, deployment) land here as
/// each subsystem comes online.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub providers: ProvidersConfig,

    /// M2: tier-based routing table. Doc 01 §12 + Doc 02 §4.6.
    /// Optional — if missing, the CLI falls through to single-provider
    /// dispatch (existing behaviour).
    #[serde(default)]
    pub routing: RoutingConfig,

    /// IDs that came from the user's TOML, captured *before* the
    /// builtin-merge step so callers can distinguish "explicitly
    /// declared by the user" from "ambient builtin default".
    ///
    /// Used by the CLI's implicit-pick logic: with builtins always
    /// merged in, a user with a single declared provider still expects
    /// `tars run` (no `--provider`) to use it rather than fail with
    /// "ambiguous (8 providers)". This field gives that filter a
    /// reliable basis without sprinkling a second config layer
    /// throughout the codebase.
    ///
    /// Skipped in serialization — it's an internal post-load annotation.
    #[serde(skip)]
    pub user_provider_ids: HashSet<ProviderId>,
}

impl Config {
    /// Validate the entire config; collect all errors before returning
    /// (no fail-fast — operators want the full list to fix in one pass).
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errs = Vec::new();
        self.providers.validate(&mut errs);
        // Routing references must point at known provider IDs.
        let known: HashSet<_> = self.providers.iter().map(|(id, _)| id.clone()).collect();
        self.routing.validate(&known, &mut errs);
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// Iterator over only the providers the user *explicitly* declared
    /// in their TOML — excludes ambient builtin defaults that were
    /// merged in at load time. Use this for "what did the user actually
    /// want" decisions; use `providers.iter()` for "anything resolvable".
    pub fn user_declared(&self) -> impl Iterator<Item = (&ProviderId, &crate::ProviderConfig)> {
        self.providers
            .iter()
            .filter(|(id, _)| self.user_provider_ids.contains(id))
    }
}

/// Loads + validates a [`Config`] from a single TOML file.
///
/// Future iterations grow this into the 5-layer merge described in
/// Doc 06 §2 (Compiled → Built-in → System → User → Tenant → Per-Request).
/// This v0.1 reads exactly one file so we can wire providers up first.
pub struct ConfigManager;

impl ConfigManager {
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref().to_path_buf();
        Self::do_load(path)
    }

    fn do_load(path: PathBuf) -> Result<Config, ConfigError> {
        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        let mut cfg: Config = toml::from_str(&raw).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            source: Box::new(e),
        })?;
        merge_builtins_into(&mut cfg);
        cfg.validate().map_err(ConfigError::validation_failed)?;
        Ok(cfg)
    }

    /// Parse-only — useful for tests and programmatic configuration
    /// (e.g. tests embedding TOML inline).
    pub fn load_from_str(src: &str) -> Result<Config, ConfigError> {
        let mut cfg: Config = toml::from_str(src).map_err(|e| ConfigError::Parse {
            path: PathBuf::from("<inline>"),
            source: Box::new(e),
        })?;
        merge_builtins_into(&mut cfg);
        cfg.validate().map_err(ConfigError::validation_failed)?;
        Ok(cfg)
    }
}

/// Layer the built-in provider defaults under whatever the user
/// declared. User entries with the same id win (per
/// `merge_builtin_with_user`'s semantics). Lets users start with an
/// empty `[providers]` table — built-ins like `mlx`, `vllm`, `openai`
/// are then resolvable by id without any explicit declaration.
///
/// Captures the pre-merge user-declared id set into
/// `cfg.user_provider_ids` so callers can later distinguish "explicit"
/// from "ambient default".
fn merge_builtins_into(cfg: &mut Config) {
    let user = std::mem::take(&mut cfg.providers).providers;
    cfg.user_provider_ids = user.keys().cloned().collect();
    let merged = merge_builtin_with_user(user);
    cfg.providers = ProvidersConfig::from_map(merged);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempfile_with_contents(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{contents}").unwrap();
        f
    }

    #[test]
    fn load_minimal_config_includes_user_and_builtins() {
        let toml_str = r#"
            [providers.openai_main]
            type = "openai"
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"
        "#;
        let cfg = ConfigManager::load_from_str(toml_str).unwrap();
        // User entry present.
        assert!(
            cfg.providers
                .get(&tars_types::ProviderId::new("openai_main"))
                .is_some()
        );
        // Built-ins also merged in (`mlx`, `vllm`, etc.).
        assert!(
            cfg.providers
                .get(&tars_types::ProviderId::new("mlx"))
                .is_some()
        );
        assert!(
            cfg.providers
                .get(&tars_types::ProviderId::new("vllm"))
                .is_some()
        );
    }

    #[test]
    fn load_from_file_round_trip_preserves_user_provider() {
        let toml_str = r#"
            [providers.local_qwen]
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
        "#;
        let f = tempfile_with_contents(toml_str);
        let cfg = ConfigManager::load_from_file(f.path()).unwrap();
        assert!(
            cfg.providers
                .get(&tars_types::ProviderId::new("local_qwen"))
                .is_some()
        );
    }

    #[test]
    fn empty_providers_block_yields_builtins_only() {
        // Previous behavior: empty `[providers]` failed validation.
        // New behavior (per Stage-2 builtin-merge): empty user table
        // is fine, the loader fills in built-in defaults so callers
        // can resolve `mlx` / `vllm` / `openai` etc. without writing
        // any TOML at all.
        let toml_str = r#"
            [providers]
        "#;
        let cfg = ConfigManager::load_from_str(toml_str)
            .expect("empty providers + builtins should validate");
        assert!(
            cfg.providers
                .get(&tars_types::ProviderId::new("openai"))
                .is_some()
        );
        assert!(
            cfg.providers
                .get(&tars_types::ProviderId::new("mlx"))
                .is_some()
        );
        assert!(
            cfg.providers
                .get(&tars_types::ProviderId::new("vllm"))
                .is_some()
        );
    }

    #[test]
    fn anthropic_missing_auth_caught_at_validate() {
        let toml_str = r#"
            [providers.ant]
            type = "anthropic"
            auth = { kind = "none" }
            default_model = "claude-opus-4-7"
        "#;
        let err = ConfigManager::load_from_str(toml_str).unwrap_err();
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn malformed_toml_returns_parse_error_with_path() {
        let toml_str = "not valid toml = = =";
        let err = ConfigManager::load_from_str(toml_str).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn typo_in_top_level_key_is_caught_by_deny_unknown_fields() {
        let toml_str = r#"
            [providerz.x]      # typo: providerz instead of providers
            type = "mock"
        "#;
        let err = ConfigManager::load_from_str(toml_str).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn typo_in_provider_field_is_caught_by_deny_unknown_fields() {
        let toml_str = r#"
            [providers.x]
            type = "openai"
            base_ulr = "wrong"   # typo: base_ulr
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"
        "#;
        let err = ConfigManager::load_from_str(toml_str).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn missing_file_returns_io_error_with_path() {
        let err = ConfigManager::load_from_file("/nonexistent/path/config.toml").unwrap_err();
        match err {
            ConfigError::Io { path, .. } => {
                assert!(path.to_str().unwrap().contains("nonexistent"));
            }
            _ => panic!("wrong error variant: {err:?}"),
        }
    }

    #[test]
    fn all_validation_errors_collected_at_once() {
        // Two distinct violations — we expect BOTH in the error list.
        let toml_str = r#"
            [providers.ant]
            type = "anthropic"
            auth = { kind = "none" }
            default_model = "claude-opus-4-7"

            [providers.compat]
            type = "openai_compat"
            base_url = ""
            default_model = "foo"
        "#;
        let err = ConfigManager::load_from_str(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationFailed { errors } => {
                // Should catch both the anthropic auth violation and
                // the openai_compat empty base_url violation.
                assert!(errors.iter().any(|e| e.key.contains("ant.auth")));
                assert!(errors.iter().any(|e| e.key.contains("compat.base_url")));
            }
            _ => panic!("wrong error variant"),
        }
    }
}
