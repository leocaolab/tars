//! OpenAI HTTP backend.
//!
//! Also serves OpenAI-compatible endpoints (vLLM, llama.cpp server,
//! Groq, Together, DeepSeek) by overriding `base_url`.
//!
//! Async + streaming; routes `max_tokens` vs `max_completion_tokens` per
//! model and tracks usage.
//!
//! ## Module layout
//!
//! - [`provider`] — `OpenAiProvider`, `OpenAiProviderBuilder`, the
//!   `LlmProvider` + `BatchSubmitter` impls, the batch helpers, and
//!   the default capability descriptor. The orchestration + I/O layer.
//! - [`adapter`] — `OpenAiAdapter` (request translation, SSE event
//!   parsing, error classification, URL construction). The
//!   protocol-translation layer; reusable in tests without an
//!   `HttpProviderBase`.
//! - [`dialect`] — the `OpenAiDialect` behavior seam + `StandardDialect`.
//!   Per-variant quirks (DeepSeek, LM Studio, …) live in their own impls;
//!   the default methods delegate to the adapter/mapping code below.
//! - [`mapping`] — pure helpers: `translate_openai_batch_status`,
//!   `parse_openai_batch_results`, `openai_chat_completion_to_chat_response`,
//!   `parse_openai_usage`, `drain_buffer_into`. Stateless, no I/O.

mod adapter;
mod dialect;
mod mapping;
mod provider;

pub use adapter::OpenAiAdapter;
pub use dialect::{OpenAiDialect, StandardDialect};
pub use provider::{OpenAiProvider, OpenAiProviderBuilder, default_openai_capabilities};
