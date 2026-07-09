//! Provider-level error model. See Doc 01 §11.
//!
//! These errors are *typed*: every Provider implementation maps its own
//! transport-layer errors into one of these variants so the Pipeline /
//! Runtime layers can make uniform retry / fallback / backoff decisions.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::validation::ValidationReason;

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

    /// Circuit breaker is open for this provider — recent failure rate
    /// crossed the configured threshold. The breaker rejects calls
    /// without contacting the provider until `until`.
    /// Class is Retriable so a fallback chain (a caller composing
    /// several `LlmService`s) skips to the next candidate immediately.
    /// Doc 02 §4.7.
    #[error("circuit open until {until:?}")]
    CircuitOpen { until: std::time::Instant },

    /// Network / transport-level failure.
    #[error("network: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Failed to parse the provider's response.
    #[error("parse: {0}")]
    Parse(String),

    /// CLI subprocess died / returned malformed JSON / non-zero exit.
    #[error("subprocess died: exit={exit_code:?} stderr={stderr}")]
    CliSubprocessDied {
        exit_code: Option<i32>,
        stderr: String,
    },

    /// Model emitted a tool_use for a tool that isn't registered.
    /// Surfaced by the Session auto-loop when it can't find a handler
    /// for the model's chosen tool name. **Permanent class** — retrying
    /// is futile because the model will keep emitting the same name;
    /// the caller's ToolRegistry needs the missing tool added (or the
    /// system prompt updated to stop suggesting it).
    #[error("model called unknown tool: {name}")]
    UnknownTool { name: String },

    /// `ValidationMiddleware` rejected the response from an
    /// `OutputValidator::Reject` outcome. Always classified as
    /// `ErrorClass::Permanent` — `RetryMiddleware` never retries on
    /// validation failures (same prompt → same output; model retry
    /// is a gamble that doesn't belong inside the runtime). Callers
    /// that need to re-ask the model with prompt variation should
    /// catch `ValidationFailed` at their own layer.
    ///
    /// `reason` is a typed [`ValidationReason`] (B-20.v2); its
    /// `Display` reproduces the message string while `reason.kind()` +
    /// structured detail let callers branch programmatically.
    #[error("validation failed: {validator}: {reason}")]
    ValidationFailed {
        validator: String,
        reason: ValidationReason,
    },

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
            RateLimited { .. } | ModelOverloaded | Network(_) | CircuitOpen { .. } => {
                ErrorClass::Retriable
            }
            Auth(_)
            | InvalidRequest(_)
            | ContextTooLong { .. }
            | ContentFiltered { .. }
            | BudgetExceeded
            | UnknownTool { .. }
            | ValidationFailed { .. } => ErrorClass::Permanent,
            Parse(_) | Internal(_) | CliSubprocessDied { .. } => ErrorClass::MaybeRetriable,
        }
    }

    /// Suggested wait time before retrying. `None` means caller decides.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::ModelOverloaded => Some(Duration::from_secs(10)),
            Self::CircuitOpen { until } => until
                .checked_duration_since(std::time::Instant::now())
                .or(Some(Duration::ZERO)),
            _ => None,
        }
    }

    /// Typed discriminator — the variant of this error as a value type
    /// (no payload), useful for: trigger matching (a fallback chain
    /// deciding whether to try the next provider), telemetry
    /// (`RetryAttempt.error_kind`),
    /// Python-side classification (`TarsProviderError.kind`), and the
    /// `LlmCallFinished` event's `CallResult::Error { kind }`. Round-
    /// trips to the same snake_case string everywhere via serde and
    /// [`ProviderErrorKind::as_str`].
    pub fn kind(&self) -> ProviderErrorKind {
        use ProviderError::*;
        use ProviderErrorKind as K;
        match self {
            Auth(_) => K::Auth,
            RateLimited { .. } => K::RateLimited,
            BudgetExceeded => K::BudgetExceeded,
            InvalidRequest(_) => K::InvalidRequest,
            ContentFiltered { .. } => K::ContentFiltered,
            ContextTooLong { .. } => K::ContextTooLong,
            ModelOverloaded => K::ModelOverloaded,
            CircuitOpen { .. } => K::CircuitOpen,
            Network(_) => K::Network,
            Parse(_) => K::Parse,
            CliSubprocessDied { .. } => K::CliSubprocessDied,
            UnknownTool { .. } => K::UnknownTool,
            ValidationFailed { .. } => K::ValidationFailed,
            Internal(_) => K::Internal,
        }
    }
}

/// Discriminator companion to [`ProviderError`]. One variant per
/// `ProviderError` variant; serializes as the snake_case wire string
/// every consumer already used — telemetry / Python / event store —
/// so making this typed is backward-compatible at the JSON wire and
/// breaks only direct Rust `.kind()` callers (who were passing
/// `&'static str` around).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Auth,
    RateLimited,
    BudgetExceeded,
    InvalidRequest,
    ContentFiltered,
    ContextTooLong,
    ModelOverloaded,
    CircuitOpen,
    Network,
    Parse,
    CliSubprocessDied,
    UnknownTool,
    ValidationFailed,
    Internal,
}

impl ProviderErrorKind {
    /// Snake-case wire form — the string every existing consumer
    /// (telemetry, Python, `LlmCallFinished`) is keyed on. Kept in
    /// sync with the serde `rename_all = "snake_case"` annotation
    /// above: changing one without the other would break round-trips.
    pub fn as_str(&self) -> &'static str {
        use ProviderErrorKind::*;
        match self {
            Auth => "auth",
            RateLimited => "rate_limited",
            BudgetExceeded => "budget_exceeded",
            InvalidRequest => "invalid_request",
            ContentFiltered => "content_filtered",
            ContextTooLong => "context_too_long",
            ModelOverloaded => "model_overloaded",
            CircuitOpen => "circuit_open",
            Network => "network",
            Parse => "parse",
            CliSubprocessDied => "cli_subprocess_died",
            UnknownTool => "unknown_tool",
            ValidationFailed => "validation_failed",
            Internal => "internal",
        }
    }
}

impl std::fmt::Display for ProviderErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for ProviderErrorKind {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(err: reqwest::Error) -> Self {
        // A blanket map to `Network` (Retriable) is wrong for HTTP
        // *status* errors: a 4xx (e.g. 400/401) is permanent, and
        // 429/503 carry their own retry semantics. Misclassifying a
        // 400 as a retriable network blip would make `RetryMiddleware`
        // pointlessly re-send a request that can never succeed.
        if let Some(status) = err.status() {
            return match status.as_u16() {
                429 => ProviderError::RateLimited { retry_after: None },
                503 => ProviderError::ModelOverloaded,
                401 | 403 => ProviderError::Auth(err.to_string()),
                400 | 404 | 405 | 409 | 422 => ProviderError::InvalidRequest(err.to_string()),
                // Other 5xx: treat as overloaded/transient server-side.
                s if s >= 500 => ProviderError::ModelOverloaded,
                // Remaining 4xx are client errors — permanent.
                s if s >= 400 => ProviderError::InvalidRequest(err.to_string()),
                // Shouldn't happen (status() implies an error response),
                // but fall back to network rather than panicking.
                _ => ProviderError::Network(Box::new(err)),
            };
        }
        // No status → genuine transport failure (connect / timeout /
        // decode). Retriable network class is correct here.
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
