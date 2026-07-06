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

#[macro_use]
mod builder_macros;

pub mod auth;
pub mod backends;
pub mod batch;
pub mod child_reaper;
pub mod http_base;
pub mod provider;
pub mod registry;
pub mod schema_adapt;
pub mod subprocess_diagnostics;
pub mod tool_buffer;
// Audit `tars-provider-src-transport-1` + TODO O-1: the `transport`
// module was a speculative `HttpTransport` trait with no
// in-tree call site (every backend goes straight through
// `HttpProviderBase.client`). The trigger condition O-1 set —
// "we hit `tars-pipeline` MVP without anyone needing it" — has
// fired, so the module was deleted on 2026-05-03 (commit follows).

pub use auth::{Auth, AuthError, AuthResolver, BasicAuthResolver, ResolvedAuth, basic};
pub use batch::{BatchSubmitter, MockBatchSubmitter};
pub use child_reaper::{deregister, kill_all_spawned, register};
pub use http_base::{HttpAdapter, HttpProviderBase, HttpProviderConfig};
pub use provider::{LlmEventStream, LlmProvider};
pub use registry::{ProviderRegistry, RegistryError};
pub use schema_adapt::{SchemaDialect, adapt_schema};
pub use subprocess_diagnostics::{
    SubprocessDiagnostics, diagnose_child_exit, find_claude_session_log,
    summarise_claude_session_log, worktree_diff_summary,
};
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
pub use backends::gemini_cli::{
    GeminiCliProvider, GeminiCliProviderBuilder, SubprocessRunner as GeminiCliSubprocessRunner,
    gemini_cli,
};
pub use backends::mock::{CannedResponse, MockProvider};
pub use backends::openai::{OpenAiAdapter, OpenAiProvider, OpenAiProviderBuilder};
pub use backends::vllm::{vllm, vllm_local};
