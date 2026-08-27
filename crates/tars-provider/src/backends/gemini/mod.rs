//! Google Gemini HTTP backend.
//!
//! Wire format reference:
//! <https://ai.google.dev/gemini-api/docs/text-generation>
//!
//! Differences from OpenAI / Anthropic:
//!
//! - **Auth**: `?key=...` query param (alternative: ADC bearer for
//!   Vertex AI, not yet supported here).
//! - **Roles**: assistant is `model`, not `assistant`. System is a
//!   separate `system_instruction` (NOT a role).
//! - **Messages**: `contents` array, each with `role` + `parts`.
//! - **Tool calls**: `functionCall` part (singular, no `tool_calls` list);
//!   parallel calls = multiple parts in the same message.
//! - **Tool results**: `functionResponse` part inside a `user`-role message.
//! - **Structured output**: `responseSchema` + `responseMimeType`.
//! - **Thinking**: parts have a `thought: bool` flag; thinking config
//!   sets `thinking_config.thinking_budget`.
//! - **Safety filter null**: when blocked the response has
//!   `candidates: null` — surface as ContentFiltered, don't index `[0]`.
//! - **Streaming endpoint**: `streamGenerateContent?alt=sse&key=...`.
//!
//! ## Module layout
//!
//! - [`provider`] — `GeminiProviderBuilder`, `GeminiProvider`, and the
//!   trait impls connecting them to the HTTP base + the (unsupported)
//!   batch surface.
//! - [`adapter`] — `GeminiAdapter` and its `HttpAdapter` impl: request
//!   translation, SSE event parsing, error classification. Reusable in
//!   tests without an `HttpProviderBase`.
//! - [`mapping`] — pure helpers (`map_stop_reason`, `parse_usage`,
//!   `truncate`, `urlencoding`).

mod adapter;
mod mapping;
mod provider;

pub use adapter::GeminiAdapter;
pub use provider::{GeminiProvider, GeminiProviderBuilder};
