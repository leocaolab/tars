//! `tars-tools` — Tool trait + ToolRegistry + built-in tools (Doc 05).
//!
//! What this crate **is**: the executable side of tool calling. The
//! [`Tool`] trait defines what a callable tool looks like; the
//! [`ToolRegistry`] holds a name-keyed table of them and dispatches
//! one [`tars_types::ToolCall`] (the LLM's request) into one
//! [`tars_types::Message::Tool`] (the result we feed back next turn).
//!
//! What this crate is **not**: it does NOT own the agent loop. The
//! "call LLM → see ToolCalls → execute → re-call LLM → repeat"
//! conversation is the Agent's job (today: `tars-runtime`'s
//! `WorkerAgent`, follow-on commit). This crate just gives that loop
//! the primitives — `tools.dispatch(call) → Message`.
//!
//! ## Scope of the first cut
//!
//! Doc 05 §3.3 specifies a richer `Tool` trait (idempotency tags,
//! side-effect declarations, IAM scopes, budget hooks, timeouts).
//! We ship the **executable core** today — name + description +
//! input_schema + execute — because that's what the Worker needs to
//! stop being a stub. The remaining fields slot in when their
//! consumers ship:
//!
//! - **idempotency_key tags** → wait for tool-side dedupe (today the
//!   trajectory layer's `StepIdempotencyKey` is per-step, not per-tool;
//!   most tools don't need their own dedupe layer).
//! - **side_effects declaration** → blocked on Saga compensation
//!   (Doc 04 §6) which doesn't exist yet.
//! - **iam_scopes** → blocked on `tars-security` (Doc 14 M6).
//! - **budget_hint** → blocked on BudgetMiddleware (TODO B-2).
//! - **timeout** → easy to bolt on; deferred until a tool actually
//!   needs it (the first long-running one — `git fetch`, `web fetch`,
//!   shell exec). The CancellationToken in [`ToolContext`] covers
//!   the upstream-cancel case today.
//!
//! ## Built-in tools
//!
//! - [`builtins::ReadFileTool`] — `fs.read_file`. Read a UTF-8 text
//!   file, optional path-jail, hard size cap. The smallest useful
//!   tool that proves the trait + registry plumbing end-to-end and
//!   gives WorkerAgent something real to do.
//!
//! Future builtins (each its own commit when WorkerAgent's loop has
//! a consumer for it):
//!   - `fs.write_file` — needs Saga compensation thinking before it
//!     can ship safely (the rollback story matters once Workers can
//!     mutate the filesystem).
//!   - `fs.list_dir`
//!   - `git.fetch_pr_diff`
//!   - `web.fetch`
//!   - `shell.exec` — biggest blast radius; ships last with explicit
//!     allowlist + jail.

mod registry;
mod tool;

pub mod builtins;

pub use registry::{ToolRegistry, ToolRegistryError};
pub use tool::{Tool, ToolContext, ToolError, ToolResult};
