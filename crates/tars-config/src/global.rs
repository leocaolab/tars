//! Process-global immutable [`Config`] singleton (Doc 06 — process isolation).
//!
//! One tenant per process, so a single global immutable config is *correct*:
//! [`init_tars`] once at the composition root, [`Config::get`] everywhere,
//! no hot-reload. "Multi-tenant" = N single-tenant processes, each with its own
//! singleton — never a shared-process registry keyed by tenant.
//!
//! Two doors, both **fail on a second call**:
//!
//! - [`init_tars`] — read `<home>/config.toml` off disk and install it. This is
//!   the only initialization tars has: everything downstream (the provider
//!   registry, and any future global) is *derived* from this config, not an
//!   independent fact.
//! - [`Config::set`] — install a [`Config`] the caller already built (an
//!   embedding app that owns its own config discovery, a test).
//!
//! Neither is idempotent-by-silence. A second installer gets
//! [`ConfigError::AlreadyInitialized`] rather than `Ok(())` over a config it
//! never provided — the failure mode where a library, a test, or a second entry
//! point wins the race and everyone else runs against a config they cannot see.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::error::ConfigError;
use crate::manager::{Config, ConfigManager};

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Initialize tars: read `<home>/config.toml` and install it as the
/// process-global [`Config`].
///
/// `home` is the explicit `--tars_home` override; otherwise `$TARS_HOME`;
/// otherwise `~/.tars` (see [`resolve_home`]).
///
/// This is the composition root. It installs the *primary* state — the config
/// value that cannot be reconstructed. Derived state (the provider registry)
/// is built by its own layer's `init`, reading [`Config::get`].
///
/// Errors with [`ConfigError::AlreadyInitialized`] if a config is already
/// installed; the freshly-parsed one is dropped and the caller is told.
pub fn init_tars(home: Option<PathBuf>) -> Result<(), ConfigError> {
    if CONFIG.get().is_some() {
        return Err(ConfigError::AlreadyInitialized);
    }
    let dir = resolve_home(home).ok_or(ConfigError::NoHome)?;
    let cfg = ConfigManager::load_from_file(dir.join("config.toml"))?;
    CONFIG.set(cfg).map_err(|_| ConfigError::AlreadyInitialized)
}

impl Config {
    /// The process-global config.
    ///
    /// Panics if [`init_tars`] / [`Config::set`] has not run — a startup-contract
    /// violation (a programming error, not a runtime condition). The composition
    /// root must install before anything calls `get`. This is the one sanctioned
    /// panic in the config layer (Doc 06 §C1: `get` is infallible by design).
    pub fn get() -> &'static Config {
        CONFIG
            .get()
            .expect("init_tars() must run at startup before Config::get()")
    }

    /// The process-global config if installed — non-panicking, for callers that
    /// must report "the composition root never ran" as a typed error.
    pub fn try_get() -> Option<&'static Config> {
        CONFIG.get()
    }

    /// Whether the global config has been installed.
    pub fn is_loaded() -> bool {
        CONFIG.get().is_some()
    }

    /// Install an already-constructed config as the process global — the DI
    /// entry point.
    ///
    /// Where [`init_tars`] reads `<home>/config.toml` off disk, `set` takes a
    /// [`Config`] the caller already built. Same once-only contract: a second
    /// call errors with [`ConfigError::AlreadyInitialized`] rather than
    /// silently dropping `cfg`.
    pub fn set(cfg: Config) -> Result<(), ConfigError> {
        CONFIG.set(cfg).map_err(|_| ConfigError::AlreadyInitialized)
    }
}

/// Resolve the tars home directory: `--tars_home` flag > `$TARS_HOME` > `~/.tars`.
///
/// Deliberately **not** XDG — matches [`crate::default_config_path`] and the
/// OpenClaw/Hermes `~/.<tool>` convention. Returns `None` only when there is no
/// flag, no `$TARS_HOME`, and no discoverable home directory.
pub fn resolve_home(flag: Option<PathBuf>) -> Option<PathBuf> {
    flag.or_else(|| std::env::var_os("TARS_HOME").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".tars")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_home_flag_wins_over_everything() {
        let explicit = PathBuf::from("/custom/tars/home");
        assert_eq!(resolve_home(Some(explicit.clone())), Some(explicit));
    }

    #[test]
    fn resolve_home_falls_back_to_dot_tars() {
        // Only meaningful when the test env leaves $TARS_HOME unset — otherwise
        // the env layer (correctly) takes precedence and this assertion is moot.
        if std::env::var_os("TARS_HOME").is_none() {
            let home = resolve_home(None).expect("HOME should resolve in test env");
            assert!(
                home.ends_with(".tars"),
                "default home should end with .tars, got {home:?}"
            );
        }
    }

    /// A second install reports rather than silently dropping the caller's
    /// config. Both doors share the one cell, so whichever runs first wins and
    /// the loser is *told* — the property `Config::set(..) -> ()` could not
    /// express.
    #[test]
    fn second_install_is_reported_not_swallowed() {
        // This test owns the global for the whole process; the first call may
        // lose to another test's install, which is itself the condition under
        // test. Assert on the invariant, not on who won.
        let first = Config::set(Config::default());
        let second = Config::set(Config::default());
        assert!(
            matches!(second, Err(ConfigError::AlreadyInitialized)),
            "a second install must error, got {second:?}"
        );
        if first.is_ok() {
            assert!(Config::is_loaded(), "the winning install is visible");
        }
    }
}
