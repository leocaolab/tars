//! `tars-tools` — Tool trait + ToolRegistry + built-in tools.
//!
//! The executable side of tool calling. The [`Tool`] trait defines what a
//! callable tool looks like; the [`ToolRegistry`] holds a name-keyed table of
//! them and dispatches tool calls.
//!
//! This crate does NOT own the agent loop. It provides the execution primitives
//! (`tools.dispatch(call) → Message`) for the agent to use.

mod registry;
mod tool;

pub mod builtins;

pub use registry::{ToolRegistry, ToolRegistryError};
pub use tars_sandbox::{SandboxMode, SandboxPolicy};
pub use tool::{Tool, ToolContext, ToolError, ToolResult};
