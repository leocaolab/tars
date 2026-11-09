//! Defines the classification of a failed LLM call that the call log stores and every layer
//! decides on: its [`ErrorClass`] and its [`LlmErrorKind`]. The errors themselves are
//! tars-llm's.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// The cause goes away with time: a rate limit, an overloaded or unreachable provider.
    Transient,
    /// The cause stays until the request, the configuration or the code changes. A failure is
    /// this until it is shown to go away by itself.
    Permanent,
}

/// The kind of a failed LLM call: one per variant of tars-llm's `ProviderError` and of its
/// `LlmError`, which the call log records under the same names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorKind {
    Auth,
    RateLimited,
    BudgetExceeded,
    InvalidRequest,
    ContentFiltered,
    ContextTooLong,
    ModelOverloaded,
    ServerError,
    RequestTimeout,
    ProviderTimedOut,
    Unavailable,
    Network,
    StreamCut,
    Unfinished,
    Parse,
    CliSubprocessDied,
    TimedOut,
    MalformedToolCall,
    TruncatedToolCall,
    IncompleteToolCall,
    Incompatible,
    ValidationFailed,
    ValidationCancelled,
    BudgetStoreFailed,
    BudgetBalanceCorrupt,
    Cancelled,
    Encode,
    InvalidUrl,
    InvalidHeader,
    DaemonScript,
    Spawn,
    Io,
    DaemonFailed,
    DaemonExited,
    Sandbox,
    BatchItemCanceled,
    BatchItemExpired,
    BatchItemFailed,
    Internal,
}

impl LlmErrorKind {
    /// Returns the snake_case word consumers key on, the same word serde writes.
    pub fn as_str(&self) -> &'static str {
        use LlmErrorKind::*;
        match self {
            Auth => "auth",
            RateLimited => "rate_limited",
            BudgetExceeded => "budget_exceeded",
            InvalidRequest => "invalid_request",
            ContentFiltered => "content_filtered",
            ContextTooLong => "context_too_long",
            ModelOverloaded => "model_overloaded",
            ServerError => "server_error",
            RequestTimeout => "request_timeout",
            ProviderTimedOut => "provider_timed_out",
            Unavailable => "unavailable",
            Network => "network",
            StreamCut => "stream_cut",
            Unfinished => "unfinished",
            Parse => "parse",
            CliSubprocessDied => "cli_subprocess_died",
            TimedOut => "timed_out",
            MalformedToolCall => "malformed_tool_call",
            TruncatedToolCall => "truncated_tool_call",
            IncompleteToolCall => "incomplete_tool_call",
            Incompatible => "incompatible",
            ValidationFailed => "validation_failed",
            ValidationCancelled => "validation_cancelled",
            BudgetStoreFailed => "budget_store_failed",
            BudgetBalanceCorrupt => "budget_balance_corrupt",
            Cancelled => "cancelled",
            Encode => "encode",
            InvalidUrl => "invalid_url",
            InvalidHeader => "invalid_header",
            DaemonScript => "daemon_script",
            Spawn => "spawn",
            Io => "io",
            DaemonFailed => "daemon_failed",
            DaemonExited => "daemon_exited",
            Sandbox => "sandbox",
            BatchItemCanceled => "batch_item_canceled",
            BatchItemExpired => "batch_item_expired",
            BatchItemFailed => "batch_item_failed",
            Internal => "internal",
        }
    }
}

impl std::fmt::Display for LlmErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for LlmErrorKind {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
