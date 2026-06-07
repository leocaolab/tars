//! [`Agent`] — the user-facing agent abstraction (Doc 20 §1).
//!
//! > An Agent is a collection of capabilities (skills) that a user can hand
//! > a task to.
//!
//! THE thing called "agent". Everything in `tars-runtime` (the single-call
//! `trait Agent`, `Session`, `LlmWorker`, `WorkerAgent`) is plumbing BELOW
//! this — what a native agent happens to use, not the agent itself.

use async_trait::async_trait;

use tars_types::Usage;

use crate::context::AgentContext;
use crate::ids::AgentId;
use crate::role::AgentRole;
use crate::skill::SkillSet;
use crate::task::Task;

/// The result of an Agent running a Task. Any side effects (file edits,
/// commands) already happened out-of-band in `ctx.cwd`; this is the
/// reportable result + accounting, not the side effect itself.
#[derive(Clone, Debug, Default)]
pub struct AgentOutput {
    /// Human/agent-readable result of the task.
    pub summary: String,
    /// Optional structured result for machine consumers.
    pub data: Option<serde_json::Value>,
    /// Token accounting (`Usage::default()` for non-LLM agents).
    pub usage: Usage,
}

impl AgentOutput {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            data: None,
            usage: Usage::default(),
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }
}

/// Why an Agent failed to complete a Task. Distinct from a Task that ran
/// and produced a "the work didn't pan out" result — that's a successful
/// `AgentOutput` the caller interprets.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// Cancelled mid-run (an upstream Drop / SIGINT).
    #[error("cancelled")]
    Cancelled,
    /// The task itself was malformed / unsatisfiable as stated.
    #[error("bad task: {0}")]
    BadTask(String),
    /// A required capability was not permitted for this agent.
    #[error("denied: {0}")]
    Denied(String),
    /// The agent tried and failed (LLM error, tool failure, subprocess
    /// died, …).
    #[error("execution failed: {0}")]
    Execution(String),
}

impl AgentError {
    /// One-word classification for logs / metrics.
    pub fn classification(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::BadTask(_) => "bad_task",
            Self::Denied(_) => "denied",
            Self::Execution(_) => "execution",
        }
    }
}

/// An Agent: a capability set you hand a Task to.
///
/// Implementations come in two kinds, both conforming to THIS interface so
/// the orchestration (routing / ensemble / run_plan / events) treats them
/// uniformly:
///   - **native** — built on an LLM: `run` internally turns the Task into
///     prompts and drives a Session loop over a pure-inference provider +
///     the tools its skills name. White-box (tars owns the loop).
///   - **user** — whatever `run` the user writes.
///
/// `run` takes a [`Task`] (user-level intent), NOT a `ChatRequest`: a
/// request is LLM-message-level, the internal detail of how a *native*
/// agent turns a task into calls. A user agent that doesn't use an LLM must
/// never see one.
#[async_trait]
pub trait Agent: Send + Sync {
    /// Stable id for this agent instance.
    fn id(&self) -> &AgentId;

    /// What kind of agent this is — routing / inter-agent flows use it.
    fn role(&self) -> &AgentRole;

    /// The capabilities this agent has — what it IS.
    fn skills(&self) -> &SkillSet;

    /// Hand it a task; it does it.
    async fn run(&self, task: Task, ctx: AgentContext) -> Result<AgentOutput, AgentError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TaskId;
    use crate::skill::Skill;

    /// Smallest agent that exercises the trait surface — echoes the goal.
    struct EchoAgent {
        id: AgentId,
        role: AgentRole,
        skills: SkillSet,
    }

    #[async_trait]
    impl Agent for EchoAgent {
        fn id(&self) -> &AgentId {
            &self.id
        }
        fn role(&self) -> &AgentRole {
            &self.role
        }
        fn skills(&self) -> &SkillSet {
            &self.skills
        }
        async fn run(
            &self,
            task: Task,
            _ctx: AgentContext,
        ) -> Result<AgentOutput, AgentError> {
            Ok(AgentOutput::new(format!("did: {}", task.goal)))
        }
    }

    #[tokio::test]
    async fn a_user_agent_takes_a_task_and_returns_output() {
        let agent = EchoAgent {
            id: AgentId::new("echo-1"),
            role: AgentRole::worker("test"),
            skills: SkillSet::new().with(Skill::new("noop", "does nothing")),
        };
        assert_eq!(agent.id().as_str(), "echo-1");
        assert_eq!(agent.role().kind(), "worker");
        assert!(agent.skills().contains("noop"));

        let task = Task::new(TaskId::new("t1"), "say hi");
        let out = agent.run(task, AgentContext::new()).await.unwrap();
        assert_eq!(out.summary, "did: say hi");
    }
}
