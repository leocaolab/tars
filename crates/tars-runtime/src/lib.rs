//! tars-runtime — the trajectory record, the durable step core, and a session.
//!
//! What is left here after the Doc 04 agent generation was removed: the pieces
//! that other crates actually reach for, and nothing that exists only to be
//! built on later.
//!
//! - [`AgentEvent`] and [`LocalRuntime`]'s successor surface — the
//!   event-sourced trajectory. `tars trajectory` and [`run_report`] read it;
//!   it is the record of what a run did, in order.
//! - [`Session`] — a stateful multi-turn conversation over `LlmService`,
//!   enforcing role alternation and carrying a budget. The Python and Node
//!   bindings are its consumers.
//! - Small helpers with their own reason to exist: [`bind`] (a skill set to
//!   the tools that back it), [`PromptBuilder`], and [`sync`] wrappers.

mod bind;
mod error;
mod event;
mod prompt;
mod runtime;
pub mod run_report;
mod session;
pub mod sync;

pub use bind::{BindError, bind};
pub use error::RuntimeError;
pub use event::{AgentEvent, StepIdempotencyKey, tool_sequence};
pub use prompt::PromptBuilder;
pub use runtime::{LocalRuntime, Runtime};
pub use run_report::build_run_report;
pub use session::{Budget, Session, SessionError, SessionOptions, Tokenizer, Turn};
pub use sync::{complete_async, complete_sync, shared_runtime};
// Tools live in `tars-tools` now (Doc 23). Re-export the whole contract —
// including the gate/approval/sandbox seams — so callers that build gated
// Sessions name it from one place.
pub use tars_tools::{
    ApprovalDecision, ApprovalRequest, ApprovalSink, DenyAllSink, PermissionView, SandboxPolicy,
    Tool, ToolContext, ToolDecision, ToolError, ToolRegistry, ToolResult,
};
