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
//! See `docs/01-llm-provider.md`.

pub mod auth;
pub mod backends;
pub mod http_base;
pub mod provider;
pub mod tool_buffer;

pub use auth::{Auth, AuthResolver, ResolvedAuth, AuthError};
pub use http_base::{HttpAdapter, HttpProviderBase, HttpProviderConfig};
pub use provider::{LlmProvider, LlmEventStream};
pub use tool_buffer::ToolCallBuffer;

// Re-export concrete backends at the crate root for ergonomic use.
pub use backends::mock::MockProvider;
pub use backends::openai::{OpenAiProvider, OpenAiProviderBuilder};
