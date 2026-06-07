//! [`AgentContext`] — the ENVIRONMENT an Agent runs in.
//!
//! Doc 20 §3: the "where / how", separate from the Task's "what".
//! Deliberately implementation-agnostic — NO `LlmService`, NO
//! `ToolRegistry`. A native agent reads `cwd` + `permissions` and builds
//! its own Session internally; a user agent uses whatever it needs. This is
//! the discipline the `tars-model` crate boundary enforces: the contract
//! can't reach into the implementation.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::permission::Permissions;

#[derive(Clone)]
pub struct AgentContext {
    /// Working directory the agent acts on (its worktree). Filesystem
    /// tools resolve relative paths against this; `None` = process cwd.
    pub cwd: Option<PathBuf>,
    /// Cooperative cancellation — a Drop'd parent / SIGINT propagates here.
    pub cancel: CancellationToken,
    /// What the agent is ALLOWED to do (gates its skills).
    pub permissions: Permissions,
    /// Opaque correlation id for observability (trajectory / run). The
    /// model doesn't interpret it; the runtime threads its events under it.
    pub trajectory_id: Option<String>,
}

impl Default for AgentContext {
    fn default() -> Self {
        Self {
            cwd: None,
            cancel: CancellationToken::new(),
            permissions: Permissions::default(),
            trajectory_id: None,
        }
    }
}

impl AgentContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_permissions(mut self, permissions: Permissions) -> Self {
        self.permissions = permissions;
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_trajectory_id(mut self, id: impl Into<String>) -> Self {
        self.trajectory_id = Some(id.into());
        self
    }
}
