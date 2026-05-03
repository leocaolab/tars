//! Concrete provider implementations.
//!
//! Naming convention:
//! - `*_cli` = subscription-authenticated path through a user-installed
//!   binary (the user's `claude login` / `gcloud auth` is reused; we
//!   never see the credentials).
//! - Provider name (no suffix) = direct HTTP API path with our own key.
//! - `vllm` etc. = OpenAI-compatible local servers.

pub mod anthropic;
pub mod claude_cli;
pub mod codex_cli;
pub mod gemini;
pub mod gemini_cli;
pub mod llamacpp;
pub mod mlx;
pub mod mock;
pub mod openai;
pub mod vllm;
