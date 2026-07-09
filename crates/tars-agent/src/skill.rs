//! [`Skill`] / [`SkillSet`] — what an Agent CAN do.
//!
//! Doc 05 distinguishes Skill (composite capability) from Tool (atomic).
//! At the MODEL level a Skill is just the named capability — its identity
//! and intent. The concrete tools / sub-steps that back it live in the
//! implementation layer (a native agent maps its skills onto a
//! `ToolRegistry`); the model stays implementation-agnostic.
//!
//! A skill is WHAT the agent can do; whether it's ALLOWED to is the
//! separate [`Permissions`](crate::Permissions) policy. "Can edit" ≠
//! "allowed to edit".

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    /// Stable name — the key permissions + routing reference. Convention
    /// mirrors tools: `category.action` (`fs.edit`, `code.review`).
    pub name: String,
    /// One line: what the capability is / when it applies.
    pub description: String,
}

impl Skill {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// The set of capabilities an Agent has — this is what the agent IS
/// (Doc 20 §1). Order-preserving + dedup-by-name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkillSet {
    skills: Vec<Skill>,
}

impl SkillSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a skill (no-op if a skill of the same name is already present).
    /// Chainable.
    pub fn with(mut self, skill: Skill) -> Self {
        if !self.contains(&skill.name) {
            self.skills.push(skill);
        }
        self
    }

    pub fn contains(&self, name: &str) -> bool {
        self.skills.iter().any(|s| s.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter()
    }

    pub fn names(&self) -> Vec<&str> {
        self.skills.iter().map(|s| s.name.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }
}

impl FromIterator<Skill> for SkillSet {
    fn from_iter<I: IntoIterator<Item = Skill>>(iter: I) -> Self {
        let mut set = SkillSet::new();
        for s in iter {
            set = set.with(s);
        }
        set
    }
}
