//! Per-request context shared across pipeline layers.
//!
//! Deliberately minimal at the Provider layer. Full `RequestContext`
//! (with budget handle, attributes, etc.) lives in `tars-pipeline`;
//! providers only need IDs + cancel + deadline.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub use tokio_util::sync::CancellationToken;

use crate::ids::{PrincipalId, SessionId, TenantId, TraceId};

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub trace_id: TraceId,
    pub tenant_id: TenantId,
    pub session_id: SessionId,
    pub principal_id: PrincipalId,
    /// Hard deadline. None = no deadline (rare in production).
    pub deadline: Option<Instant>,
    /// Cooperative cancellation. Anyone holding this can cancel; long
    /// awaits in adapters must `select!` against `cancel.cancelled()`.
    pub cancel: CancellationToken,
    /// Free-form attributes used by middleware to pass values to inner
    /// layers without bloating the strongly-typed fields.
    pub attributes: Arc<RwLock<HashMap<String, serde_json::Value>>>,
}

impl RequestContext {
    /// A test/dev context — fresh trace, no deadline, no real principal.
    /// **Do not use in production** — there's no IAM/audit attached.
    pub fn test_default() -> Self {
        Self {
            trace_id: TraceId::new("trace-test"),
            tenant_id: TenantId::new("tenant-test"),
            session_id: SessionId::new("session-test"),
            principal_id: PrincipalId::new("principal-test"),
            deadline: None,
            cancel: CancellationToken::new(),
            attributes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}
