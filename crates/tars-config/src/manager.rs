//! Top-level [`Config`] container + [`ConfigManager`] for loading from
//! a TOML file. v0.1: single-file load, full validation, no hot-reload.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tars_types::ProviderId;

use crate::builtin::merge_builtin_with_user;
use crate::error::{ConfigError, ValidationError};
use crate::providers::ProvidersConfig;
use crate::roles::RoleConfig;
use crate::routing::RoutingConfig;
use crate::sandbox::SandboxConfig;

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

    /// `[roles]` — arbitrary role name → `(provider, model)`. See
    /// [`crate::roles`].
    ///
    /// ```toml
    /// [roles.critic]
    /// provider = "deepseek"
    /// model    = "deepseek-chat"
    /// ```
    ///
    /// Distinct from [`routing`](Self::routing): `routing.tiers` keys on the
    /// fixed [`ModelTier`](tars_types::ModelTier) enum, whereas `roles` keys on
    /// free-form names. Resolution is one lookup, no fallback chain; a role
    /// that is not configured is an error. Each `provider` must reference a
    /// declared provider id (checked by [`validate`](Self::validate)).
    #[serde(default)]
    pub roles: HashMap<String, RoleConfig>,

    /// M4 (D6): user security config → `tars_sandbox::SandboxPolicy`, threaded
    /// into `ToolContext.sandbox`. **Optional** — absent `[sandbox]` = `None` =
    /// today's behaviour (unconfined / `DangerFullAccess`). Present = opt into
    /// confinement. See [`crate::sandbox`] + [`crate::resolve_policy`] (the
    /// `--sandbox` flag overrides the mode here).
    #[serde(default)]
    pub sandbox: Option<SandboxConfig>,

    /// `[web_search]` — which web-search backend the `web.search` tool uses.
    /// Schema owned by sisurf ([`sisurf_core::SearchConfig`]); the committed
    /// TOML omits the API key, which tars resolves from the environment and
    /// injects via [`crate::inject_search_keys`]. **Optional** — absent =
    /// keyless DuckDuckGo default.
    #[serde(default)]
    pub web_search: Option<sisurf_core::SearchConfig>,

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
        // Each `[roles]` entry must reference a known provider id — same
        // dangling-reference check the routing tiers get — and name a model.
        for (role, entry) in &self.roles {
            let id = &entry.provider;
            if !known.contains(id) {
                errs.push(ValidationError::new(
                    format!("roles.{role}.provider"),
                    format!(
                        "references unknown provider `{id}` — add a [providers.{id}] section or fix the role mapping"
                    ),
                ));
            }
            if entry.model.trim().is_empty() {
                errs.push(ValidationError::new(
                    format!("roles.{role}.model"),
                    "is empty — a role must name the concrete model it calls".to_string(),
                ));
            }
        }
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
/// Under Doc 06 (process isolation) this loads the global immutable Config
/// once from ~/.tars; the per-workspace `[roles]` overlay is a separate small
/// layer. The old shared-process 5-layer merge (System/Tenant/Per-Request +
/// hot reload) is the DEPRECATED appendix of Doc 06, not the target.
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
        // `load_from_file` opens a fresh handle on `f.path()`; flush so the
        // bytes are guaranteed visible through that second handle.
        f.flush().unwrap();
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
    fn web_search_section_loads_into_config_and_is_key_injectable() {
        // A `[web_search]` table must parse (Config has deny_unknown_fields, so
        // the field has to exist) straight into sisurf's owned schema, with the
        // secret ABSENT from the file — tars injects it from the env later.
        let toml_str = r#"
            [web_search]
            backend = "google_cse"
            google_cse = { cx = "my-cx-id" }
        "#;
        let cfg = ConfigManager::load_from_str(toml_str).expect("web_search section must load");
        let ws = cfg.web_search.expect("web_search present");
        assert_eq!(ws.backend, sisurf_core::BackendKind::GoogleCse);
        assert_eq!(ws.google_cse.as_ref().unwrap().cx, "my-cx-id");
        assert!(
            ws.google_cse.as_ref().unwrap().api_key.is_empty(),
            "the API key must NOT be committed in config.toml"
        );
        // Without an injected key, build() typed-fails (not a silent fallback).
        assert!(matches!(
            ws.build(),
            Err(sisurf_core::WebError::MissingApiKey(_))
        ));
    }

    #[test]
    fn absent_web_search_section_is_none() {
        let cfg = ConfigManager::load_from_str("[providers]\n").expect("loads");
        assert!(cfg.web_search.is_none(), "absent [web_search] = None (keyless DDG default)");
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
    fn roles_table_loads_as_name_to_provider_and_model() {
        let toml_str = r#"
            [providers.deepseek]
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "deepseek-chat"

            [roles.critic]
            provider = "deepseek"
            model    = "deepseek-chat"

            [roles.fixer]
            provider = "mlx"
            model    = "mlx-community/Qwen3-8B"
        "#;
        let cfg = ConfigManager::load_from_str(toml_str).expect("[roles.<name>] must load");
        assert_eq!(cfg.roles["critic"].provider, tars_types::ProviderId::new("deepseek"));
        assert_eq!(cfg.roles["critic"].model, "deepseek-chat");
        // `mlx` is a built-in, merged in, so a role pointing at it validates.
        assert_eq!(cfg.roles["fixer"].provider, tars_types::ProviderId::new("mlx"));
    }

    #[test]
    fn absent_roles_table_is_empty_map() {
        let cfg = ConfigManager::load_from_str("[providers]\n").expect("loads");
        assert!(cfg.roles.is_empty(), "absent [roles] = empty map");
    }

    #[test]
    fn role_referencing_unknown_provider_fails_validation() {
        let toml_str = r#"
            [roles.critic]
            provider = "no_such_provider"
            model    = "whatever"
        "#;
        let err = ConfigManager::load_from_str(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationFailed { errors } => {
                // The key points at the offending *field*, not just the role.
                assert!(errors.iter().any(|e| e.key == "roles.critic.provider"));
                assert!(errors.iter().any(|e| e.message.contains("no_such_provider")));
            }
            _ => panic!("wrong error variant: {err:?}"),
        }
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
