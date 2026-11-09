//! Runs the tool calls a model emits: the [`Tool`] trait, a [`ToolRegistry`] that turns a call
//! into the tool message the next turn sees, and the built-in tools.
//!
//! It does not run the agent loop and does not decide when a call is made.

#![deny(clippy::print_stdout, clippy::print_stderr)]

mod registry;
mod tool;

pub mod builtins;

pub use registry::{DuplicateToolError, ToolRegistry};
pub use tars_sandbox::{ResolveRootError, SandboxMode, SandboxPolicy};
pub use tool::{FsOp, Tool, ToolEnv, ToolError, ToolOutput};
