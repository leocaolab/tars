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
mod critic;
mod error;
mod event;
mod message;
mod orchestrator;
mod prompt;
mod runtime;
mod task;
mod worker;

pub use agent::{
    Agent, AgentContext, AgentError, AgentOutput, AgentRole, AgentStepResult, SingleShotAgent,
};
pub use critic::{CriticAgent, CriticError, PartialResultRef};
pub use error::RuntimeError;
pub use event::{AgentEvent, StepIdempotencyKey};
pub use message::{AgentMessage, VerdictKind};
pub use orchestrator::{OrchestratorAgent, OrchestratorError, Plan, PlanStep};
pub use prompt::PromptBuilder;
pub use runtime::{execute_agent_step, AgentExecutionError, LocalRuntime, Runtime};
pub use task::{run_task, RunTaskConfig, RunTaskError, StepOutcome, TaskOutcome};
pub use worker::{WorkerAgent, WorkerError};
