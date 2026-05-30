//! OpenAI HTTP backend.
//!
//! Also serves OpenAI-compatible endpoints (vLLM, llama.cpp server,
//! Groq, Together, DeepSeek) by overriding `base_url`.
//!
//! Mirrors the Python `OpenAIClient` (the equivalent Python OpenAI
//! client) semantics for `max_tokens` vs `max_completion_tokens`
//! routing and usage tracking, but is async + streaming.
//!
//! ## Module layout (split per `arc scan --judge` finding `ARC-L5-M-11`)
//!
//! Originally a single 1358-line file mixing provider lifecycle, batch
//! plumbing, protocol translation, and pure JSON helpers. Split into
//! focused sub-modules with the same public surface, mirroring the
//! existing `anthropic/`, `gemini/`, and `claude_cli/` layouts:
//!
//! - [`provider`] — `OpenAiProvider`, `OpenAiProviderBuilder`, the
//!   `LlmProvider` + `BatchSubmitter` impls, the batch helpers, and
//!   the default capability descriptor. The orchestration + I/O layer.
//! - [`adapter`] — `OpenAiAdapter` (request translation, SSE event
//!   parsing, error classification, URL construction). The
//!   protocol-translation layer; reusable in tests without an
//!   `HttpProviderBase`. URL builders are inherent methods over the
//!   adapter's private `base_url`, so they live here rather than in a
//!   standalone `urls.rs`.
//! - [`mapping`] — pure helpers: `translate_openai_batch_status`,
//!   `parse_openai_batch_results`, `openai_chat_completion_to_chat_response`,
//!   `parse_openai_usage`, `drain_buffer_into`. Stateless, no I/O —
//!   the JSON conversion layer.

mod adapter;
mod mapping;
mod provider;

pub use adapter::OpenAiAdapter;
pub use provider::{OpenAiProvider, OpenAiProviderBuilder, default_openai_capabilities};
