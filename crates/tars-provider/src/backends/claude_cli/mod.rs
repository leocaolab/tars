//! Claude Code CLI as an LLM Provider — subscription path.
//!
//! Since this module is the **claude construction surface** on
//! top of the shared CLI-delegate machinery in
//! [`crate::backends::cli`]. It:
//!
//! - Shells out to `claude -p - --model X --output-format json|stream-json
//!   --permission-mode bypassPermissions …` and feeds the prompt on stdin.
//! - **Strips** `ANTHROPIC_API_KEY` (case-insensitive) and 3rd-party
//!   routing env vars before exec'ing the child. If any leak through,
//!   `claude` switches to API-billing mode and silently bills the
//!   wrong account.
//! - OS-sandboxes the spawn when `TARS_CLAUDE_SANDBOX=1`.
//! - Parses the CLI's JSON output into canonical `ChatEvent`s.
//!
//!
//! - [`provider`] — [`ClaudeCliProviderBuilder`], the `claude_cli()`
//!   convenience helper, default capabilities, and the builder → dialect +
//!   backend wiring. The claude construction API.

mod provider;

// Re-export the shared CLI-delegate types under the historical
// `backends::claude_cli::…` paths that `registry.rs`, the crate root
// (`lib.rs`), and the `security_delegate_cli` integration test import.
pub use crate::backends::cli::{
    ClaudeCliEffort, ClaudeCliTools, RealSubprocessRunner, SubprocessInvocation, SubprocessRunner,
};
pub use provider::{ClaudeCliProvider, ClaudeCliProviderBuilder, claude_cli};
