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
//! ## Module layout (split per `arc scan --judge` finding)
//!
//! Originally a single 1060-line file mixing provider lifecycle,
//! protocol translation, and free-function helpers; the L5 Tribunal
//! flagged it as the only god-module across the provider backends.
//! Split into three sub-modules with the same public surface:
//!
//! - [`provider`] — `GeminiProviderBuilder`, `GeminiProvider`, and the
//!   trait impls that connect them to the HTTP base + the (unsupported)
//!   batch surface. The orchestration layer.
//! - [`adapter`] — `GeminiAdapter` and its `HttpAdapter` impl: request
//!   translation, SSE event parsing, error classification. The
//!   protocol-translation layer; reusable in tests without an
//!   `HttpProviderBase`.
//! - [`mapping`] — pure helpers (`map_stop_reason`, `parse_usage`,
//!   `truncate`, `urlencoding`). No I/O, no state, easy to unit-test.

mod adapter;
mod mapping;
mod provider;

pub use adapter::GeminiAdapter;
pub use provider::{GeminiProvider, GeminiProviderBuilder};
