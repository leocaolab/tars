//! Provider-level error model. See Doc 01 §11.
//!
//! These errors are *typed*: every Provider implementation maps its own
//! transport-layer errors into one of these variants so the Pipeline /
//! Runtime layers can make uniform retry / fallback / backoff decisions.

use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    /// Authentication / credentials problem.
    #[error("auth: {0}")]
    Auth(String),

    /// Hit rate limits at the provider; respect `Retry-After` if known.
    #[error("rate limited (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// Per-tenant budget exhausted (raised by middleware, but providers
    /// can also surface it if they're aware of caller's quotas).
    #[error("budget exceeded")]
    BudgetExceeded,

    /// Bad request — malformed payload, unsupported field. Permanent.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Provider declined to answer due to content policy.
    #[error("content filtered (category={category})")]
    ContentFiltered { category: String },

    /// Input exceeds model's context window.
    #[error("context too long: requested {requested}, limit {limit}")]
    ContextTooLong { limit: u32, requested: u32 },

    /// Provider is overloaded — typically 503 / "model overloaded".
    #[error("model overloaded")]
    ModelOverloaded,

    /// Network / transport-level failure.
    #[error("network: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Failed to parse the provider's response.
    #[error("parse: {0}")]
    Parse(String),

    /// CLI subprocess died / returned malformed JSON / non-zero exit.
    #[error("subprocess died: exit={exit_code:?} stderr={stderr}")]
    CliSubprocessDied { exit_code: Option<i32>, stderr: String },

    /// Catch-all for adapter bugs. Should be rare.
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorClass {
    /// Try again with backoff. May succeed.
    Retriable,
    /// Try again, but be conservative — could be a server-side bug or
    /// a parse problem that won't get better on its own.
    MaybeRetriable,
    /// Don't retry. Fix the request / config / wait for human.
    Permanent,
}

impl ProviderError {
    pub fn class(&self) -> ErrorClass {
        use ProviderError::*;
        match self {
            RateLimited { .. } | ModelOverloaded | Network(_) => ErrorClass::Retriable,
            Auth(_) | InvalidRequest(_) | ContextTooLong { .. }
            | ContentFiltered { .. } | BudgetExceeded => ErrorClass::Permanent,
            Parse(_) | Internal(_) | CliSubprocessDied { .. } => {
                ErrorClass::MaybeRetriable
            }
        }
    }

    /// Suggested wait time before retrying. `None` means caller decides.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::ModelOverloaded => Some(Duration::from_secs(10)),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(err: reqwest::Error) -> Self {
        // Defer status-code classification to the adapter — this catch
        // is for genuine network / timeout / decode failures.
        ProviderError::Network(Box::new(err))
    }
}

impl From<serde_json::Error> for ProviderError {
    fn from(err: serde_json::Error) -> Self {
        ProviderError::Parse(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_partition_correctly() {
        assert_eq!(
            ProviderError::RateLimited { retry_after: None }.class(),
            ErrorClass::Retriable
        );
        assert_eq!(
            ProviderError::Auth("bad key".into()).class(),
            ErrorClass::Permanent
        );
        assert_eq!(
            ProviderError::Parse("bad json".into()).class(),
            ErrorClass::MaybeRetriable
        );
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let e = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(e.retry_after(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn overloaded_default_retry_is_10s() {
        let e = ProviderError::ModelOverloaded;
        assert_eq!(e.retry_after(), Some(Duration::from_secs(10)));
    }
}
