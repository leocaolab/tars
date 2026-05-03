//! Top-level [`Config`] container + [`ConfigManager`] for loading from
//! a TOML file. v0.1: single-file load, full validation, no hot-reload.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, ValidationError};
use crate::providers::ProvidersConfig;

/// Top-level configuration. Future fields (pipeline, cache, agents,
/// tools, tenants, secrets, observability, deployment) land here as
/// each subsystem comes online.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub providers: ProvidersConfig,
}

impl Config {
    /// Validate the entire config; collect all errors before returning
    /// (no fail-fast — operators want the full list to fix in one pass).
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errs = Vec::new();
        self.providers.validate(&mut errs);
        if errs.is_empty() { Ok(()) } else { Err(errs) }
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
        let cfg: Config = toml::from_str(&raw).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        cfg.validate()
            .map_err(|errors| ConfigError::ValidationFailed { errors })?;
        Ok(cfg)
    }

    /// Parse-only — useful for tests and programmatic configuration
    /// (e.g. tests embedding TOML inline).
    pub fn load_from_str(src: &str) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(src).map_err(|e| ConfigError::Parse {
            path: PathBuf::from("<inline>"),
            message: e.to_string(),
        })?;
        cfg.validate()
            .map_err(|errors| ConfigError::ValidationFailed { errors })?;
        Ok(cfg)
    }
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
    fn load_minimal_config_from_str() {
        let toml_str = r#"
            [providers.openai_main]
            type = "openai"
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"
        "#;
        let cfg = ConfigManager::load_from_str(toml_str).unwrap();
        assert_eq!(cfg.providers.len(), 1);
    }

    #[test]
    fn load_from_file_round_trip() {
        let toml_str = r#"
            [providers.local_qwen]
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
        "#;
        let f = tempfile_with_contents(toml_str);
        let cfg = ConfigManager::load_from_file(f.path()).unwrap();
        assert_eq!(cfg.providers.len(), 1);
    }

    #[test]
    fn empty_providers_block_fails_validation() {
        let toml_str = r#"
            [providers]
        "#;
        let err = ConfigManager::load_from_str(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationFailed { errors } => {
                assert!(errors.iter().any(|e| e.key == "providers"));
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
