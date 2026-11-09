//! Holds the [`crate::LlmProvider`] backends. A module's suffix says how it reaches the model:
//!
//! - `*_cli` runs a binary the user installed and logged into (`claude login`); tars never sees
//!   the credentials.
//! - `*_sdk` runs one long-lived child that embeds a vendor SDK and owns its login, and talks to
//!   it over stdin and stdout (`tools/claude-daemon/` for `claude_sdk`).
//! - A bare vendor name calls that vendor's HTTP API with tars's key.
//! - `vllm`, `llamacpp` and `mlx` are OpenAI-compatible local servers.

pub mod anthropic;
pub mod cassette;
#[cfg(feature = "cli")]
pub mod claude_cli;
#[cfg(feature = "cli")]
pub mod claude_sdk;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "cli")]
pub mod codex_cli;
pub mod gemini;
pub mod llamacpp;
pub mod mlx;
pub mod mock;
pub mod openai;
#[cfg(feature = "cli")]
pub mod subprocess_diagnostics;
pub mod vllm;
