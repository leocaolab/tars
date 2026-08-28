//! Google Gemini HTTP backend.
//!
//! Wire format reference:
//! <https://ai.google.dev/gemini-api/docs/text-generation>
//!


mod adapter;
mod mapping;
mod provider;

pub use adapter::GeminiAdapter;
pub use provider::{GeminiProvider, GeminiProviderBuilder};
