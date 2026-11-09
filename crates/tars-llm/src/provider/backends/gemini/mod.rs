//! Calls Google's Gemini API (<https://ai.google.dev/gemini-api/docs/text-generation>) with an
//! API key. It does not support Vertex AI or batches.

mod adapter;
mod map_restore;
mod mapping;
mod provider;

pub use adapter::GeminiAdapter;
pub(crate) use provider::DEFAULT_BASE_URL;
pub use provider::{GeminiProvider, GeminiProviderBuilder};
