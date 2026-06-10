//! [`PermissionView`] — the dispatch gate's decision source, kept leaf-level.
//!
//! `tars-tools` must stay a leaf crate (it depends only on `tars-types`), so it
//! cannot reach for `tars_model::Permissions`. The dispatch gate
//! ([`crate::ToolRegistry::dispatch`]) therefore consults a thin
//! [`PermissionView`] instead — any `Fn(&str) -> ToolDecision`. The runtime
//! adapts its real `Permissions` into one of these at the call site. `None` on
//! [`ToolContext`](crate::ToolContext) means allow-all (the historical default).

/// What the policy decides for a tool invocation. Mirrors `tars_model::Decision`
/// but lives here so the tool layer doesn't depend on `tars-model`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    /// Run it.
    Allow,
    /// Reject up front.
    Deny,
    /// Defer to a human (see [`crate::ApprovalSink`]).
    Ask,
}

/// A decision source the dispatch gate consults by tool name. Blanket-impl'd for
/// any `Fn(&str) -> ToolDecision`, so the runtime passes a closure that adapts
/// its `Permissions` without this crate knowing about `tars-model`.
pub trait PermissionView: Send + Sync {
    fn decide(&self, tool: &str) -> ToolDecision;
}

impl<F> PermissionView for F
where
    F: Fn(&str) -> ToolDecision + Send + Sync,
{
    fn decide(&self, tool: &str) -> ToolDecision {
        self(tool)
    }
}
