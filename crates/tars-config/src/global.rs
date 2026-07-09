//! Process-global immutable [`Config`] singleton (Doc 06 — process isolation).
//!
//! One tenant per process, so a single global immutable config is *correct*:
//! [`Config::load`] once at the composition root, [`Config::get`] everywhere,
//! no hot-reload. "Multi-tenant" = N single-tenant processes, each with its own
//! singleton — never a shared-process registry keyed by tenant.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::error::ConfigError;
use crate::manager::{Config, ConfigManager};

static CONFIG: OnceLock<Config> = OnceLock::new();

impl Config {
    /// Load the process-global config once, at the composition root.
    ///
    /// `home` is the explicit `--tars_home` override; otherwise `$TARS_HOME`;
    /// otherwise `~/.tars` (see [`resolve_home`]). Reads `<home>/config.toml`.
    /// Idempotent: a second call is a no-op (the first load wins), so multiple
    /// entry points may call it defensively.
    pub fn load(home: Option<PathBuf>) -> Result<(), ConfigError> {
        if CONFIG.get().is_some() {
            return Ok(());
        }
        let dir = resolve_home(home).ok_or(ConfigError::NoHome)?;
        let cfg = ConfigManager::load_from_file(dir.join("config.toml"))?;
        // First writer wins; a lost race just drops our freshly-parsed copy.
        let _ = CONFIG.set(cfg);
        Ok(())
    }

    /// The process-global config.
    ///
    /// Panics if [`Config::load`] has not run — a startup-contract violation
    /// (a programming error, not a runtime condition). The composition root
    /// must `load` before anything calls `get`. This is the one sanctioned
    /// panic in the config layer (Doc 06 §C1: `get` is infallible by design).
    pub fn get() -> &'static Config {
        CONFIG
            .get()
            .expect("Config::load() must run at startup before Config::get()")
    }

    /// Whether the global config has been loaded (for defensive callers/tests).
    pub fn is_loaded() -> bool {
        CONFIG.get().is_some()
    }

    /// Install an already-constructed config as the process global — the DI
    /// entry point.
    ///
    /// Where [`Config::load`] reads `<home>/config.toml` off disk, `set` takes
    /// a [`Config`] value the caller already built (an embedding app that owns
    /// its own config discovery, a test). Same first-wins / double-init
    /// semantics as [`Config::load`]: a second call is a no-op (the first
    /// install wins), so the DI entry and the disk-loading entry compose
    /// without fighting over the singleton.
    pub fn set(cfg: Config) {
        let _ = CONFIG.set(cfg);
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
}
