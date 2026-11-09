//! Holds the process-global [`Config`], installed once and never replaced.
//!
//! A process serves one tenant, so one immutable config is enough; several tenants are several
//! processes. Both ways in, [`init_tars`] (read `<home>/config.toml`) and [`Config::set`] (take a
//! config the caller built), fail on a second call with [`ConfigError::AlreadyInitialized`]:
//! succeeding silently would leave the second installer running against a config it cannot see.
//! The provider registry is built from this config by the composition root that installed it,
//! which keeps it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use thiserror::Error;

use crate::config::Config;
use crate::error::ConfigError;
use crate::model_catalog::{ModelCatalog, install_catalog, load_catalog};
use crate::paths::resolve_home;

static CONFIG: OnceLock<Config> = OnceLock::new();

/// The config and model catalog of one tars home, read but not installed.
#[derive(Debug)]
pub struct LoadedHome {
    pub config: Config,
    pub catalog: ModelCatalog<'static>,
}

/// Reads `<home>/config.toml` and the model catalog with that home's `models.json` laid over the
/// shipped provider data, installing neither: a composition root builds what it needs from them
/// first and calls [`install`] only once all of it succeeded, so a failed start can be retried.
///
/// `home` is the caller's explicit home; without it [`resolve_home`] supplies one.
///
/// # Errors
///
/// Returns [`ConfigError::NoHome`] when no home resolves, and the load, validation or catalog
/// error otherwise.
pub fn load_tars(home: Option<PathBuf>) -> Result<LoadedHome, ConfigError> {
    let dir = resolve_home(home).ok_or(ConfigError::NoHome)?;
    let config = Config::load_file(dir.join("config.toml"))?;
    let catalog = load_catalog(&dir)?;
    Ok(LoadedHome { config, catalog })
}

/// Installs a loaded home's catalog and config as the process globals.
///
/// # Errors
///
/// Returns [`ConfigError::AlreadyInitialized`] when a config is already installed, before
/// installing anything.
pub fn install(loaded: LoadedHome) -> Result<(), ConfigError> {
    if CONFIG.get().is_some() {
        return Err(ConfigError::AlreadyInitialized);
    }
    install_catalog(loaded.catalog)?;
    CONFIG
        .set(loaded.config)
        .map_err(|_| ConfigError::AlreadyInitialized)
}

/// Loads `<home>` as [`load_tars`] does and installs it, for a root with nothing to build from
/// it that can fail.
///
/// # Errors
///
/// Returns [`ConfigError::AlreadyInitialized`] when a config is already installed, before
/// reading anything, and the errors of [`load_tars`] and [`install`] otherwise.
pub fn init_tars(home: Option<PathBuf>) -> Result<(), ConfigError> {
    if CONFIG.get().is_some() {
        return Err(ConfigError::AlreadyInitialized);
    }
    install(load_tars(home)?)
}

impl Config {
    /// Returns the process-global config.
    ///
    /// # Panics
    ///
    /// Panics when neither [`init_tars`] nor [`Config::set`] has run, which is a startup bug.
    pub fn get() -> &'static Config {
        CONFIG
            .get()
            .expect("init_tars() must run at startup before Config::get()")
    }

    /// Returns the process-global config, or `None` before it is installed.
    pub fn try_get() -> Option<&'static Config> {
        CONFIG.get()
    }

    pub fn is_loaded() -> bool {
        CONFIG.get().is_some()
    }

    /// Installs a config the caller built as the process global.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::AlreadyInitialized`] when a config is already installed.
    pub fn set(cfg: Config) -> Result<(), ConfigError> {
        CONFIG.set(cfg).map_err(|_| ConfigError::AlreadyInitialized)
    }
}

/// Why a store path cannot be named.
#[derive(Debug, Error)]
pub enum ResolveStoreDirError {
    #[error(
        "no tars config is installed; run init_tars or Config::set before asking for a store path"
    )]
    ConfigNotInstalled,
    #[error(
        "the installed config has no store_dir; the consumer sets it with Config::with_store_dir"
    )]
    StoreDirUnset,
}

/// Returns the store directory of `config`, the installed config or `None` before one is.
fn store_dir_of(config: Option<&Config>) -> Result<&Path, ResolveStoreDirError> {
    config
        .ok_or(ResolveStoreDirError::ConfigNotInstalled)?
        .store_dir()
        .ok_or(ResolveStoreDirError::StoreDirUnset)
}

// Each store opens its own pool on the paths below; these only name files. A name is listed
// whether or not this repo ships the store that reads it: the layout is the contract a
// consumer's store relies on.

/// Returns `<store_dir>/<file>` of the installed config.
fn store_path(file: &str) -> Result<PathBuf, ResolveStoreDirError> {
    store_dir_of(Config::try_get()).map(|dir| dir.join(file))
}

/// Returns `<store_dir>/events.sqlite`, the agent event log.
///
/// # Errors
///
/// [`ResolveStoreDirError`] when no config is installed or its `store_dir` is unset; so do the
/// other `get_*db_path` functions.
pub fn get_eventdb_path() -> Result<PathBuf, ResolveStoreDirError> {
    store_path("events.sqlite")
}
/// Returns `<store_dir>/llm_records.db`, the raw LLM request and response bodies.
pub fn get_bodydb_path() -> Result<PathBuf, ResolveStoreDirError> {
    store_path("llm_records.db")
}
/// Returns `<store_dir>/pipeline_events.db`, the pipeline events.
pub fn get_pipelinedb_path() -> Result<PathBuf, ResolveStoreDirError> {
    store_path("pipeline_events.db")
}
/// Returns `<store_dir>/durable.sqlite`, the task queue and trajectory durable store.
pub fn durable_store_path() -> Result<PathBuf, ResolveStoreDirError> {
    store_path("durable.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_config() -> Config {
        Config::from_toml("").expect("an empty config with the built-ins validates")
    }

    #[test]
    fn second_install_is_reported_not_swallowed() {
        // Another test may install first, so this asserts only that the second call loses.
        let first = Config::set(empty_config());
        let second = Config::set(empty_config());
        assert!(
            matches!(second, Err(ConfigError::AlreadyInitialized)),
            "a second install must error, got {second:?}"
        );
        if first.is_ok() {
            assert!(Config::is_loaded(), "the winning install is visible");
        }
    }

    #[test]
    fn a_store_path_needs_an_installed_config_with_a_store_dir() {
        assert!(matches!(
            store_dir_of(None),
            Err(ResolveStoreDirError::ConfigNotInstalled)
        ));
        assert!(matches!(
            store_dir_of(Some(&empty_config())),
            Err(ResolveStoreDirError::StoreDirUnset)
        ));
        let config = empty_config().with_store_dir(PathBuf::from("/s"));
        assert_eq!(store_dir_of(Some(&config)).ok(), Some(Path::new("/s")));
    }
}
