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
            "unable to locate default config directory (is HOME set?)\n\
             pass --config <PATH> to specify explicitly"
        )
    })?;
    if !resolved.is_file() {
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
    use std::io::Write;

    const MINIMAL_VALID_TOML: &str = r#"
[providers.local_qwen]
type = "openai_compat"
base_url = "http://localhost:8000/v1"
default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
"#;

    #[test]
    fn explicit_missing_path_errors_with_path_in_message() {
        let bogus = PathBuf::from("/definitely/not/a/real/tars/path.toml");
        let err = load(Some(bogus.clone())).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"));
        assert!(msg.contains("path.toml"));
    }

    #[test]
    fn explicit_path_to_directory_errors_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(Some(dir.path().to_path_buf())).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not found"),
            "directory path should error like a missing file, got: {msg}"
        );
    }

    #[test]
    fn explicit_valid_path_loads_successfully() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{MINIMAL_VALID_TOML}").unwrap();
        let cfg = load(Some(f.path().to_path_buf())).expect("valid config should load");
        // Post-merge: user provider + ambient builtins. Check the
        // user-declared count, which the loader stamps on Config.
        assert_eq!(cfg.user_provider_ids.len(), 1);
    }

    #[test]
    fn explicit_malformed_toml_errors_with_path_context() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "this is = not valid = toml ===").unwrap();
        let err = load(Some(f.path().to_path_buf())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&f.path().display().to_string()),
            "error should mention the offending path, got: {msg}"
        );
    }
}
