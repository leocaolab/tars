//! Calls OpenAI's chat-completions API and the many servers that speak it, with a dialect for
//! each one's quirks.

mod adapter;
mod dialect;
mod mapping;
mod provider;

pub use adapter::OpenAiAdapter;
pub use dialect::{OpenAiDialect, StandardDialect};
pub(crate) use provider::CompatServer;
pub(crate) use provider::DEFAULT_BASE_URL;
pub use provider::{OpenAiProvider, OpenAiProviderBuilder, default_openai_capabilities};
