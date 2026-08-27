//! Per-CLI [`CliDialect`](super::dialect::CliDialect) implementations.
//! M0 ships [`claude::ClaudeCliDialect`]; M1 adds [`codex::CodexCliDialect`];
//! M2 adds [`opencode::OpenCodeDialect`]; M3 adds
//! [`antigravity::AntigravityDialect`] — the first `OutputMode::Text` one and
//! Google's going-forward agent-CLI delegate.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod opencode;
