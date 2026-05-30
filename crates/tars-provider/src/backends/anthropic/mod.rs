//! Anthropic (Claude) HTTP backend.
//!
//! Wire format reference: <https://docs.anthropic.com/en/api/messages>
//!
//! Differences from OpenAI worth noting:
//!
//! - **Auth**: `x-api-key` header (not `Authorization: Bearer`).
//! - **Versioning**: `anthropic-version: 2023-06-01` mandatory header.
//! - **System**: separate top-level `system` field, not a message role.
//! - **Tool calls**: `tool_use` content blocks; no JSON-string nesting
//!   (args arrive as a real object — easier than OpenAI in this regard).
//! - **Caching**: explicit `cache_control: {type: "ephemeral"}` markers
//!   inserted on specific blocks. We attach to the system prompt and
//!   to the *last* message when [`tars_types::CacheDirective::MarkBoundary`]
//!   is set.
//! - **Thinking**: a `thinking` content block + `thinking` config; we
//!   surface the deltas as [`tars_types::ChatEvent::ThinkingDelta`].
//! - **Structured output**: emulated via a forced `tool_choice` (Doc
//!   01 §9). The "tool" is a synthetic schema-only call.
//! - **Streaming events**: SSE with named events (`message_start`,
//!   `content_block_start`, `content_block_delta`, `message_delta`,
//!   `message_stop`, `ping`, `error`). The named events are key — we
//!   route on `raw.event`, not just `data`.
//!
//! ## Module layout (split per `arc scan --judge` finding `ARC-L5-M-12`)
//!
//! Originally a single 1296-line file mixing provider lifecycle,
//! protocol translation, batch-API plumbing, and pure JSON helpers.
//! Split into focused sub-modules with the same public surface,
//! mirroring the Batch 6 `gemini.rs` and the M-13 `claude_cli.rs`
//! splits:
//!
//! - [`provider`] — `AnthropicProvider`, `AnthropicProviderBuilder`,
//!   the `LlmProvider` impl, the `BatchSubmitter` impl, default
//!   capabilities. The orchestration + I/O layer.
//! - [`adapter`] — `AnthropicAdapter` (request translation, SSE event
//!   parsing, error classification, URL construction). Re-usable in
//!   tests without an `HttpProviderBase`.
//! - [`mapping`] — pure helpers: `translate_batch_status`,
//!   `parse_batch_results`, `message_to_chat_response`, `map_stop_reason`,
//!   `parse_usage`, `truncate`. Stateless, no I/O — the JSON conversion
//!   layer.

mod adapter;
mod mapping;
mod provider;

pub use adapter::AnthropicAdapter;
pub use provider::{AnthropicProvider, AnthropicProviderBuilder};
