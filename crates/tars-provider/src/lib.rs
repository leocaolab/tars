//! `tars-provider` — LLM Provider trait and concrete backends.
//!
//! Every backend (HTTP API, CLI subprocess, embedded) is an impl of
//! [`LlmProvider`]. The trait is intentionally minimal: `stream` is the
//! basic operation and `complete` defaults to "consume the stream and
//! aggregate".
//!
//! Module map:
//! - [`provider`] — trait + types
//! - [`auth`]     — `Auth` enum + `AuthResolver` trait + basic resolvers
//! - [`http_base`] — shared `HttpAdapter` infra (reqwest client, retry, SSE)
//! - [`tool_buffer`] — accumulates streaming tool calls
//! - [`backends`] — concrete provider implementations
//!
//! See `docs/architecture/01-llm-provider.md`.

#[macro_use]
mod builder_macros;

pub mod auth;
pub mod backends;
pub mod batch;
pub mod child_reaper;
pub mod global;
pub mod http_base;
pub mod provider;
pub mod registry;
pub mod schema_adapt;
pub mod tool_buffer;

pub use auth::{Auth, AuthError, AuthResolver, BasicAuthResolver, ResolvedAuth, basic};
pub use batch::{BatchSubmitter, MockBatchSubmitter};
pub use child_reaper::{deregister, kill_all_spawned, register};
pub use http_base::{HttpAdapter, HttpProviderBase, HttpProviderConfig};
pub use provider::{LlmEventStream, LlmProvider};
pub use registry::{ProviderRegistry, RegistryError};
pub use schema_adapt::{SchemaDialect, adapt_schema};
pub use tool_buffer::ToolCallBuffer;

// Re-export concrete backends at the crate root for ergonomic use.
pub use backends::anthropic::{AnthropicAdapter, AnthropicProvider, AnthropicProviderBuilder};
#[cfg(feature = "bedrock")]
pub use backends::bedrock::{BedrockProvider, BedrockProviderBuilder};
pub use backends::claude_cli::{
    ClaudeCliEffort, ClaudeCliProvider, ClaudeCliProviderBuilder, ClaudeCliTools,
    SubprocessRunner as ClaudeCliSubprocessRunner, claude_cli,
};
pub use backends::gemini::{GeminiAdapter, GeminiProvider, GeminiProviderBuilder};
pub use backends::mock::{CannedResponse, MockProvider};
pub use backends::openai::{OpenAiAdapter, OpenAiProvider, OpenAiProviderBuilder};
pub use backends::vllm::{vllm, vllm_local};
