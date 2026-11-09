//! Loads the CLI's config and runs `tars config`. The user layer is the `--config` path (clap has
//! already merged `$TARS_CONFIG` into it), else [`tars_config::default_config_path`], the file
//! `tars init` writes; the system layer is `--system-config` (or `$TARS_SYSTEM_CONFIG`), read only
//! when given. Every command that reads a config loads it through here, so they all read the same
//! files and take the same operator sections.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use tars_config::{
    Config, ConfigError, EffectiveSetting, LayerPaths, LayeredConfig, OperatorLayers, Section,
    SectionErrors, default_config_path,
};
use tars_llm::{CircuitBreakerConfig, HttpClientConfig, OutputReserve, RetryConfig};

use crate::Ending;

/// The config files the flags and the environment name.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigPaths {
    /// The user layer; `None` is the default path.
    pub(crate) user: Option<PathBuf>,
    /// The system layer; `None` reads none.
    pub(crate) system: Option<PathBuf>,
}

/// Why the CLI's config did not load.
#[derive(Debug, thiserror::Error)]
pub enum LoadConfigError {
    #[error(
        "cannot resolve the tars home for config.toml (set $TARS_HOME or HOME); \
         pass --config <PATH> to specify explicitly"
    )]
    NoHome,
    #[error(
        "config path {} exists but is not a regular file; pass --config <PATH> pointing at a \
         TOML file",
        path.display()
    )]
    NotAFile { path: PathBuf },
    #[error("config file not found at {}; create one or pass --config <PATH>", path.display())]
    NotFound { path: PathBuf },
    #[error("cannot access config file at {}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("loading config from {}", list(files))]
    Invalid {
        files: Vec<PathBuf>,
        #[source]
        source: ConfigError,
    },
    #[error("the operator sections did not assemble:\n{0}")]
    Operator(SectionErrors),
}

/// The CLI's config and the operator settings its commands use.
#[derive(Debug)]
pub(crate) struct LoadedConfig {
    pub(crate) config: Config,
    pub(crate) llm: LlmSettings,
}

/// The `[llm.*]` operator settings. The CLI takes all four whichever command runs: `eval` uses
/// every one, and a file that sets one is never told it went unread because `probe` ran.
#[derive(Debug)]
pub(crate) struct LlmSettings {
    pub(crate) retry: RetryConfig,
    pub(crate) breaker: CircuitBreakerConfig,
    pub(crate) http: HttpClientConfig,
    pub(crate) reserve: OutputReserve,
    /// Holds every setting of the four sections with where it came from.
    pub(crate) settings: Vec<EffectiveSetting>,
}

/// Loads the config the flags name and takes its operator sections, logging every setting with
/// its source.
///
/// # Errors
///
/// Fails when no user path is given and no tars home resolves, when the user file is missing
/// (unless a system file is given) or not a regular file, when a file does not parse, and when an
/// operator section does not assemble.
pub(crate) fn load(paths: &ConfigPaths) -> Result<LoadedConfig, LoadConfigError> {
    let user = paths
        .user
        .clone()
        .or_else(default_config_path)
        .ok_or(LoadConfigError::NoHome)?;
    // Not `Path::is_file()`: it reports an unreadable file as absent.
    match std::fs::metadata(&user) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(LoadConfigError::NotAFile { path: user }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && paths.system.is_some() => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LoadConfigError::NotFound { path: user });
        }
        Err(source) => return Err(LoadConfigError::Unreadable { path: user, source }),
    }
    let loaded = load_layers(&LayerPaths {
        system: paths.system.clone(),
        user: Some(user),
    })?;
    for setting in &loaded.llm.settings {
        tracing::info!(target: "tars::config", "{setting}");
    }
    Ok(loaded)
}

fn load_layers(paths: &LayerPaths) -> Result<LoadedConfig, LoadConfigError> {
    let LayeredConfig { config, operator } =
        Config::load_layers(paths).map_err(|source| LoadConfigError::Invalid {
            files: paths.system.iter().chain(&paths.user).cloned().collect(),
            source,
        })?;
    let llm = take_llm(operator).map_err(LoadConfigError::Operator)?;
    Ok(LoadedConfig { config, llm })
}

fn list(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|f| f.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Takes the four `[llm.*]` sections and ends the assembly, reporting every failure at once.
fn take_llm(mut operator: OperatorLayers) -> Result<LlmSettings, SectionErrors> {
    let retry = operator.take::<RetryConfig>(Section::LlmRetry);
    let breaker = operator.take::<CircuitBreakerConfig>(Section::LlmCircuitBreaker);
    let http = operator.take::<HttpClientConfig>(Section::LlmHttp);
    let reserve = operator.take::<OutputReserve>(Section::LlmReserve);
    let finished = operator.finish();
    let (((retry, breaker), (http, reserve)), ()) = SectionErrors::zip(
        SectionErrors::zip(
            SectionErrors::zip(retry, breaker),
            SectionErrors::zip(http, reserve),
        ),
        finished,
    )?;
    let settings = [
        retry.settings(),
        breaker.settings(),
        http.settings(),
        reserve.settings(),
    ]
    .concat();
    Ok(LlmSettings {
        retry: retry.into_value(),
        breaker: breaker.into_value(),
        http: http.into_value(),
        reserve: reserve.into_value(),
        settings,
    })
}

/// `tars config`: what the loaded config is, and whether a set of files would load.
#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print each operator setting this process would run with and where it came from: an
    /// owner's default, or the system or user file that set it.
    Explain {
        /// A section (`llm.retry`) or one key (`llm.retry.max_attempts`); all settings when left
        /// out.
        key: Option<String>,
    },
    /// Load a set of config files as a process would and report every problem, without building
    /// a provider or touching the network.
    Check {
        /// The system file to check.
        #[arg(long)]
        system: PathBuf,
        /// A user file to lay over it; none when left out.
        #[arg(long)]
        user: Option<PathBuf>,
    },
}

/// Why `tars config` did not pass.
#[derive(Debug, thiserror::Error)]
pub enum ConfigCommandError {
    #[error(transparent)]
    Load(#[from] LoadConfigError),
    #[error("no operator setting is `{key}`; the sections are {}", sections())]
    UnknownKey { key: String },
    #[error("the files do not load")]
    CheckFailed(#[source] LoadConfigError),
}

impl ConfigCommandError {
    /// Says whether the command did its work before it failed: a check that found a problem did.
    pub(crate) fn ending(&self) -> Ending {
        match self {
            Self::CheckFailed(_) => Ending::Failed,
            Self::Load(_) | Self::UnknownKey { .. } => Ending::NotRun,
        }
    }
}

fn sections() -> String {
    Section::ALL.map(Section::path).join(", ")
}

/// Runs `tars config`; `explain` loads the files `paths` names, `check` the files it is given.
///
/// # Errors
///
/// Fails when the config does not load, when `explain` is asked for a key no setting has, and
/// when `check` finds a problem.
pub(crate) fn execute(args: ConfigArgs, paths: &ConfigPaths) -> Result<(), ConfigCommandError> {
    match args.command {
        ConfigCommand::Explain { key } => {
            let loaded = load(paths)?;
            let lines = explain(&loaded.llm.settings, key.as_deref());
            if let (true, Some(key)) = (lines.is_empty(), key) {
                return Err(ConfigCommandError::UnknownKey { key });
            }
            for line in lines {
                println!("{line}");
            }
        }
        ConfigCommand::Check { system, user } => {
            let loaded = load_layers(&LayerPaths {
                system: Some(system),
                user,
            })
            .map_err(ConfigCommandError::CheckFailed)?;
            println!(
                "ok: {} provider(s), {} operator setting(s)",
                loaded.config.providers().iter().count(),
                loaded.llm.settings.len()
            );
        }
    }
    Ok(())
}

/// Returns the settings `key` names (a section, one key, or all when `None`), one line each.
fn explain(settings: &[EffectiveSetting], key: Option<&str>) -> Vec<String> {
    settings
        .iter()
        .filter(|s| key.is_none_or(|k| s.key == k || s.key.starts_with(&format!("{k}."))))
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tars_config::{FileLayer, SettingSource};

    fn user_only(path: PathBuf) -> ConfigPaths {
        ConfigPaths {
            user: Some(path),
            system: None,
        }
    }

    const MINIMAL_VALID_TOML: &str = r#"
[providers.local_qwen]
type = "openai_compat"
gen_ai_provider_name = "local"
base_url = "http://localhost:8000/v1"
default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
"#;

    #[test]
    fn explicit_missing_path_errors_with_path_in_message() {
        let bogus = PathBuf::from("/definitely/not/a/real/tars/path.toml");
        let err = load(&user_only(bogus.clone())).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"));
        assert!(msg.contains("path.toml"));
    }

    #[test]
    fn explicit_path_to_directory_errors_as_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(&user_only(dir.path().to_path_buf())).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not a regular file"),
            "directory path should be rejected as not-a-regular-file, got: {msg}"
        );
    }

    #[test]
    fn explicit_valid_path_loads_successfully() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{MINIMAL_VALID_TOML}").unwrap();
        let cfg = load(&user_only(f.path().to_path_buf()))
            .expect("valid config should load")
            .config;

        assert_eq!(cfg.user_declared().count(), 1);
    }

    #[test]
    fn explicit_malformed_toml_errors_with_path_context() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "this is = not valid = toml ===").unwrap();
        let err = load(&user_only(f.path().to_path_buf())).unwrap_err();
        let msg = tars_utils::display_chain(&err);
        assert!(
            msg.contains(&f.path().display().to_string()),
            "error should mention the offending path, got: {msg}"
        );
    }

    /// A directory holding a system file and a user file, each written only when given.
    struct Files {
        dir: tempfile::TempDir,
    }

    impl Files {
        fn new(system: Option<&str>, user: Option<&str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for (name, text) in [("system.toml", system), ("config.toml", user)] {
                if let Some(text) = text {
                    std::fs::write(dir.path().join(name), text).unwrap();
                }
            }
            Self { dir }
        }

        fn system(&self) -> PathBuf {
            self.dir.path().join("system.toml")
        }

        fn user(&self) -> PathBuf {
            self.dir.path().join("config.toml")
        }

        fn paths(&self) -> ConfigPaths {
            ConfigPaths {
                user: Some(self.user()),
                system: self.system().exists().then(|| self.system()),
            }
        }
    }

    fn source_of<'a>(llm: &'a LlmSettings, key: &str) -> &'a SettingSource {
        &llm.settings
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("no setting {key}"))
            .source
    }

    fn user_file(files: &Files) -> SettingSource {
        SettingSource::File {
            layer: FileLayer::User,
            path: files.user(),
        }
    }

    #[test]
    fn with_no_operator_section_every_setting_is_its_owners_default() {
        let files = Files::new(None, Some(MINIMAL_VALID_TOML));
        let llm = load(&files.paths()).expect("loads").llm;
        assert_eq!(
            format!("{:?}", llm.retry),
            format!("{:?}", RetryConfig::default())
        );
        assert_eq!(llm.breaker, CircuitBreakerConfig::default());
        assert_eq!(llm.reserve, OutputReserve::default());
        assert!(!llm.settings.is_empty());
        for setting in &llm.settings {
            assert!(
                matches!(setting.source, SettingSource::Default { .. }),
                "{setting}"
            );
        }
        assert!(matches!(
            source_of(&llm, "llm.retry.max_attempts"),
            SettingSource::Default { owner } if owner.ends_with("RetryConfig")
        ));
    }

    #[test]
    fn a_user_retry_key_is_the_users_and_the_rest_stay_defaults() {
        let user = format!("{MINIMAL_VALID_TOML}\n[llm.retry]\nmax_attempts = 5\n");
        let files = Files::new(None, Some(&user));
        let llm = load(&files.paths()).expect("loads").llm;
        assert_eq!(llm.retry.max_attempts, 5);
        assert_eq!(
            source_of(&llm, "llm.retry.max_attempts"),
            &user_file(&files)
        );
        assert!(matches!(
            source_of(&llm, "llm.retry.initial_backoff_ms"),
            SettingSource::Default { .. }
        ));
    }

    #[test]
    fn a_system_cooldown_reaches_the_breaker_with_the_system_file_as_its_source() {
        let files = Files::new(Some("[llm.circuit_breaker]\ncooldown_secs = 60\n"), None);
        let llm = load(&files.paths()).expect("a system file alone loads").llm;
        let breaker_60 = CircuitBreakerConfig::new(
            std::num::NonZeroU32::new(5).expect("5 is not zero"),
            std::time::Duration::from_secs(60),
        )
        .expect("a minute is a cooldown");
        assert_eq!(llm.breaker, breaker_60);
        assert_eq!(
            source_of(&llm, "llm.circuit_breaker.cooldown_secs"),
            &SettingSource::File {
                layer: FileLayer::System,
                path: files.system(),
            }
        );
    }

    #[test]
    fn check_reports_a_misspelt_section_and_a_misspelt_key_together_and_fails() {
        let files = Files::new(
            Some("[llm.retri]\nmax_attempts = 5\n\n[llm.retry]\nmax_attemps = 5\n"),
            None,
        );
        let args = ConfigArgs {
            command: ConfigCommand::Check {
                system: files.system(),
                user: None,
            },
        };
        let err = execute(args, &ConfigPaths::default()).unwrap_err();
        assert_eq!(err.ending(), Ending::Failed);
        let ConfigCommandError::CheckFailed(LoadConfigError::Operator(errors)) = &err else {
            panic!("expected operator errors, got {err:?}");
        };
        let lines: Vec<String> = errors
            .as_slice()
            .iter()
            .map(|e| tars_utils::display_chain(e))
            .collect();
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert!(
            lines.iter().any(|l| l.contains("max_attemps")),
            "{lines:#?}"
        );
        assert!(lines.iter().any(|l| l.contains("llm.retri")), "{lines:#?}");
    }

    #[test]
    fn check_passes_a_system_file_that_loads() {
        let files = Files::new(Some("[llm.http]\nconnect_timeout_ms = 2000\n"), None);
        let args = ConfigArgs {
            command: ConfigCommand::Check {
                system: files.system(),
                user: None,
            },
        };
        execute(args, &ConfigPaths::default()).expect("a good system file passes");
    }

    #[test]
    fn explain_names_the_user_layer_for_a_key_both_layers_set() {
        let files = Files::new(
            Some("[llm.retry]\nmax_attempts = 2\n"),
            Some("[llm.retry]\nmax_attempts = 4\n"),
        );
        let llm = load(&files.paths()).expect("loads").llm;
        let lines = explain(&llm.settings, Some("llm.retry.max_attempts"));
        assert_eq!(
            lines,
            [format!(
                "llm.retry.max_attempts = 4  # user file {}",
                files.user().display()
            )]
        );
    }

    #[test]
    fn explain_takes_a_section_a_key_or_nothing_and_not_a_prefix_of_a_name() {
        let files = Files::new(None, Some(MINIMAL_VALID_TOML));
        let settings = load(&files.paths()).expect("loads").llm.settings;
        let all = explain(&settings, None);
        assert_eq!(all.len(), settings.len());
        let retry = explain(&settings, Some("llm.retry"));
        assert!(!retry.is_empty() && retry.iter().all(|l| l.starts_with("llm.retry.")));
        assert_eq!(explain(&settings, Some("llm.retry.max_attempts")).len(), 1);
        assert!(explain(&settings, Some("llm.re")).is_empty());
    }
}
