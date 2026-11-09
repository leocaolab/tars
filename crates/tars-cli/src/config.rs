//! Loads the CLI's config: the `--config` path (clap has already merged `$TARS_CONFIG` into it),
//! else [`tars_config::default_config_path`], the file `tars init` writes. Every command that
//! reads a config loads it through here, so they all read the same file.

use std::path::PathBuf;

use tars_config::{Config, ConfigError, default_config_path};

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
    #[error("loading config from {}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: ConfigError,
    },
}

/// Loads the config at `path`, else at the default path.
///
/// # Errors
///
/// Fails when no path is given and no tars home resolves, when the file is missing
/// or not a regular file, or when it does not parse.
pub fn load(path: Option<PathBuf>) -> Result<Config, LoadConfigError> {
    let path = path
        .or_else(default_config_path)
        .ok_or(LoadConfigError::NoHome)?;
    // Not `Path::is_file()`: it reports an unreadable file as absent.
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(LoadConfigError::NotAFile { path }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LoadConfigError::NotFound { path });
        }
        Err(source) => return Err(LoadConfigError::Unreadable { path, source }),
    }
    Config::load_file(&path).map_err(|source| LoadConfigError::Invalid { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        let err = load(Some(bogus.clone())).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"));
        assert!(msg.contains("path.toml"));
    }

    #[test]
    fn explicit_path_to_directory_errors_as_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(Some(dir.path().to_path_buf())).unwrap_err();
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
        let cfg = load(Some(f.path().to_path_buf())).expect("valid config should load");

        assert_eq!(cfg.user_declared().count(), 1);
    }

    #[test]
    fn explicit_malformed_toml_errors_with_path_context() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "this is = not valid = toml ===").unwrap();
        let err = load(Some(f.path().to_path_buf())).unwrap_err();
        let msg = tars_utils::display_chain(&err);
        assert!(
            msg.contains(&f.path().display().to_string()),
            "error should mention the offending path, got: {msg}"
        );
    }
}
