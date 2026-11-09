//! Calls Anthropic's Messages API (<https://docs.anthropic.com/en/api/messages>) and its batches.
//! Where it differs from OpenAI: the key goes in `x-api-key` with a required `anthropic-version`
//! header; the system prompt is a top-level field; tool arguments arrive as objects; prompt caching
//! needs explicit `cache_control` markers, put on the system prompt and the last user message when
//! [`tars_types::CacheDirective::MarkBoundary`] is set; structured output is a forced call to a
//! schema-only tool; and stream events are routed by their SSE event name.

mod adapter;
mod mapping;
mod provider;

pub use adapter::AnthropicAdapter;
pub use provider::{AnthropicProvider, AnthropicProviderBuilder};
pub(crate) use provider::{DEFAULT_API_VERSION, DEFAULT_BASE_URL};
