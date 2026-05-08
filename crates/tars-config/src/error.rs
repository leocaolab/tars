//! Typed errors for config load / validate paths.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io reading {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Audit `tars-config-src-error-1`: original variant stored
    /// `message: String` and lost the underlying serde / toml error
    /// chain. Storing the boxed source restores `e.source()` walks
    /// (anyhow, miette, … all consume that chain to render context).
    #[error("config parse error in {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Cross-section / cross-key validation failure.
    /// `errors` is a flat list — startup prints all of them rather than
    /// one-at-a-time so users can fix everything in one pass.
    #[error("config validation failed ({})", .errors.len())]
    ValidationFailed { errors: Vec<ValidationError> },

    #[error("provider {id} references unknown auth source: {detail}")]
    BadAuthRef { id: String, detail: String },

    #[error("internal: {0}")]
    Internal(String),
}

impl ConfigError {
    /// Construct a `ValidationFailed` variant, asserting the error list
    /// is non-empty. Callers building this enum from validation results
    /// already short-circuit on an empty list (see `Config::validate`),
    /// so an empty vec here indicates an upstream bug. Enforced in all
    /// builds because this is a `pub` constructor — a release-only escape
    /// would render the confusing "config validation failed (0)".
    pub fn validation_failed(errors: Vec<ValidationError>) -> Self {
        assert!(
            !errors.is_empty(),
            "ConfigError::validation_failed called with no errors — \
             validation success should be reported as Ok(()), not as a failure variant",
        );
        Self::ValidationFailed { errors }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{key}: {message}")]
pub struct ValidationError {
    pub key: String,
    pub message: String,
}

impl ValidationError {
    pub fn new(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
        }
    }
}
