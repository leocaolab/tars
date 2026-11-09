//! Describes who a call is made for. Providers carry a principal and never enforce it: the
//! pipeline's IAM middleware does, and the audit reads it. The model is in
//! `docs/architecture/10-security-model.md` §4.

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
    HumanUser {
        email: Option<String>,
    },
    ServiceAccount {
        description: String,
    },
    DelegatedSubprocess {
        parent: PrincipalId,
        scope_subset: Vec<Scope>,
    },
}

/// A grant of permission, the shape each IAM backend (RBAC / ABAC / OPA) projects its rules
/// onto. Providers never read a scope's body.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub resource: String,
    /// Sorted and deduplicated when built through [`Scope::new`], so it hashes stably.
    pub actions: Vec<String>,
}

impl Scope {
    /// Builds a scope with its actions sorted and deduplicated.
    pub fn new(resource: impl Into<String>, actions: Vec<String>) -> Self {
        let mut actions = actions;
        actions.sort();
        actions.dedup();
        Self {
            resource: resource.into(),
            actions,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

impl Principal {
    /// Returns the scopes this principal acts with; a delegated subprocess acts with its
    /// `scope_subset`, not its parent's `scopes`.
    pub fn effective_scopes(&self) -> &[Scope] {
        match &self.kind {
            PrincipalKind::DelegatedSubprocess { scope_subset, .. } => scope_subset,
            _ => &self.scopes,
        }
    }
}
