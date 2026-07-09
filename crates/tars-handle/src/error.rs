//! Typed error for role resolution ([`crate::roles`]).
//!
//! The old per-scope `Tars` handle carried registry / store / workspace-config
//! / io failures too; those paths were deleted with the scope facade. What
//! survives is the standalone role→provider resolver, whose only failure mode
//! is "this role maps to no provider".

use thiserror::Error;

use tars_types::ProviderId;

#[derive(Debug, Error)]
pub enum TarsError {
    /// A `role` did not resolve to any provider — not in the flat `[roles]`
    /// map, not a known tier, not a literal provider id, no `default` tier
    /// candidate, and the registry isn't a single-provider registry to fall
    /// back to. Carries the role and the resolved provider id (if any) so the
    /// message is actionable.
    #[error(
        "role `{role}` maps to no provider — add a `[roles]` entry \
         (`{role} = \"<provider>\"`), name a provider id directly, or declare a \
         `default` tier{}",
        .tried.as_ref().map(|p| format!(" (tried provider id `{p}`)")).unwrap_or_default()
    )]
    UnknownRole {
        role: String,
        tried: Option<ProviderId>,
    },
}
