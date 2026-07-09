//! [`Task`] — the recursive unit of intent handed to an [`Agent`](crate::Agent).
//!
//! Doc 20 §4: a Task originates from user input (the root goal) but is NOT
//! the raw input — it's the structured unit an Agent consumes. An
//! orchestrator may decompose a Task into sub-Tasks for sub-agents;
//! `parent` records that lineage. A Task is implementation-agnostic: it is
//! "what to do", never LLM messages (the `Task → prompt → ChatRequest`
//! translation is a native agent's internal job).

use serde::{Deserialize, Serialize};

use crate::ids::TaskId;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    /// Identity of this task (supplied by the orchestrator/runtime).
    pub id: TaskId,
    /// What to accomplish.
    pub goal: String,
    /// Relevant context/data the agent needs (a file path, a PR diff,
    /// upstream findings). Named bag so an agent can pick what it uses.
    #[serde(default)]
    pub inputs: Vec<TaskInput>,
    /// What "done" means — optional acceptance criteria / constraints.
    /// The hook for deterministic acceptance gating (see the autonomous-
    /// architect line: a task's "done" should be a checkable assertion).
    #[serde(default)]
    pub acceptance: Option<String>,
    /// The Task this was decomposed from. `None` for a root task.
    #[serde(default)]
    pub parent: Option<TaskId>,
}

impl Task {
    /// A root task: an id + a goal, no parent.
    pub fn new(id: impl Into<TaskId>, goal: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            goal: goal.into(),
            inputs: Vec::new(),
            acceptance: None,
            parent: None,
        }
    }

    /// Attach an input. Chainable.
    pub fn with_input(mut self, name: impl Into<String>, value: serde_json::Value) -> Self {
        self.inputs.push(TaskInput {
            name: name.into(),
            value,
        });
        self
    }

    /// Set acceptance criteria. Chainable.
    pub fn with_acceptance(mut self, acceptance: impl Into<String>) -> Self {
        self.acceptance = Some(acceptance.into());
        self
    }

    /// Mark this task as decomposed from `parent`. Chainable.
    pub fn child_of(mut self, parent: impl Into<TaskId>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// Look up an input by name.
    pub fn input(&self, name: &str) -> Option<&serde_json::Value> {
        self.inputs
            .iter()
            .find(|i| i.name == name)
            .map(|i| &i.value)
    }
}

/// One named piece of context attached to a [`Task`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskInput {
    pub name: String,
    pub value: serde_json::Value,
}
