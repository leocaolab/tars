//! Loads one TOML file into a [`Config`], merges the built-in providers under it and validates
//! the result. There is no reload: a loaded config does not change.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tars_types::ProviderId;

use crate::builtin::merge_builtin_with_user;
use crate::error::{ConfigError, ValidationError, ValidationErrors, ValidationProblem};
use crate::providers::ProvidersConfig;
use crate::roles::RoleConfig;
use crate::sandbox::SandboxConfig;

/// A loaded tars config: the operator's providers with the built-ins under them, the roles and
/// the sandbox table. Only [`Config::load_file`] and [`Config::from_toml`] make one, so every
/// `Config` has passed validation.
#[derive(Clone, Debug)]
pub struct Config {
    providers: ProvidersConfig,
    roles: HashMap<String, RoleConfig>,
    sandbox: Option<SandboxConfig>,
    /// Holds the provider ids the user's TOML declared, before the built-ins were merged in. With
    /// the built-ins always present, `tars run` without `--provider` picks from this set; counting
    /// the built-ins would make every config ambiguous.
    user_provider_ids: HashSet<ProviderId>,
    store_dir: Option<PathBuf>,
}

/// The config file as written, before the built-ins merge in and before validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    providers: ProvidersConfig,
    /// Maps a free-form role name to the provider and model that serve it; see [`crate::roles`].
    ///
    /// ```toml
    /// [roles.critic]
    /// provider = "deepseek"
    /// model    = "deepseek-chat"
    /// ```
    #[serde(default)]
    roles: HashMap<String, RoleConfig>,
    #[serde(default)]
    sandbox: Option<SandboxConfig>,
}

impl Config {
    pub fn providers(&self) -> &ProvidersConfig {
        &self.providers
    }

    pub fn roles(&self) -> &HashMap<String, RoleConfig> {
        &self.roles
    }

    /// Returns the `[sandbox]` table, `None` when there is none, which [`crate::resolve_policy`]
    /// turns into a read-only policy.
    pub fn sandbox(&self) -> Option<&SandboxConfig> {
        self.sandbox.as_ref()
    }

    /// Returns the workspace store directory the consumer named; every `get_*db_path` joins onto
    /// it. It is never read from the file.
    pub fn store_dir(&self) -> Option<&Path> {
        self.store_dir.as_deref()
    }

    /// Returns this config with `dir` as its store directory, typically
    /// `<repo_root>/.<tool>/tars/`; the consumer names it before [`Config::set`].
    #[must_use]
    pub fn with_store_dir(self, dir: PathBuf) -> Self {
        Self {
            store_dir: Some(dir),
            ..self
        }
    }
}

impl Config {
    /// Checks the whole config and returns every error found, not just the first, so the operator
    /// fixes them in one pass.
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errs = Vec::new();
        self.providers.validate(&mut errs);
        let known: HashSet<_> = self.providers.iter().map(|(id, _)| id.clone()).collect();
        for (role, entry) in &self.roles {
            let id = &entry.provider;
            if !known.contains(id) {
                errs.push(ValidationError::new(
                    format!("roles.{role}.provider"),
                    ValidationProblem::UnknownProvider { id: id.to_string() },
                ));
            }
            if let Some(problem) = crate::providers::model_spec_problem(
                &entry.model,
                ValidationProblem::RoleWithoutModel,
            ) {
                errs.push(ValidationError::new(format!("roles.{role}.model"), problem));
            }
        }
        match ValidationErrors::collect(errs) {
            None => Ok(()),
            Some(errors) => Err(errors),
        }
    }

    /// Returns the providers the user's TOML declared, without the merged-in built-ins.
    pub fn user_declared(&self) -> impl Iterator<Item = (&ProviderId, &crate::ProviderConfig)> {
        self.providers
            .iter()
            .filter(|(id, _)| self.user_provider_ids.contains(id))
    }
}

impl Config {
    /// Reads, merges and validates the TOML file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] when the file cannot be read, and what [`Config::from_toml`]
    /// returns otherwise, with `path` in a parse error.
    pub fn load_file(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref().to_path_buf();
        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        Self::parse(&raw, path)
    }

    /// Parses TOML text, lays the built-in providers under it and validates the result.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] for text that is not a config, and
    /// [`ConfigError::ValidationFailed`] with every broken rule otherwise.
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        Self::parse(text, PathBuf::from("<inline>"))
    }

    fn parse(text: &str, path: PathBuf) -> Result<Config, ConfigError> {
        let file: ConfigFile = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path,
            source: Box::new(source),
        })?;
        let cfg = with_builtins(file);
        cfg.validate()
            .map_err(|errors| ConfigError::ValidationFailed { errors })?;
        Ok(cfg)
    }
}

/// Lays the built-in providers under the user's, the user's entry winning on the same id, and
/// records which ids the user declared.
fn with_builtins(file: ConfigFile) -> Config {
    let user = file.providers.providers;
    let user_provider_ids = user.keys().cloned().collect();
    Config {
        providers: ProvidersConfig::from_map(merge_builtin_with_user(user)),
        roles: file.roles,
        sandbox: file.sandbox,
        user_provider_ids,
        store_dir: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempfile_with_contents(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{contents}").unwrap();
        // `load_file` reads through a second handle, which sees only flushed bytes.
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
        let cfg = Config::from_toml(toml_str).unwrap();
        assert!(
            cfg.providers()
                .get(&tars_types::ProviderId::new("openai_main"))
                .is_some()
        );
        assert!(
            cfg.providers()
                .get(&tars_types::ProviderId::new("mlx"))
                .is_some()
        );
        assert!(
            cfg.providers()
                .get(&tars_types::ProviderId::new("vllm"))
                .is_some()
        );
    }

    #[test]
    fn a_routing_section_is_an_unknown_field_and_routing_is_not_offered_as_one() {
        let err = Config::from_toml("[routing.tiers]\nfast = [\"mlx\"]\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let message = tars_utils::display_chain(&err);
        assert!(message.contains("unknown field `routing`"), "{message}");

        let message = tars_utils::display_chain(&Config::from_toml("[routng]\n").unwrap_err());
        assert!(message.contains("unknown field `routng`"), "{message}");
        assert!(!message.contains("`routing`"), "{message}");
    }

    fn validation_problems(toml: &str) -> Vec<(String, ValidationProblem)> {
        match Config::from_toml(toml) {
            Err(ConfigError::ValidationFailed { errors }) => errors
                .as_slice()
                .iter()
                .map(|e| (e.key.clone(), e.problem.clone()))
                .collect(),
            other => panic!("expected a validation failure, got {other:?}"),
        }
    }

    #[test]
    fn a_model_spec_the_grammar_refuses_fails_at_load() {
        for spec in ["@latest", "flash@newest", "flash@"] {
            let toml = format!(
                "[providers.g]\ntype = \"gemini\"\nauth = {{ kind = \"secret\", secret = {{ source = \"env\", var = \"K\" }} }}\ndefault_model = \"{spec}\"\n"
            );
            let problems = validation_problems(&toml);
            assert!(
                matches!(
                    problems.as_slice(),
                    [(key, ValidationProblem::UnparsableModelSpec(_))]
                        if key == "providers.g.default_model"
                ),
                "{spec:?}: {problems:?}"
            );
        }
        let problems =
            validation_problems("[roles.critic]\nprovider = \"gemini\"\nmodel = \"pro@v2\"\n");
        assert!(
            matches!(
                problems.as_slice(),
                [(key, ValidationProblem::UnparsableModelSpec(_))] if key == "roles.critic.model"
            ),
            "{problems:?}"
        );
    }

    #[test]
    fn a_latest_spec_and_a_concrete_id_load() {
        Config::from_toml(
            "[roles.critic]\nprovider = \"gemini\"\nmodel = \"pro@latest\"\n\n[roles.fast]\nprovider = \"gemini\"\nmodel = \"gemini-3.8-flash\"\n",
        )
        .expect("a `<series>@latest` spec and a concrete id are both model specs");
    }

    #[test]
    fn a_cassette_recording_from_an_unknown_provider_or_itself_fails_at_load() {
        for (source, problem) in [
            (
                "no_such_provider",
                ValidationProblem::UnknownProvider {
                    id: "no_such_provider".into(),
                },
            ),
            (
                "tape",
                ValidationProblem::RecordsFromCassette { id: "tape".into() },
            ),
        ] {
            let toml = format!(
                "[providers.tape]\ntype = \"cassette\"\npath = \"/nonexistent/tape.json\"\nrecord_from = \"{source}\"\n"
            );
            match Config::from_toml(&toml) {
                Err(ConfigError::ValidationFailed { errors }) => assert_eq!(
                    errors.as_slice(),
                    [ValidationError::new("providers.tape.record_from", problem)]
                ),
                other => panic!("{source}: expected a validation failure, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_web_search_section_is_an_unknown_field() {
        let err = Config::from_toml("[web_search]\nbackend = \"brave\"\n").unwrap_err();
        assert!(
            tars_utils::display_chain(&err).contains("unknown field `web_search`"),
            "{err}"
        );
    }

    #[test]
    fn a_validation_failure_shows_every_key_and_message() {
        let toml_str = r#"
            [roles.critic]
            provider = "no_such_provider"
            model    = ""
        "#;
        let message = Config::from_toml(toml_str).unwrap_err().to_string();
        assert!(
            message.starts_with("config validation failed:\n  "),
            "{message}"
        );
        assert!(
            message
                .contains("roles.critic.provider: references unknown provider `no_such_provider`"),
            "{message}"
        );
        assert!(
            message.contains("roles.critic.model: is empty"),
            "{message}"
        );
    }

    #[test]
    fn validation_errors_are_never_empty() {
        assert!(ValidationErrors::collect(Vec::new()).is_none());
        let one =
            ValidationErrors::collect(vec![ValidationError::new("k", ValidationProblem::Empty)])
                .unwrap();
        assert_eq!(one.to_string(), "  k: must not be empty");
    }

    #[test]
    fn load_file_round_trip_preserves_user_provider() {
        let toml_str = r#"
            [providers.local_qwen]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
        "#;
        let f = tempfile_with_contents(toml_str);
        let cfg = Config::load_file(f.path()).unwrap();
        assert!(
            cfg.providers()
                .get(&tars_types::ProviderId::new("local_qwen"))
                .is_some()
        );
    }

    #[test]
    fn empty_providers_block_yields_builtins_only() {
        let toml_str = r#"
            [providers]
        "#;
        let cfg = Config::from_toml(toml_str).expect("empty providers + builtins should validate");
        assert!(
            cfg.providers()
                .get(&tars_types::ProviderId::new("openai"))
                .is_some()
        );
        assert!(
            cfg.providers()
                .get(&tars_types::ProviderId::new("mlx"))
                .is_some()
        );
        assert!(
            cfg.providers()
                .get(&tars_types::ProviderId::new("vllm"))
                .is_some()
        );
    }

    #[test]
    fn roles_table_loads_as_name_to_provider_and_model() {
        let toml_str = r#"
            [providers.deepseek]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "deepseek-chat"

            [roles.critic]
            provider = "deepseek"
            model    = "deepseek-chat"

            [roles.fixer]
            provider = "mlx"
            model    = "mlx-community/Qwen3-8B"
        "#;
        let cfg = Config::from_toml(toml_str).expect("[roles.<name>] must load");
        assert_eq!(
            cfg.roles()["critic"].provider,
            tars_types::ProviderId::new("deepseek")
        );
        assert_eq!(cfg.roles()["critic"].model, "deepseek-chat");
        assert_eq!(
            cfg.roles()["fixer"].provider,
            tars_types::ProviderId::new("mlx")
        );
    }

    #[test]
    fn absent_roles_table_is_empty_map() {
        let cfg = Config::from_toml("[providers]\n").expect("loads");
        assert!(cfg.roles().is_empty(), "absent [roles] = empty map");
    }

    #[test]
    fn role_referencing_unknown_provider_fails_validation() {
        let toml_str = r#"
            [roles.critic]
            provider = "no_such_provider"
            model    = "whatever"
        "#;
        let err = Config::from_toml(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationFailed { errors } => {
                let errors = errors.as_slice();
                assert!(errors.iter().any(|e| e.key == "roles.critic.provider"));
                assert!(
                    errors
                        .iter()
                        .any(|e| e.problem.to_string().contains("no_such_provider"))
                );
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
        let err = Config::from_toml(toml_str).unwrap_err();
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn malformed_toml_returns_parse_error_with_path() {
        let toml_str = "not valid toml = = =";
        let err = Config::from_toml(toml_str).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn typo_in_top_level_key_is_caught_by_deny_unknown_fields() {
        let toml_str = r#"
            [providerz.x]      # typo: providerz instead of providers
            type = "mock"
        "#;
        let err = Config::from_toml(toml_str).unwrap_err();
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
        let err = Config::from_toml(toml_str).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn missing_file_returns_io_error_with_path() {
        let err = Config::load_file("/nonexistent/path/config.toml").unwrap_err();
        match err {
            ConfigError::Io { path, .. } => {
                assert!(path.to_str().unwrap().contains("nonexistent"));
            }
            _ => panic!("wrong error variant: {err:?}"),
        }
    }

    #[test]
    fn all_validation_errors_collected_at_once() {
        let toml_str = r#"
            [providers.ant]
            type = "anthropic"
            auth = { kind = "none" }
            default_model = "claude-opus-4-7"

            [providers.compat]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = ""
            default_model = "foo"
        "#;
        let err = Config::from_toml(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationFailed { errors } => {
                let errors = errors.as_slice();
                assert!(errors.iter().any(|e| e.key.contains("ant.auth")));
                assert!(errors.iter().any(|e| e.key.contains("compat.base_url")));
            }
            _ => panic!("wrong error variant"),
        }
    }
}
