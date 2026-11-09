//! Defines [`LlmProvider`] and its backends: HTTP APIs, CLI subprocesses, a local SDK daemon,
//! cassette replay and a mock. See
//! `docs/architecture/01-llm-provider.md`.
//!
//! It does not retry, meter or budget calls; the middleware chain of an [`LlmService`] does.

#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod pipeline;
pub mod provider;

pub use provider::auth::{
    Auth, AuthError, AuthResolver, EnvFileAuthResolver, EnvSource, ProcessEnv, ResolvedAuth, basic,
    env_credential,
};
pub use provider::batch::{BatchResultItem, BatchStatus, BatchSubmitter, MockBatchSubmitter};
#[cfg(feature = "cli")]
pub use provider::child_reaper::{deregister, kill_all_spawned, register};
pub use provider::endpoint::{Endpoint, Route, route};
pub use provider::http_base::{
    HttpAdapter, HttpClient, HttpClientConfig, HttpOverrides, apply_extra_headers,
};
pub use provider::registry::{ProviderRegistry, RegistryError};
pub use provider::schema_adapt::{SchemaDialect, adapt_schema};
pub use provider::tool_buffer::StreamDecodeState;
pub use provider::trace_headers::TraceHeaderWriter;
pub use provider::wire::WireRecorder;
pub use provider::{LlmEventStream, LlmProvider};

pub use provider::backends::anthropic::{
    AnthropicAdapter, AnthropicProvider, AnthropicProviderBuilder,
};
#[cfg(feature = "cli")]
pub use provider::backends::claude_cli::{
    ClaudeCliEffort, ClaudeCliProviderBuilder, ClaudeToolAccess,
    SubprocessRunner as ClaudeCliSubprocessRunner, claude_cli,
};
#[cfg(feature = "cli")]
pub use provider::backends::cli::AgentCliBackend;
pub use provider::backends::gemini::{GeminiAdapter, GeminiProvider, GeminiProviderBuilder};
pub use provider::backends::mock::{CannedResponse, MockProvider};
pub use provider::backends::openai::{OpenAiAdapter, OpenAiProvider, OpenAiProviderBuilder};

pub use pipeline::circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CooldownTooLongError, MAX_COOLDOWN,
};
pub use pipeline::error::LlmError;
pub use pipeline::middleware::budget::{BudgetConfigError, PerCallBudgetMiddleware};
pub use pipeline::middleware::event_emitter::{BodyRetention, EventEmitterMiddleware};
pub use pipeline::middleware::rate_limit::{
    SharedBucketMismatchError, TpmBuckets, TpmRateLimitConfig, TpmRateLimiter,
};
pub use pipeline::middleware::retry::{RetryConfig, RetryMiddleware};
pub use pipeline::middleware::telemetry::TelemetryMiddleware;
pub use pipeline::middleware::tenant_budget::{
    BudgetStore, BudgetStoreError, InMemoryBudgetStore, TenantBudgetMiddleware,
};
pub use pipeline::middleware::validation::{
    OutputValidator, ValidationMiddleware,
    builtin::{JsonShapeValidator, MaxLengthValidator, NotEmptyValidator, OnExceed, ResponseField},
};
pub use pipeline::middleware::{ChainOptions, LlmServiceBuilder, Middleware};
pub use pipeline::service::{
    LlmService, Next, OpenFailure, RefusedCall, ServiceCall, max_output_tokens_sent,
};

pub use pipeline::complete::complete_async;
pub use provider::error::{
    CredentialError, CredentialKind, DaemonScriptError, Feature, IoOp, LastFailure, NetworkError,
    ParseProblem, ProviderError, ProviderRefusal, ReceivedToolCall, RequestRefusal,
    SandboxSetupError,
};
pub use tars_types::{Caller, ChatEvent, ChatRequest};
