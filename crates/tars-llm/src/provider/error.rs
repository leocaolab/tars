//! Defines the errors an LLM call fails with. Every provider maps its transport's errors onto
//! [`ProviderError`], so the pipeline and runtime make one set of retry, fallback and backoff
//! decisions from its [`ErrorClass`] and [`LlmErrorKind`].

use std::path::PathBuf;
use std::time::Duration;

use reqwest::header::InvalidHeaderValue;
use tars_types::providers::cache::CacheHandleError;
use tars_types::{
    BatchJobId, ErrorClass, JsonKind, LlmErrorKind, MalformedArguments, ParseBatchItemIdError,
    ParseBatchJobIdError, ProviderId, RECORD_RAW_MAX, Reason,
};
use tars_utils::display_chain;
use thiserror::Error;

use crate::provider::auth::AuthError;
use crate::provider::batch::BatchStatus;
use crate::provider::schema_adapt::SchemaAdaptError;

#[derive(Debug, Error)]
pub enum ProviderError {
    /// The credential was refused or could not be put on the request.
    #[error("auth")]
    Auth(#[source] CredentialError),

    /// The provider rate-limited the call; `retry_after` is its `Retry-After` when it sent one.
    #[error("rate limited (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// The provider refused the request as malformed or unsupported, or tars refused to send it.
    #[error("invalid request")]
    InvalidRequest(#[source] RequestRefusal),

    /// The provider declined to answer under its content policy.
    #[error("content filtered (category={category})")]
    ContentFiltered { category: String },

    /// The input exceeds the model's context window. `message` is the provider's own words; the
    /// counts are there only when the provider states them in a form the adapter reads.
    #[error("context too long: {message}")]
    ContextTooLong {
        limit: Option<u32>,
        requested: Option<u32>,
        message: String,
    },

    /// The provider said it is overloaded (a 503, Anthropic's 529 and `overloaded_error`).
    #[error("model overloaded")]
    ModelOverloaded,

    /// The provider failed on its side: a 5xx other than 503, or a server failure it reported in
    /// an open stream or as an SDK exception. `status` is the HTTP status when the failure came
    /// as one; an error event in an open stream and an SDK exception have none.
    #[error("server error{}: {message}", status.map(|s| format!(" {s}")).unwrap_or_default())]
    ServerError {
        status: Option<u16>,
        message: String,
    },

    /// The provider timed out waiting for the request to arrive (a 408); the same request may be
    /// sent again (RFC 9110).
    #[error("request timeout: {0}")]
    RequestTimeout(String),

    /// The provider stopped working on the request at its own time limit, before answering
    /// (Anthropic's `timeout_error` on a 500). Unlike [`TimedOut`](Self::TimedOut), tars did not
    /// abort it; the same request reaches the same limit.
    #[error("provider timed out: {0}")]
    ProviderTimedOut(String),

    /// The provider kept failing and is not called before `until`. `last_failure` is the
    /// failure that took it out.
    #[error(
        "provider unavailable after {}: {}",
        last_failure.kind,
        last_failure.message
    )]
    Unavailable {
        until: std::time::Instant,
        last_failure: LastFailure,
    },

    #[error("network")]
    Network(#[source] NetworkError),

    /// The provider's response did not parse. `at` names the document, `line` the line of a
    /// line-delimited one (a batch results file), and `raw` the text that failed, cut to a bound.
    #[error(
        "parse: {at}{}{}",
        line.map(|n| format!(" line {n}")).unwrap_or_default(),
        raw.as_ref().map(|r| format!(" (raw: {r})")).unwrap_or_default()
    )]
    Parse {
        at: &'static str,
        line: Option<usize>,
        #[source]
        problem: Box<ParseProblem>,
        raw: Option<String>,
    },

    /// A CLI subprocess died or exited non-zero.
    #[error("subprocess died: exit={exit_code:?} stderr={stderr}")]
    CliSubprocessDied {
        exit_code: Option<i32>,
        stderr: String,
    },

    /// tars aborted the call when it exceeded its wall-clock budget, killing the subprocess or
    /// dropping the HTTP request; unlike [`CliSubprocessDied`](Self::CliSubprocessDied), the child
    /// did not fail on its own.
    ///
    /// `budget` is the caller's remaining deadline when it set one, else the provider's
    /// `timeout_secs`; `detail` is what the resource said on the way down (stderr tail, model,
    /// prompt size). It is permanent: the same call against the same budget would time out again.
    #[error("timed out after {budget:?}: {detail}")]
    TimedOut { budget: Duration, detail: String },

    /// The model sent a tool call whose arguments are not a JSON object. The model's output is
    /// wrong, not the provider: it is permanent, a circuit breaker does not count it, and `raw` is
    /// the arguments text as received, for a caller that shows it back to the model.
    #[error("model sent malformed arguments for tool `{name}` (call {id}, {} bytes)", raw.len())]
    MalformedToolCall {
        id: String,
        name: String,
        raw: String,
        #[source]
        problem: MalformedArguments,
    },

    /// The output reached the request's `max_output_tokens` inside a tool call, so its arguments
    /// are cut off; `raw` is the part received. The model's call was not wrong: the same request
    /// is cut again, and a caller raises the limit or asks for less. It is permanent, and a
    /// circuit breaker does not count it.
    #[error("output hit max_output_tokens inside tool call `{name}` (call {id}) after {} bytes of arguments", raw.len())]
    TruncatedToolCall {
        id: String,
        name: String,
        raw: String,
    },

    /// The stream ended without the turn's stop reason while a tool call's arguments were not
    /// JSON, so whether the output limit cut them or the model wrote them wrong is not known.
    /// `raw` is the part received, empty when the body ended inside a call before any of its
    /// arguments came. The call's block may have closed or not: the body ended cleanly before the
    /// stop reason, or the server never sends one. A retry may get the whole answer.
    #[error("stream ended before the stop reason with tool call `{name}` (call {id}) unparsed after {} bytes of arguments", raw.len())]
    IncompleteToolCall {
        id: String,
        name: String,
        raw: String,
    },

    /// The response body ended before the protocol's end of the stream (Anthropic `message_stop`,
    /// OpenAI `[DONE]`, Gemini's finish reason): the connection was closed under it. `open_call`
    /// is the first tool call it left open, with the arguments received.
    #[error("the response body ended before the end of the stream{}", cut_inside(open_call.as_ref()))]
    StreamCut { open_call: Option<ReceivedToolCall> },

    /// The stream ended without becoming a response: no `Finished` came before its end and no
    /// error either, or it finished with tool calls left open. The reply may be cut short. It is
    /// permanent: a server that sends no stop reason (some OpenAI-compatible proxies drop
    /// `finish_reason`) sends none again.
    #[error(transparent)]
    Unfinished(#[from] tars_types::Unfinished),

    /// The stream's tool-call events came in an order no response folds from: the adapter or the
    /// provider broke the event protocol. It is permanent.
    #[error(transparent)]
    StreamViolation(#[from] tars_types::StreamViolation),

    /// The provider stated in its `x-should-retry` header a class other than `error`'s own, and
    /// its statement decides the class; [`ProviderError::stated`] builds it only then.
    #[error("the provider says it is {}", class_word(*class))]
    Stated {
        class: ErrorClass,
        #[source]
        error: Box<ProviderError>,
    },

    /// tars could not encode the request body it built.
    #[error("the request body does not encode")]
    Encode(#[source] serde_json::Error),

    /// A URL built from the configured base URL does not parse.
    #[error("`{url}` is not a valid URL")]
    InvalidUrl {
        url: String,
        #[source]
        source: url::ParseError,
    },

    /// A configured value is not valid in the header it travels in.
    #[error("the configured `{header}` is not a valid header value")]
    InvalidHeader {
        header: &'static str,
        #[source]
        source: InvalidHeaderValue,
    },

    /// A header the provider's config adds does not make a valid one, so the request was not
    /// sent. Its value is not kept, since it may be a credential.
    #[error("a configured extra header")]
    ExtraHeader(#[source] crate::provider::http_base::ExtraHeaderError),

    /// A recording cassette got the provider's answer but could not write its file; the
    /// recording is kept in memory and written again when the cassette is dropped.
    #[error("the cassette recording was not written")]
    CassetteNotWritten(#[source] crate::provider::backends::cassette::WriteCassetteError),

    #[error("the claude_sdk daemon script")]
    DaemonScript(#[source] DaemonScriptError),

    /// The child process did not start. Its class is the OS error's: a `fork` refused for want
    /// of a process (`EAGAIN`) passes on another try, a missing program does not.
    #[error("could not start `{}`", program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// An I/O a CLI or daemon call depends on failed; its class is the OS error's.
    #[error("{op}")]
    Io {
        op: IoOp,
        #[source]
        source: std::io::Error,
    },

    /// The claude_sdk daemon answered the request with an error, in the SDK's words. The SDK
    /// does not say which kind it is (a rate limit, an overload, a lost connection), so it is
    /// transient: a call it refuses for good fails again within the retry budget.
    #[error("the claude_sdk daemon failed the request: {message}")]
    DaemonFailed { message: String },

    /// The claude_sdk daemon's stdout ended before it replied to the request.
    #[error("the claude_sdk daemon exited before it replied")]
    DaemonExited,

    /// The jail a CLI delegate runs in could not be built, so the delegate is not run.
    #[error("the CLI delegate's sandbox")]
    Sandbox(#[source] SandboxSetupError),

    /// The batch was cancelled before the provider processed this item.
    #[error("the batch was cancelled before this item ran")]
    BatchItemCanceled,

    /// The batch expired before the provider processed this item; a new batch may run it.
    #[error("the batch expired before this item ran")]
    BatchItemExpired,

    /// A batch item failed with an error code this build does not map, in the provider's words;
    /// `code` is `None` when the provider named none.
    #[error("the batch item failed{}: {message}", code.as_deref().map(|c| format!(" ({c})")).unwrap_or_default())]
    BatchItemFailed {
        code: Option<String>,
        message: String,
    },

    /// A bug in tars: an invariant of its own code broke.
    #[error("internal: {0}")]
    Internal(String),
}

/// Why the claude_sdk daemon script could not be found.
#[derive(Debug, Error)]
pub enum DaemonScriptError {
    #[error(
        "no `script_path` is configured and none of the default locations exist \
         (`tools/claude-daemon/server.mjs` walking up from the working directory, \
         `~/.tars/claude-daemon/server.mjs`)"
    )]
    NotFound,

    #[error("the script path {} is not UTF-8", path.display())]
    NotUtf8 { path: PathBuf },
}

/// An I/O a CLI or daemon call does on the way to its child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOp {
    WriteSystemPromptFile,
    WriteChildStdin,
    FlushChildStdin,
}

impl std::fmt::Display for IoOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::WriteSystemPromptFile => "writing the system prompt file",
            Self::WriteChildStdin => "writing to the child's stdin",
            Self::FlushChildStdin => "flushing the child's stdin",
        })
    }
}

/// Why the jail a CLI delegate runs in could not be built.
#[derive(Debug, Error)]
pub enum SandboxSetupError {
    /// No worktree was given and the process working directory, the jail root then, does not
    /// read.
    #[error("no worktree was given and the working directory does not read")]
    NoWorkingDirectory(#[source] std::io::Error),

    #[cfg(feature = "cli")]
    #[error("the policy cannot be built")]
    Policy(#[source] tars_sandbox::SandboxError),
}

/// A tool call a stream left open when it was cut, with the arguments received before the cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedToolCall {
    pub id: String,
    pub name: String,
    pub raw: String,
}

/// A refusal in the provider's own words: an error response, or an error event in an open stream
/// or in a batch item. `status` is the HTTP status when the refusal came as one.
#[derive(Debug, Error)]
#[error("the provider refused it{}: {message}", status.map(|s| format!(" with status {s}")).unwrap_or_default())]
pub struct ProviderRefusal {
    pub status: Option<u16>,
    pub message: String,
}

/// Why a credential did not get the request through.
#[derive(Debug, Error)]
pub enum CredentialError {
    #[error(transparent)]
    Refused(ProviderRefusal),

    #[error("the credential did not resolve")]
    Resolve(#[source] AuthError),

    /// The credential is not a valid value for the header it travels in.
    #[error("the credential is not a valid `{header}` header value")]
    Header {
        header: &'static str,
        #[source]
        source: InvalidHeaderValue,
    },

    #[error("the provider takes {expected} credentials; it got {got}")]
    Kind {
        expected: CredentialKind,
        got: CredentialKind,
    },

    #[error("the API key is empty")]
    EmptyKey,
}

/// The kind of a resolved credential, as a provider expects or receives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    ApiKey,
    Bearer,
    None,
}

impl std::fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ApiKey => "API-key",
            Self::Bearer => "bearer",
            Self::None => "none",
        })
    }
}

/// Why a request is invalid: the provider said so, or tars refused to send it.
#[derive(Debug, Error)]
pub enum RequestRefusal {
    #[error(transparent)]
    Provider(ProviderRefusal),

    #[error("the request has no messages")]
    EmptyMessages,

    #[error("max_output_tokens must be > 0")]
    ZeroMaxOutputTokens,

    #[error("the tool name `{name}` appears twice")]
    DuplicateToolName { name: String },

    #[error("the tool name `{name}` is reserved for structured-output emulation")]
    ReservedToolName { name: String },

    #[error("tool_choice names `{name}`, which is not in the tools list")]
    UnknownToolChoice { name: String },

    #[error("tool_choice names no tool")]
    EmptyToolChoiceName,

    /// The backend sends a tool result under the name of its call, read from a `<name>@<id>`
    /// tool-call id.
    #[error("the tool result's tool_call_id `{tool_call_id}` is not `<name>@<id>`")]
    ToolCallIdWithoutName { tool_call_id: String },

    #[error("a user message has no content")]
    EmptyUserMessage,

    #[error("the request has no user-visible content")]
    NoContent,

    #[error("tools `{first}` and `{second}` both travel as `{wire}`")]
    WireNameCollision {
        first: String,
        second: String,
        wire: String,
    },

    #[error("a prompt of {bytes} bytes does not fit in one {cli} argument ({limit} bytes at most)")]
    PromptTooLongForArgument {
        bytes: usize,
        limit: usize,
        cli: &'static str,
    },

    #[error("this backend does not support {0}")]
    Unsupported(Feature),

    #[error("tool `{tool}`'s parameter schema cannot be adapted")]
    ToolSchema {
        tool: String,
        #[source]
        source: SchemaAdaptError,
    },

    #[error("the response schema cannot be adapted")]
    ResponseSchema(#[source] SchemaAdaptError),

    #[error("the cached-content handle is refused")]
    CacheHandle(#[source] CacheHandleError),

    #[error("the batch has no items")]
    EmptyBatch,

    #[error("no batch job has id {id}")]
    UnknownBatchJob { id: BatchJobId },

    #[error("batch job {id} is not yet terminal (status: {status:?})")]
    BatchNotTerminal { id: BatchJobId, status: BatchStatus },

    #[error("cassette {cassette} has no recording of request fp={fingerprint} for model {model}")]
    NoRecording {
        cassette: ProviderId,
        fingerprint: String,
        model: String,
    },
}

/// A feature a backend does not offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    Batch,
    BatchCancel,
    /// A `system` message in the message list; such a backend takes system text only in
    /// `ChatRequest.system`.
    SystemMessage,
    /// Counting a request's tokens with the model's tokenizer rather than estimating them.
    ExactTokenCount,
    /// An image in a tool result; such a backend sends a tool result as text only.
    ImageInToolResult,
}

impl std::fmt::Display for Feature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Batch => "batch submission (submit the requests one by one)",
            Self::BatchCancel => "cancelling a batch",
            Self::SystemMessage => {
                "a `system` message role (put system text in `ChatRequest.system`)"
            }
            Self::ExactTokenCount => "counting tokens exactly (it only estimates them)",
            Self::ImageInToolResult => "an image in a tool result (it sends tool results as text)",
        })
    }
}

/// A failure of the connection to the provider, below any response it gave.
#[derive(Debug, Error)]
pub enum NetworkError {
    #[error(transparent)]
    Http(reqwest::Error),

    #[error(transparent)]
    Stream(eventsource_stream::EventStreamError<reqwest::Error>),

    #[error("no SSE chunk received within {0:?}")]
    Idle(Duration),
}

/// What was wrong with a provider's response that did not parse.
#[derive(Debug, Error)]
pub enum ParseProblem {
    #[error("not JSON")]
    Json(#[source] serde_json::Error),

    /// The body did not read, or did not decode as JSON; reqwest does not tell the two apart.
    #[error("the body did not read as JSON")]
    JsonBody(#[source] reqwest::Error),

    #[error("missing `{0}`")]
    Missing(&'static str),

    #[error("unknown `{field}` value {value:?}")]
    UnexpectedValue { field: &'static str, value: String },

    #[error("expected a JSON {expected}, got {got}")]
    WrongType { expected: JsonKind, got: JsonKind },

    #[error("block index {0} exceeds usize")]
    IndexOverflow(u64),

    #[error("the batch item id does not parse")]
    ItemId(#[source] ParseBatchItemIdError),

    #[error("the batch job id does not parse")]
    JobId(#[source] ParseBatchJobIdError),

    #[error("tool call index {index} ended before it started")]
    UnstartedToolCall { index: usize },

    /// The call cannot be routed back without both, and most providers refuse the follow-up
    /// tool-result message.
    #[error("tool call index {index} ended with an empty id or name (id=`{id}`, name=`{name}`)")]
    UnidentifiedToolCall {
        index: usize,
        id: String,
        name: String,
    },
}

fn cut_inside(call: Option<&ReceivedToolCall>) -> String {
    call.map(|c| {
        format!(
            " inside tool call `{}` (call {}) after {} bytes of arguments",
            c.name,
            c.id,
            c.raw.len()
        )
    })
    .unwrap_or_default()
}

fn class_word(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "transient",
        ErrorClass::Permanent => "permanent",
    }
}

/// The failure that made a provider unavailable, in its kind and its own words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastFailure {
    pub kind: LlmErrorKind,
    pub message: String,
}

impl LastFailure {
    pub fn of(e: &ProviderError) -> Self {
        Self {
            kind: e.kind(),
            message: display_chain(e),
        }
    }
}

impl ProviderError {
    /// Returns whether the cause of the failure goes away with time.
    pub fn class(&self) -> ErrorClass {
        use ProviderError::*;
        match self {
            Stated { class, .. } => *class,
            RateLimited { .. }
            | ModelOverloaded
            | ServerError { .. }
            | RequestTimeout(_)
            | Network(_)
            | StreamCut { .. }
            | DaemonFailed { .. }
            | BatchItemExpired
            | Unavailable { .. } => ErrorClass::Transient,
            Spawn { source, .. } | Io { source, .. } => io_class(source.kind()),
            Auth(_)
            | ProviderTimedOut(_)
            | InvalidRequest(_)
            | ContextTooLong { .. }
            | ContentFiltered { .. }
            | MalformedToolCall { .. }
            | TruncatedToolCall { .. }
            | Parse { .. }
            | Internal(_)
            | Encode(_)
            | InvalidUrl { .. }
            | InvalidHeader { .. }
            | ExtraHeader(_)
            | CassetteNotWritten(_)
            | DaemonScript(_)
            | DaemonExited
            | Sandbox(_)
            | BatchItemCanceled
            | BatchItemFailed { .. }
            | CliSubprocessDied { .. }
            | TimedOut { .. }
            | IncompleteToolCall { .. }
            | Unfinished(_)
            | StreamViolation(_) => ErrorClass::Permanent,
        }
    }

    /// Returns how long the provider is unavailable for, when it said so (a `Retry-After`) or
    /// its breaker knows; `None` when nothing says.
    pub fn unavailable_for(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::Unavailable { until, .. } => until
                .checked_duration_since(std::time::Instant::now())
                .or(Some(Duration::ZERO)),
            Self::Stated { error, .. } => error.unavailable_for(),
            _ => None,
        }
    }

    /// Returns the error with the class the provider stated, `class`, deciding its class: itself
    /// when the provider stated none or the one it already has.
    pub fn stated(self, class: Option<ErrorClass>) -> Self {
        match class {
            Some(class) if class != self.class() => Self::Stated {
                class,
                error: Box::new(self),
            },
            _ => self,
        }
    }

    /// Returns the error's kind without its payload, which fallback triggers, telemetry,
    /// tars-py and the call log key on.
    pub fn kind(&self) -> LlmErrorKind {
        use LlmErrorKind as K;
        use ProviderError::*;
        match self {
            Auth(_) => K::Auth,
            RateLimited { .. } => K::RateLimited,
            InvalidRequest(_) => K::InvalidRequest,
            ContentFiltered { .. } => K::ContentFiltered,
            ContextTooLong { .. } => K::ContextTooLong,
            ModelOverloaded => K::ModelOverloaded,
            ServerError { .. } => K::ServerError,
            RequestTimeout(_) => K::RequestTimeout,
            ProviderTimedOut(_) => K::ProviderTimedOut,
            Unavailable { .. } => K::Unavailable,
            Network(_) => K::Network,
            StreamCut { .. } => K::StreamCut,
            Unfinished(_) => K::Unfinished,
            StreamViolation(_) => K::StreamViolation,
            Stated { error, .. } => error.kind(),
            Parse { .. } => K::Parse,
            CliSubprocessDied { .. } => K::CliSubprocessDied,
            TimedOut { .. } => K::TimedOut,
            MalformedToolCall { .. } => K::MalformedToolCall,
            TruncatedToolCall { .. } => K::TruncatedToolCall,
            IncompleteToolCall { .. } => K::IncompleteToolCall,
            Encode(_) => K::Encode,
            InvalidUrl { .. } => K::InvalidUrl,
            InvalidHeader { .. } => K::InvalidHeader,
            ExtraHeader(_) => K::InvalidHeader,
            CassetteNotWritten(_) => K::Io,
            DaemonScript(_) => K::DaemonScript,
            Spawn { .. } => K::Spawn,
            Io { .. } => K::Io,
            DaemonFailed { .. } => K::DaemonFailed,
            DaemonExited => K::DaemonExited,
            Sandbox(_) => K::Sandbox,
            BatchItemCanceled => K::BatchItemCanceled,
            BatchItemExpired => K::BatchItemExpired,
            BatchItemFailed { .. } => K::BatchItemFailed,
            Internal(_) => K::Internal,
        }
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(err: reqwest::Error) -> Self {
        // An HTTP status error is not a network blip: a 4xx is permanent, so mapping it to
        // transient `Network` would re-send a request that cannot succeed.
        match err.status() {
            Some(status) => status_error(status.as_u16(), err.to_string(), None),
            // Without a status it is a transport failure (connect, timeout, decode).
            None => ProviderError::Network(NetworkError::Http(err)),
        }
    }
}

/// Returns the error an HTTP error status means, for a provider that gives the status no
/// meaning of its own; `retry_after` is the response's `Retry-After`, for a 429.
pub(crate) fn status_error(
    status: u16,
    message: String,
    retry_after: Option<Duration>,
) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth(CredentialError::Refused(ProviderRefusal {
            status: Some(status),
            message,
        })),
        408 => ProviderError::RequestTimeout(message),
        429 => ProviderError::RateLimited { retry_after },
        503 => ProviderError::ModelOverloaded,
        500..=599 => ProviderError::ServerError {
            status: Some(status),
            message,
        },
        _ => ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal {
            status: Some(status),
            message,
        })),
    }
}

/// Returns the class of an OS error: `EAGAIN` (`WouldBlock`), an interrupted call, a timed-out
/// one and a pipe whose reader went away (the next call starts a new child) pass on another try.
fn io_class(kind: std::io::ErrorKind) -> ErrorClass {
    use std::io::ErrorKind as K;
    match kind {
        K::WouldBlock | K::Interrupted | K::TimedOut | K::BrokenPipe => ErrorClass::Transient,
        _ => ErrorClass::Permanent,
    }
}

/// Returns the class an `x-should-retry` header value states: `"true"` says the same request
/// may succeed later, `"false"` that it will not; any other value states nothing.
pub(crate) fn stated_class(should_retry: Option<&str>) -> Option<ErrorClass> {
    match should_retry {
        Some("true") => Some(ErrorClass::Transient),
        Some("false") => Some(ErrorClass::Permanent),
        _ => None,
    }
}

/// Says what a failed LLM call means to the step that made it. The error's own class decides
/// whether the step may run again; only a permanent failure is told apart further, by who can
/// fix it.
impl From<ProviderError> for Reason {
    fn from(e: ProviderError) -> Self {
        if e.class() == ErrorClass::Transient {
            return Reason::Transient(display_chain(&e));
        }
        if let ProviderError::MalformedToolCall { raw, .. } = &e {
            return Reason::invalid_input_truncated(raw, display_chain(&e), RECORD_RAW_MAX);
        }
        match e.kind() {
            // The model cannot fix a credential, and its text would show it our configuration.
            // A request refused as invalid is the one our code and configuration built (no
            // messages, a zero output limit, a `tool_choice` naming no tool). Bytes this build
            // cannot read, our own failure, and a CLI or daemon we run: what has to change is on
            // our side, and the text would show the model our internals.
            LlmErrorKind::Auth
            | LlmErrorKind::InvalidRequest
            | LlmErrorKind::Parse
            | LlmErrorKind::Internal
            | LlmErrorKind::Encode
            | LlmErrorKind::InvalidUrl
            | LlmErrorKind::InvalidHeader
            | LlmErrorKind::DaemonScript
            | LlmErrorKind::Spawn
            | LlmErrorKind::Io
            | LlmErrorKind::DaemonExited
            | LlmErrorKind::Sandbox
            | LlmErrorKind::CliSubprocessDied => Reason::defect(display_chain(&e)),
            _ => Reason::CallFailed(display_chain(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(status: u16, message: &str) -> ProviderRefusal {
        ProviderRefusal {
            status: Some(status),
            message: message.into(),
        }
    }

    fn invalid_url() -> ProviderError {
        ProviderError::InvalidUrl {
            url: "v1/messages".into(),
            source: url::ParseError::RelativeUrlWithoutBase,
        }
    }

    fn io_error(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::from(kind)
    }

    fn unparsed() -> ProviderError {
        ProviderError::Parse {
            at: "a test response",
            line: None,
            problem: Box::new(ParseProblem::Missing("id")),
            raw: None,
        }
    }

    #[test]
    fn classes_partition_correctly() {
        assert_eq!(
            ProviderError::RateLimited { retry_after: None }.class(),
            ErrorClass::Transient
        );
        assert_eq!(
            ProviderError::Auth(CredentialError::Refused(refusal(401, "bad key"))).class(),
            ErrorClass::Permanent
        );
        assert_eq!(unparsed().class(), ErrorClass::Permanent);
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let e = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(e.unavailable_for(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn an_overload_says_nothing_about_how_long() {
        assert_eq!(ProviderError::ModelOverloaded.unavailable_for(), None);
    }

    #[test]
    fn a_transient_call_is_a_transient_step_failure() {
        let reason = Reason::from(ProviderError::RateLimited { retry_after: None });
        assert!(matches!(reason, Reason::Transient(_)), "{reason:?}");
        assert!(reason.is_retryable());
    }

    #[test]
    fn a_failure_on_our_side_is_a_defect_the_model_does_not_see() {
        for e in [
            unparsed(),
            invalid_url(),
            ProviderError::CliSubprocessDied {
                exit_code: Some(137),
                stderr: "Killed".into(),
            },
        ] {
            let reason = Reason::from(e);
            assert!(matches!(reason, Reason::Defect { .. }), "{reason:?}");
            assert!(!reason.may_reach_model());
            assert!(!reason.is_retryable());
        }
    }

    #[test]
    fn a_timeout_is_permanent_until_shown_to_go_away() {
        let reason = Reason::from(ProviderError::TimedOut {
            budget: Duration::from_secs(1),
            detail: "slow".into(),
        });
        assert!(matches!(reason, Reason::CallFailed(_)), "{reason:?}");
        assert!(!reason.is_retryable());
    }

    #[test]
    fn a_refused_credential_never_reaches_the_model() {
        let reason = Reason::from(ProviderError::Auth(CredentialError::Refused(refusal(
            401,
            "invalid x-api-key",
        ))));
        assert!(matches!(reason, Reason::Defect { .. }), "{reason:?}");
        assert!(!reason.may_reach_model());
    }

    /// Returns one of every variant. The match below has no wildcard, so a new variant does not
    /// compile until it is sampled here too.
    fn every_variant() -> Vec<ProviderError> {
        let _sampled = |e: &ProviderError| match e {
            ProviderError::Auth(_)
            | ProviderError::RateLimited { .. }
            | ProviderError::InvalidRequest(_)
            | ProviderError::ContentFiltered { .. }
            | ProviderError::ContextTooLong { .. }
            | ProviderError::ModelOverloaded
            | ProviderError::ServerError { .. }
            | ProviderError::RequestTimeout(_)
            | ProviderError::ProviderTimedOut(_)
            | ProviderError::Unavailable { .. }
            | ProviderError::Network(_)
            | ProviderError::StreamCut { .. }
            | ProviderError::Unfinished(_)
            | ProviderError::StreamViolation(_)
            | ProviderError::Stated { .. }
            | ProviderError::Parse { .. }
            | ProviderError::CliSubprocessDied { .. }
            | ProviderError::TimedOut { .. }
            | ProviderError::MalformedToolCall { .. }
            | ProviderError::TruncatedToolCall { .. }
            | ProviderError::IncompleteToolCall { .. }
            | ProviderError::Encode(_)
            | ProviderError::InvalidUrl { .. }
            | ProviderError::InvalidHeader { .. }
            | ProviderError::ExtraHeader(_)
            | ProviderError::CassetteNotWritten(_)
            | ProviderError::DaemonScript(_)
            | ProviderError::Spawn { .. }
            | ProviderError::Io { .. }
            | ProviderError::DaemonFailed { .. }
            | ProviderError::DaemonExited
            | ProviderError::Sandbox(_)
            | ProviderError::BatchItemCanceled
            | ProviderError::BatchItemExpired
            | ProviderError::BatchItemFailed { .. }
            | ProviderError::Internal(_) => (),
        };
        vec![
            ProviderError::Auth(CredentialError::Refused(refusal(401, "401"))),
            ProviderError::RateLimited { retry_after: None },
            ProviderError::InvalidRequest(RequestRefusal::Provider(refusal(400, "bad"))),
            ProviderError::ContentFiltered {
                category: "violence".into(),
            },
            ProviderError::ContextTooLong {
                limit: None,
                requested: None,
                message: "too long".into(),
            },
            ProviderError::ModelOverloaded,
            ProviderError::ServerError {
                status: Some(502),
                message: "bad gateway".into(),
            },
            ProviderError::RequestTimeout("request timeout".into()),
            ProviderError::ProviderTimedOut("request timed out".into()),
            ProviderError::StreamCut { open_call: None },
            ProviderError::Unfinished(tars_types::Unfinished::NoStopReason),
            ProviderError::StreamViolation(tars_types::StreamViolation::ArgsWithoutStart {
                index: 0,
            }),
            ProviderError::Stated {
                class: ErrorClass::Permanent,
                error: Box::new(ProviderError::ModelOverloaded),
            },
            ProviderError::Unavailable {
                until: std::time::Instant::now(),
                last_failure: LastFailure {
                    kind: LlmErrorKind::Network,
                    message: "network: reset".into(),
                },
            },
            ProviderError::Network(NetworkError::Idle(Duration::from_secs(1))),
            unparsed(),
            ProviderError::CliSubprocessDied {
                exit_code: Some(1),
                stderr: "boom".into(),
            },
            ProviderError::TimedOut {
                budget: Duration::from_secs(1),
                detail: "slow".into(),
            },
            ProviderError::MalformedToolCall {
                id: "c".into(),
                name: "t".into(),
                raw: "[]".into(),
                problem: MalformedArguments::NotAnObject(tars_types::JsonKind::Array),
            },
            ProviderError::TruncatedToolCall {
                id: "c".into(),
                name: "t".into(),
                raw: "{".into(),
            },
            ProviderError::IncompleteToolCall {
                id: "c".into(),
                name: "t".into(),
                raw: "{".into(),
            },
            ProviderError::Encode(serde_json::from_str::<u8>("x").unwrap_err()),
            invalid_url(),
            ProviderError::InvalidHeader {
                header: "anthropic-version",
                source: reqwest::header::HeaderValue::from_str("\n").unwrap_err(),
            },
            ProviderError::ExtraHeader(crate::provider::http_base::ExtraHeaderError::Name {
                header: "bad name".into(),
                source: reqwest::header::HeaderName::from_bytes(b"bad name").unwrap_err(),
            }),
            ProviderError::CassetteNotWritten(
                crate::provider::backends::cassette::WriteCassetteError::Encode(
                    serde_json::from_str::<u8>("x").unwrap_err(),
                ),
            ),
            ProviderError::DaemonScript(DaemonScriptError::NotFound),
            ProviderError::Spawn {
                program: "node".into(),
                source: io_error(std::io::ErrorKind::NotFound),
            },
            ProviderError::Spawn {
                program: "node".into(),
                source: io_error(std::io::ErrorKind::WouldBlock),
            },
            ProviderError::Io {
                op: IoOp::WriteChildStdin,
                source: io_error(std::io::ErrorKind::BrokenPipe),
            },
            ProviderError::Io {
                op: IoOp::WriteSystemPromptFile,
                source: io_error(std::io::ErrorKind::PermissionDenied),
            },
            ProviderError::DaemonFailed {
                message: "rate limited".into(),
            },
            ProviderError::DaemonExited,
            ProviderError::Sandbox(SandboxSetupError::NoWorkingDirectory(io_error(
                std::io::ErrorKind::NotFound,
            ))),
            ProviderError::BatchItemCanceled,
            ProviderError::BatchItemExpired,
            ProviderError::BatchItemFailed {
                code: Some("server_error".into()),
                message: "m".into(),
            },
            ProviderError::Internal("i".into()),
        ]
    }

    #[test]
    fn a_step_reruns_exactly_the_calls_the_error_calls_retriable() {
        for e in every_variant() {
            let retriable = e.class() != ErrorClass::Permanent;
            let reason = Reason::from(e);
            assert_eq!(reason.is_retryable(), retriable, "{reason:?}");
        }
    }

    #[test]
    fn only_a_failure_of_our_own_making_is_kept_from_the_model() {
        for e in every_variant() {
            let ours = e.class() == ErrorClass::Permanent
                && matches!(
                    e,
                    ProviderError::Auth(_)
                        | ProviderError::InvalidRequest(_)
                        | ProviderError::Parse { .. }
                        | ProviderError::Internal(_)
                        | ProviderError::Encode(_)
                        | ProviderError::InvalidUrl { .. }
                        | ProviderError::InvalidHeader { .. }
                        | ProviderError::ExtraHeader(_)
                        | ProviderError::CassetteNotWritten(_)
                        | ProviderError::DaemonScript(_)
                        | ProviderError::Spawn { .. }
                        | ProviderError::Io { .. }
                        | ProviderError::DaemonExited
                        | ProviderError::Sandbox(_)
                        | ProviderError::CliSubprocessDied { .. }
                );
            let reason = Reason::from(e);
            assert_eq!(reason.may_reach_model(), !ours, "{reason:?}");
        }
    }

    #[test]
    fn a_request_our_code_built_wrong_never_reaches_the_model() {
        let reason = Reason::from(ProviderError::InvalidRequest(
            RequestRefusal::ZeroMaxOutputTokens,
        ));
        assert!(matches!(reason, Reason::Defect { .. }), "{reason:?}");
        assert!(!reason.may_reach_model());
    }

    #[test]
    fn malformed_tool_arguments_go_back_to_the_model_with_its_own_text() {
        let reason = Reason::from(ProviderError::MalformedToolCall {
            id: "c1".into(),
            name: "edit".into(),
            raw: "[1, 2]".into(),
            problem: MalformedArguments::NotAnObject(tars_types::JsonKind::Array),
        });
        match &reason {
            Reason::InvalidInput { raw, .. } => assert_eq!(raw, "[1, 2]"),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert!(reason.may_reach_model());
        assert!(!reason.is_retryable());
    }

    #[test]
    fn another_permanent_failure_is_a_failed_call_the_model_reads() {
        let reason = Reason::from(ProviderError::ContextTooLong {
            limit: Some(200_000),
            requested: Some(210_000),
            message: "prompt is too long".into(),
        });
        assert!(matches!(reason, Reason::CallFailed(_)), "{reason:?}");
        assert!(reason.may_reach_model());
        assert!(!reason.is_retryable());
        assert!(reason.to_string().contains("prompt is too long"));
    }

    fn class_and_reach(e: ProviderError) -> (ErrorClass, bool) {
        let class = e.class();
        (class, Reason::from(e).may_reach_model())
    }

    #[test]
    fn a_server_error_is_transient_and_reruns_the_step() {
        let e = ProviderError::ServerError {
            status: Some(500),
            message: "Internal server error".into(),
        };
        assert_eq!(class_and_reach(e), (ErrorClass::Transient, true));
    }

    #[test]
    fn a_request_timeout_is_transient_and_reruns_the_step() {
        let e = ProviderError::RequestTimeout("request timeout".into());
        assert_eq!(class_and_reach(e), (ErrorClass::Transient, true));
    }

    #[test]
    fn a_provider_timeout_is_permanent_and_the_model_reads_it() {
        let e = ProviderError::ProviderTimedOut("request timed out".into());
        let (class, reaches) = class_and_reach(e);
        assert_eq!((class, reaches), (ErrorClass::Permanent, true));
        let reason = Reason::from(ProviderError::ProviderTimedOut("t".into()));
        assert!(matches!(reason, Reason::CallFailed(_)), "{reason:?}");
    }

    #[test]
    fn a_cut_stream_is_transient_and_reruns_the_step_with_or_without_an_open_call() {
        let call = ReceivedToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            raw: "{\"pa".into(),
        };
        for open_call in [None, Some(call)] {
            let reason = Reason::from(ProviderError::StreamCut { open_call });
            assert!(matches!(reason, Reason::Transient(_)), "{reason:?}");
            assert!(reason.is_retryable());
            assert!(reason.may_reach_model());
        }
    }

    #[test]
    fn a_stated_class_decides_over_the_errors_own() {
        let said_permanent = ProviderError::ServerError {
            status: Some(500),
            message: "m".into(),
        }
        .stated(Some(ErrorClass::Permanent));
        assert_eq!(said_permanent.class(), ErrorClass::Permanent);
        assert_eq!(said_permanent.kind(), LlmErrorKind::ServerError);
        let reason = Reason::from(said_permanent);
        assert!(matches!(reason, Reason::CallFailed(_)), "{reason:?}");

        let said_transient =
            ProviderError::InvalidRequest(RequestRefusal::Provider(refusal(409, "conflict")))
                .stated(Some(ErrorClass::Transient));
        assert_eq!(said_transient.class(), ErrorClass::Transient);
        assert_eq!(said_transient.kind(), LlmErrorKind::InvalidRequest);
        assert!(Reason::from(said_transient).is_retryable());
    }

    #[test]
    fn a_stated_error_says_only_the_statement_and_hangs_the_error_as_its_source() {
        let e = ProviderError::ServerError {
            status: Some(500),
            message: "m".into(),
        }
        .stated(Some(ErrorClass::Permanent));
        assert_eq!(e.to_string(), "the provider says it is permanent");
        assert_eq!(
            display_chain(&e),
            "the provider says it is permanent: server error 500: m"
        );
    }

    #[test]
    fn a_stated_class_that_agrees_or_is_absent_leaves_the_error_as_it_is() {
        for class in [None, Some(ErrorClass::Transient)] {
            let e = ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(3)),
            }
            .stated(class);
            assert!(matches!(e, ProviderError::RateLimited { .. }), "{e:?}");
        }
    }

    #[test]
    fn a_stated_error_keeps_how_long_its_cause_said_the_provider_is_unavailable() {
        let e = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        }
        .stated(Some(ErrorClass::Permanent));
        assert_eq!(e.unavailable_for(), Some(Duration::from_secs(3)));
    }

    #[test]
    fn only_true_and_false_state_a_class() {
        assert_eq!(stated_class(Some("true")), Some(ErrorClass::Transient));
        assert_eq!(stated_class(Some("false")), Some(ErrorClass::Permanent));
        assert_eq!(stated_class(Some("yes")), None);
        assert_eq!(stated_class(None), None);
    }

    #[test]
    fn an_os_error_that_passes_on_another_try_is_transient() {
        use std::io::ErrorKind as K;
        for kind in [K::WouldBlock, K::Interrupted, K::TimedOut, K::BrokenPipe] {
            assert_eq!(io_class(kind), ErrorClass::Transient, "{kind:?}");
        }
        for kind in [K::NotFound, K::PermissionDenied, K::InvalidInput] {
            assert_eq!(io_class(kind), ErrorClass::Permanent, "{kind:?}");
        }
    }

    #[test]
    fn a_spawn_refused_for_want_of_a_process_reruns_the_step() {
        let e = ProviderError::Spawn {
            program: "node".into(),
            source: io_error(std::io::ErrorKind::WouldBlock),
        };
        assert!(Reason::from(e).is_retryable());
    }

    #[test]
    fn a_missing_program_is_a_defect_the_model_does_not_see() {
        let reason = Reason::from(ProviderError::Spawn {
            program: "node".into(),
            source: io_error(std::io::ErrorKind::NotFound),
        });
        assert!(matches!(reason, Reason::Defect { .. }), "{reason:?}");
    }

    #[test]
    fn a_daemon_error_reply_is_transient_and_reruns_the_step() {
        let e = ProviderError::DaemonFailed {
            message: "rate limited".into(),
        };
        assert_eq!(e.class(), ErrorClass::Transient);
        assert!(Reason::from(e).is_retryable());
    }

    #[test]
    fn an_expired_batch_item_is_transient_and_a_cancelled_one_is_not() {
        assert_eq!(
            ProviderError::BatchItemExpired.class(),
            ErrorClass::Transient
        );
        assert_eq!(
            ProviderError::BatchItemCanceled.class(),
            ErrorClass::Permanent
        );
    }

    #[test]
    fn a_status_means_its_own_error() {
        let kind = |status| status_error(status, "m".into(), None).kind();
        assert_eq!(kind(401), LlmErrorKind::Auth);
        assert_eq!(kind(403), LlmErrorKind::Auth);
        assert_eq!(kind(407), LlmErrorKind::InvalidRequest);
        assert_eq!(kind(408), LlmErrorKind::RequestTimeout);
        assert_eq!(kind(409), LlmErrorKind::InvalidRequest);
        assert_eq!(kind(429), LlmErrorKind::RateLimited);
        assert_eq!(kind(499), LlmErrorKind::InvalidRequest);
        assert_eq!(kind(500), LlmErrorKind::ServerError);
        assert_eq!(kind(502), LlmErrorKind::ServerError);
        assert_eq!(kind(503), LlmErrorKind::ModelOverloaded);
        assert_eq!(kind(504), LlmErrorKind::ServerError);
        assert_eq!(kind(599), LlmErrorKind::ServerError);
        assert_eq!(kind(600), LlmErrorKind::InvalidRequest);
    }

    #[test]
    fn a_conflict_stays_permanent_until_the_provider_says_otherwise() {
        let e = status_error(409, "conflict".into(), None);
        assert_eq!(e.class(), ErrorClass::Permanent);
        assert_eq!(
            e.stated(stated_class(Some("true"))).class(),
            ErrorClass::Transient
        );
    }

    #[test]
    fn a_server_error_says_its_status() {
        let e = status_error(504, "upstream timed out".into(), None);
        assert_eq!(e.to_string(), "server error 504: upstream timed out");
        let in_stream = ProviderError::ServerError {
            status: None,
            message: "Internal server error".into(),
        };
        assert_eq!(in_stream.to_string(), "server error: Internal server error");
    }
}
