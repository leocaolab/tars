//! OpenAI HTTP backend.

mod adapter;
mod dialect;
mod mapping;
mod provider;

pub use adapter::OpenAiAdapter;
pub use dialect::{OpenAiDialect, StandardDialect};
pub use provider::{OpenAiProvider, OpenAiProviderBuilder, default_openai_capabilities};
