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
//! - The eval primitives ([`judge`], [`judge_stats`], [`check`],
//!   [`metamorphic`], [`arg_judge`], [`trajectory_match`]) — scoring that sits
//!   over the transport and has nothing to do with an agent loop.
//! - Small helpers with their own reason to exist: [`bind`] (a skill set to
//!   the tools that back it), [`PromptBuilder`], and [`sync`] wrappers.

pub mod arg_judge;
mod bind;
pub mod check;
mod error;
mod event;
pub mod judge;
pub mod judge_stats;
pub mod metamorphic;
mod prompt;
mod runtime;
pub mod run_report;
mod session;
pub mod sync;
pub mod trajectory_match;

pub use arg_judge::{ArgEquivalenceJudge, args_match_judged};
pub use bind::{BindError, bind};
pub use check::{CheckResult, CheckRunner, Invariant, MembershipInvariant, ValidatorInvariant};
pub use error::RuntimeError;
pub use event::{AgentEvent, StepIdempotencyKey, tool_sequence, tool_step_sequence};
pub use judge::{
    DEFAULT_JUDGE_PROMPT, Judge, JudgeError, LlmJudge, ensure_anti_incest, run_judge_pass,
};
pub use judge_stats::{
    JudgeItem, JudgeReport, JudgeVerdict, JudgedItem, McNemarResult, mcnemar,
};
pub use metamorphic::{
    DeleteSubstringMutation, DirectionalRelation, GoldenMatch, InvarianceRelation,
    MetamorphicRelation, Mutation, MutationVerdict, mutation_caught,
};
pub use prompt::PromptBuilder;
pub use runtime::{LocalRuntime, Runtime};
pub use run_report::build_run_report;
pub use session::{Budget, Session, SessionError, SessionOptions, Tokenizer, Turn};
pub use sync::{complete_async, complete_sync, shared_runtime};
pub use trajectory_match::{MatchMode, ToolStep};
// Tools live in `tars-tools` now (Doc 23). Re-export the whole contract —
// including the gate/approval/sandbox seams — so callers that build gated
// Sessions name it from one place.
pub use tars_tools::{
    ApprovalDecision, ApprovalRequest, ApprovalSink, DenyAllSink, PermissionView, SandboxPolicy,
    Tool, ToolContext, ToolDecision, ToolError, ToolRegistry, ToolResult,
};
