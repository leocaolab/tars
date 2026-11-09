//! Holds the per-cap allow / deny / ask policy that [`EffectTierGate`](crate::EffectTierGate)
//! consults before a cap's effect tier.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyVerdict {
    Allow,
    Deny,
    /// Requires approval before the cap runs, whatever its effect tier.
    Ask,
}

/// Maps cap names to decisions; a name with no rule gets the default, which `Default` makes
/// `Allow`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Permissions {
    rules: BTreeMap<String, PolicyVerdict>,
    default: PolicyVerdict,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            rules: BTreeMap::new(),
            default: PolicyVerdict::Allow,
        }
    }
}

impl Permissions {
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Returns a policy that denies every cap no rule names.
    pub fn deny_all() -> Self {
        Self {
            rules: BTreeMap::new(),
            default: PolicyVerdict::Deny,
        }
    }

    pub fn allow(mut self, skill: impl Into<String>) -> Self {
        self.rules.insert(skill.into(), PolicyVerdict::Allow);
        self
    }

    pub fn deny(mut self, skill: impl Into<String>) -> Self {
        self.rules.insert(skill.into(), PolicyVerdict::Deny);
        self
    }

    pub fn ask(mut self, skill: impl Into<String>) -> Self {
        self.rules.insert(skill.into(), PolicyVerdict::Ask);
        self
    }

    pub fn decide(&self, skill: &str) -> PolicyVerdict {
        self.rules.get(skill).copied().unwrap_or(self.default)
    }

    pub fn is_allowed(&self, skill: &str) -> bool {
        self.decide(skill) == PolicyVerdict::Allow
    }
}
