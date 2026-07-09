//! [`Permissions`] — what an Agent is ALLOWED to do.
//!
//! The policy that gates an agent's skills. Orthogonal to the
//! [`SkillSet`](crate::SkillSet): skills are what it CAN do, permissions
//! are what it MAY do. A native agent consults this before letting its
//! Session invoke a tool; a user agent may consult it however it sees fit.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What the policy decides for a given skill/capability invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Run it.
    Allow,
    /// Reject it up front.
    Deny,
    /// Defer to a human / higher authority (the runtime decides how to
    /// surface the prompt; the model just records the intent).
    Ask,
}

/// Per-skill allow/deny/ask policy with a fallback for unlisted skills.
///
/// `Default` is permissive (`default = Allow`, no rules) — tighten
/// explicitly. Construct a locked-down policy with [`Permissions::deny_all`]
/// then `.allow(..)` the few skills you want.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Permissions {
    rules: BTreeMap<String, Decision>,
    default: Decision,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            rules: BTreeMap::new(),
            default: Decision::Allow,
        }
    }
}

impl Permissions {
    /// Allow everything unless a rule says otherwise.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Deny everything unless a rule says otherwise (allow-list mode).
    pub fn deny_all() -> Self {
        Self {
            rules: BTreeMap::new(),
            default: Decision::Deny,
        }
    }

    pub fn allow(mut self, skill: impl Into<String>) -> Self {
        self.rules.insert(skill.into(), Decision::Allow);
        self
    }

    pub fn deny(mut self, skill: impl Into<String>) -> Self {
        self.rules.insert(skill.into(), Decision::Deny);
        self
    }

    pub fn ask(mut self, skill: impl Into<String>) -> Self {
        self.rules.insert(skill.into(), Decision::Ask);
        self
    }

    /// The decision for `skill` — its rule if present, else the default.
    pub fn decide(&self, skill: &str) -> Decision {
        self.rules.get(skill).copied().unwrap_or(self.default)
    }

    /// Convenience: is this skill outright runnable (no Ask, no Deny)?
    pub fn is_allowed(&self, skill: &str) -> bool {
        self.decide(skill) == Decision::Allow
    }
}
