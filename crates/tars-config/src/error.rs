//! Names the ways loading, validating and installing the config fail.

use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io reading {path:?}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config parse error in {path:?}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    /// Carries every validation failure at once, so the operator fixes them all in one pass; its
    /// message lists each one.
    #[error("config validation failed:\n{errors}")]
    ValidationFailed { errors: ValidationErrors },

    /// Means no home was passed, `$TARS_HOME` is unset or empty, and there is no user home
    /// directory.
    #[error("cannot resolve tars home: pass a home, or set $TARS_HOME or $HOME")]
    NoHome,

    /// Means an earlier `init_tars` or `Config::set` already installed the process-global config;
    /// the second installer is told instead of running against a config it did not provide.
    #[error(
        "tars config already initialized — init_tars/Config::set may run only once per process"
    )]
    AlreadyInitialized,

    /// Carries every operator section that did not assemble: one a file set that nobody takes, a
    /// misspelt one, or one whose values its owner refuses.
    #[error("operator sections:\n{0}")]
    Operator(crate::operator::SectionErrors),

    /// Means `<home>/models.json` exists but could not be read, or a catalog was already installed.
    #[error(transparent)]
    Catalog(#[from] crate::model_catalog::CatalogError),
}

/// One config key that breaks a validation rule.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{key}: {problem}")]
pub struct ValidationError {
    pub key: String,
    pub problem: ValidationProblem,
}

impl ValidationError {
    pub fn new(key: impl Into<String>, problem: ValidationProblem) -> Self {
        Self {
            key: key.into(),
            problem,
        }
    }
}

/// The validation rule a key breaks; the words are what the operator reads.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ValidationProblem {
    #[error(
        "references unknown provider `{id}` — add a [providers.{id}] section or fix the role mapping"
    )]
    UnknownProvider { id: String },
    #[error("is empty — a role must name the concrete model it calls")]
    RoleWithoutModel,
    #[error("must not be empty")]
    Empty,
    #[error("must not be empty when set (omit the field to use the default)")]
    EmptyWhenSet,
    #[error("must not be empty — omit the field to use the default search instead")]
    EmptyScriptPath,
    #[error("must be > 0")]
    Zero,
    #[error("must be > 0 when set")]
    ZeroWhenSet,
    #[error(
        "must be <= {max} (24h); larger values are almost certainly a typo — a per-call \
         timeout beyond a day is never intended"
    )]
    TimeoutTooLong { max: u64 },
    #[error(
        "a provider of type `{kind}` requires an api key — auth must be `secret`, as `none` \
         and `delegate` send no key (use openai_compat for a keyless local or proxy endpoint)"
    )]
    KeyRequired {
        kind: crate::providers::ProviderKind,
    },
    #[error("names cassette `{id}`, which makes no call to record — name a provider that does")]
    RecordsFromCassette { id: String },
    /// Means a model id or `<series>@latest` spec does not parse; refused at load so a typo is
    /// not found only when the provider binds.
    #[error(transparent)]
    UnparsableModelSpec(crate::model_spec::ParseModelSpecError),
    #[error("must be set for openai_compat (this distinguishes it from openai)")]
    CompatBaseUrlRequired,
    #[error("must name who serves this endpoint (e.g. \"groq\", \"vllm\")")]
    ServerNameRequired,
}

/// The validation failures of one config, displayed one per line. It is never empty: only
/// [`crate::Config::validate`] builds one, and only when it found a failure.
///
/// ```compile_fail
/// let _ = tars_config::error::ValidationErrors(vec![]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    /// Returns `errors` as one value, or `None` when there are none.
    pub(crate) fn collect(errors: Vec<ValidationError>) -> Option<Self> {
        if errors.is_empty() {
            None
        } else {
            Some(Self(errors))
        }
    }

    pub fn as_slice(&self) -> &[ValidationError] {
        &self.0
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = self.0.iter();
        if let Some(first) = lines.next() {
            write!(f, "  {first}")?;
        }
        for e in lines {
            write!(f, "\n  {e}")?;
        }
        Ok(())
    }
}
