//! Per-CLI [`CliDialect`](super::dialect::CliDialect) implementations.
//! ships [`claude::ClaudeCliDialect`]; adds [`codex::CodexCliDialect`];
//! adds [`opencode::OpenCodeDialect`]; adds
//! [`antigravity::AntigravityDialect`] — the first `OutputMode::Text` one and
//! Google's going-forward agent-CLI delegate.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod opencode;
