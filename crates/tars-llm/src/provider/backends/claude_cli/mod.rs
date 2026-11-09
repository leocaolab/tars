//! Calls Claude through the Claude Code CLI on the user's subscription: `claude -p -` with the
//! prompt on stdin, always inside the OS sandbox. `ANTHROPIC_API_KEY` and the Bedrock, Vertex and
//! Foundry switches are stripped from the child, whatever their case: with any of them `claude`
//! bills another account.

mod provider;

pub use crate::provider::backends::cli::{
    ClaudeCliEffort, ClaudeToolAccess, RealSubprocessRunner, SubprocessInvocation, SubprocessRunner,
};
pub use provider::{ClaudeCliProviderBuilder, claude_cli};
