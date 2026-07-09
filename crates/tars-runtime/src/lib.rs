//! tars-runtime — Agent Runtime core (Doc 04 + Doc 14 §9).
//!
//! M3 first-cut scope (this commit): the **event-sourced trajectory
//! primitive** that every later piece (Agent execution, Recovery,
//! Backtrack, ContextStore, PromptBuilder, OrchestratorAgent,
//! WorkerAgent, CriticAgent) is going to build on. Concretely:
//!
//! - [`AgentEvent`]: the event-log unit. Eight variants covering
//!   trajectory lifecycle (Started / Completed / Suspended /
//!   Abandoned), step lifecycle (StepStarted / StepCompleted /
//!   StepFailed), and LLM-call capture for replay. Fields stay
//!   primitive (strings, ids, `Usage`) — Doc 04's `ContentRef`,
//!   `BranchReason`, `AgentMessage` and other elaborate nested types
//!   come in once a real consumer needs them.
//! - [`Runtime`] trait: thin facade over the event store. Lets
//!   callers create a trajectory, append events, replay history.
//!   Does **not** yet host an `Agent` execution loop — that needs
//!   prompt design + tool registry + critic protocol, all of which
//!   are separate M3 commits.
//! - [`LocalRuntime`]: production impl backed by
//!   [`tars_storage::EventStore`]. Used by `tars-cli` (next commit)
//!   to log every `tars run` invocation as a one-event trajectory
//!   so the recovery / replay story has a real consumer to test
//!   against.
//!
//! Out of scope (deferred to follow-on M3 commits):
//! - `Agent` trait + `AgentMessage` typed contract (Doc 04 §4)
//! - `OrchestratorAgent` / `WorkerAgent` / `CriticAgent` defaults
//! - `ContextStore` + `ContextCompactor`
//! - `PromptBuilder`
//! - Backtrack + Saga compensation
//! - `Trajectory` struct with parent / branch / status fields
//!   (today the trajectory IS its event sequence; the struct view
//!   is a derived projection we'll add when something consumes it).

mod agent;
mod bind;
pub mod check;
mod critic;
mod ensemble_agent;
mod error;
mod event;
mod executor;
pub mod judge;
mod llm_adapters;
mod message;
mod tars_agent;
pub use ensemble_agent::EnsembleAgent;
pub use tars_agent::TarsAgent;
pub mod arg_judge;
pub mod metamorphic;
pub mod trajectory_match;
mod orchestrator;
mod prompt;
pub mod run_report;
mod runtime;
mod session;
pub mod sync;
mod task;
mod worker;

pub use agent::{
    Agent, AgentContext, LlmStreamHooks, StepError, AgentOutput, AgentRole, AgentStepResult,
    SingleShotAgent,
};
pub use bind::{BindError, bind};
pub use check::{CheckResult, CheckRunner, Invariant, MembershipInvariant, ValidatorInvariant};
pub use critic::{CriticAgent, CriticError, PartialResultRef};
pub use error::RuntimeError;
pub use event::{AgentEvent, StepIdempotencyKey, tool_sequence, tool_step_sequence};
pub use executor::{
    Critic, CriticContext, InfraClass, InfraRetryPolicy, RunPlanConfig, RunPlanError, Worker,
    WorkerContext, WorkerOutput, WorkerRegistry, default_infra_classifier, emit_step_lifecycle,
    run_plan,
};
pub use judge::{
    DEFAULT_JUDGE_PROMPT, Judge, JudgeError, LlmJudge, ensure_anti_incest, run_judge_pass,
};
pub use llm_adapters::{LlmCritic, LlmWorker};
pub use message::{AgentMessage, VerdictKind};
pub use metamorphic::{
    DeleteSubstringMutation, DirectionalRelation, GoldenMatch, InvarianceRelation,
    MetamorphicRelation, Mutation, MutationVerdict, mutation_caught,
};
pub use orchestrator::{
    Fan, OrchestratorAgent, OrchestratorError, Plan, PlanBuilder, PlanStep, StepCondition,
};
pub use arg_judge::{ArgEquivalenceJudge, args_match_judged};
pub use trajectory_match::{MatchMode, ToolStep};
pub use prompt::PromptBuilder;
pub use run_report::build_run_report;
pub use runtime::{AgentExecutionError, LocalRuntime, Runtime, execute_agent_step};
pub use session::{Budget, Session, SessionError, SessionOptions, Tokenizer, Turn};
pub use sync::{complete_async, complete_sync, shared_runtime};
// Tools live in `tars-tools` now (Doc 23). Re-export the whole contract —
// including the gate/approval/sandbox seams — so callers that build gated
// Sessions (e.g. the Codex-TUI backend, Doc 22) name it from one place.
pub use tars_tools::{
    ApprovalDecision, ApprovalRequest, ApprovalSink, DenyAllSink, PermissionView, SandboxPolicy,
    Tool, ToolContext, ToolDecision, ToolError, ToolRegistry, ToolResult,
};
pub use task::{RunTaskConfig, RunTaskError, StepOutcome, TaskOutcome, run_task};
pub use worker::{WorkerAgent, WorkerError, WorkerPersona};
