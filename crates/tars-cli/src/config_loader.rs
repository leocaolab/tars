//! Resolve where to load config from.
//!
//! Order: explicit `--config <PATH>` flag → `$TARS_CONFIG` env var
//! (already merged into the flag by clap) → XDG default.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tars_config::{Config, ConfigManager};

pub fn default_config_path() -> Option<PathBuf> {
    // dirs::config_dir() respects $XDG_CONFIG_HOME, falls back to
    // ~/.config on Linux, ~/Library/Application Support on macOS,
    // %APPDATA% on Windows.
    dirs::config_dir().map(|d| d.join("tars").join("config.toml"))
}

pub fn load(path: Option<PathBuf>) -> Result<Config> {
    let resolved = path.or_else(default_config_path).ok_or_else(|| {
        anyhow::anyhow!(
            "no config path provided and platform has no XDG-style config dir"
        )
    })?;
    if !resolved.exists() {
        anyhow::bail!(
            "config file not found at {}\n\
             create one or pass --config <PATH>",
            resolved.display(),
        );
    }
    ConfigManager::load_from_file(&resolved)
        .with_context(|| format!("loading config from {}", resolved.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_missing_path_errors_with_path_in_message() {
        let bogus = PathBuf::from("/definitely/not/a/real/tars/path.toml");
        let err = load(Some(bogus.clone())).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"));
        assert!(msg.contains("path.toml"));
    }
}
