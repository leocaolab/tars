//! Caller identity. See `docs/10-security-model.md` §4.
//!
//! `Principal` is the *who*; `Scope` is the *what they can do*.
//! Provider layer doesn't enforce these — that's the Pipeline IAM
//! middleware's job (Doc 02 §4.2). We carry them through so layers
//! that need them (cache key construction, audit) have access.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::ids::{PrincipalId, TenantId};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Principal {
    pub id: PrincipalId,
    pub tenant: TenantId,
    pub display_name: String,
    pub kind: PrincipalKind,
    pub scopes: Vec<Scope>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrincipalKind {
    HumanUser { email: Option<String> },
    ServiceAccount { description: String },
    /// A subprocess acting on behalf of a parent principal with a
    /// reduced scope set. Used for CLI / MCP integrations (Doc 10 §4.1).
    DelegatedSubprocess {
        parent: PrincipalId,
        scope_subset: Vec<Scope>,
    },
}

/// A grant of permission. The *exact* shape of a Scope is intentionally
/// open: different IAM backends (RBAC / ABAC / OPA) project their rules
/// onto this representation. The Provider layer never inspects scope
/// bodies — it only forwards them so the Cache layer can mix them into
/// the cache key (Doc 03 §3.2).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Scope {
    /// Resource the scope applies to, e.g. `tenant:acme:repo:tars`.
    pub resource: String,
    /// Action set — e.g. `["read", "invoke"]`. Sorted for stable hashing.
    pub actions: Vec<String>,
}

impl Scope {
    pub fn new(resource: impl Into<String>, actions: Vec<String>) -> Self {
        let mut actions = actions;
        actions.sort();
        actions.dedup();
        Self { resource: resource.into(), actions }
    }
}

impl Principal {
    /// Compute the deduplicated set of *granted* scope identifiers.
    /// Used by the cache key factory to make hash buckets per-IAM-view.
    pub fn scope_keys(&self) -> Vec<String> {
        let mut set: HashSet<String> = HashSet::new();
        for s in &self.scopes {
            for a in &s.actions {
                set.insert(format!("{}:{}", s.resource, a));
            }
        }
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_keys_are_sorted_and_deduped() {
        let p = Principal {
            id: PrincipalId::new("u1"),
            tenant: TenantId::new("t"),
            display_name: "alice".into(),
            kind: PrincipalKind::HumanUser { email: None },
            scopes: vec![
                Scope::new("repo:foo", vec!["read".into(), "write".into()]),
                Scope::new("repo:bar", vec!["read".into()]),
                // Duplicate — should collapse.
                Scope::new("repo:foo", vec!["read".into()]),
            ],
        };
        assert_eq!(
            p.scope_keys(),
            vec![
                "repo:bar:read".to_string(),
                "repo:foo:read".to_string(),
                "repo:foo:write".to_string(),
            ]
        );
    }
}
