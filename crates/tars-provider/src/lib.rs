//! `tars-provider` — LLM Provider trait and concrete backends.
//!
//! Per Doc 01, every backend (HTTP API, CLI subprocess, embedded) is an
//! impl of [`LlmProvider`]. The trait is intentionally minimal:
//! `stream` is the basic operation and `complete` defaults to "consume
//! the stream and aggregate".
//!
//! Module map:
//! - [`provider`] — trait + types
//! - [`auth`]     — `Auth` enum + `AuthResolver` trait + basic resolvers
//! - [`http_base`] — shared `HttpAdapter` infra (reqwest client, retry, SSE)
//! - [`tool_buffer`] — accumulates streaming tool calls (Doc 01 §8.1)
//! - [`backends`] — concrete provider implementations
//!
//! See `docs/architecture/01-llm-provider.md`.

pub mod auth;
pub mod backends;
pub mod http_base;
pub mod provider;
pub mod registry;
pub mod tool_buffer;
// Audit `tars-provider-src-transport-1` + TODO O-1: the `transport`
// module was a speculative `HttpTransport` trait with no
// in-tree call site (every backend goes straight through
// `HttpProviderBase.client`). The trigger condition O-1 set —
// "we hit `tars-pipeline` MVP without anyone needing it" — has
// fired, so the module was deleted on 2026-05-03 (commit follows).

pub use auth::{Auth, AuthResolver, ResolvedAuth, AuthError, BasicAuthResolver, basic};
pub use http_base::{HttpAdapter, HttpProviderBase, HttpProviderConfig};
pub use provider::{LlmProvider, LlmEventStream};
pub use registry::{ProviderRegistry, RegistryError};
pub use tool_buffer::ToolCallBuffer;

// Re-export concrete backends at the crate root for ergonomic use.
pub use backends::anthropic::{AnthropicAdapter, AnthropicProvider, AnthropicProviderBuilder};
pub use backends::claude_cli::{
    claude_cli, ClaudeCliProvider, ClaudeCliProviderBuilder,
    SubprocessRunner as ClaudeCliSubprocessRunner,
};
pub use backends::gemini::{GeminiAdapter, GeminiProvider, GeminiProviderBuilder};
pub use backends::gemini_cli::{
    gemini_cli, GeminiCliProvider, GeminiCliProviderBuilder,
    SubprocessRunner as GeminiCliSubprocessRunner,
};
pub use backends::mock::{CannedResponse, MockProvider};
pub use backends::openai::{OpenAiAdapter, OpenAiProvider, OpenAiProviderBuilder};
pub use backends::vllm::{vllm, vllm_local};
