//! What kind of agent this is — used by routing / inter-agent flows to
//! decide who-talks-to-whom. Mirrors the variants `tars-runtime` grew on
//! its (lower-level, single-call) agent; the model owns the canonical
//! definition now.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRole {
    /// Plans + delegates: decomposes a Task into sub-Tasks for others.
    Orchestrator,
    /// Executes one domain task. `domain` is free-form (`"code_review"`,
    /// `"fix"`, `"security_audit"`) and lets routing pick the right agent.
    Worker { domain: String },
    /// Judges another agent's output (advisory; never a hard gate).
    Critic,
}

impl AgentRole {
    pub fn worker(domain: impl Into<String>) -> Self {
        Self::Worker {
            domain: domain.into(),
        }
    }

    /// One-word class for logs / routing tables.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::Worker { .. } => "worker",
            Self::Critic => "critic",
        }
    }
}
