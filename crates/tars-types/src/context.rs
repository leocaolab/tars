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
use crate::telemetry::{new_shared_telemetry, SharedTelemetry};
use crate::validation::{new_shared_validation_outcome, SharedValidationOutcome};

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
    /// Per-call telemetry accumulator written by middleware and read
    /// by the caller after the response stream completes. See
    /// [`crate::telemetry::TelemetryAccumulator`]. Always present —
    /// middleware writes are unconditional, callers ignore the slot
    /// if they don't need it.
    pub telemetry: SharedTelemetry,
    /// Per-call validation outcome side-channel. `ValidationMiddleware`
    /// writes the aggregated summary + (if any Filter ran) the
    /// post-Filter `ChatResponse`. Caller reads after stream drain
    /// and either uses the filtered response in place of the streamed
    /// one, or substitutes `summary` onto the response builder.
    /// See [`crate::validation::SharedValidationOutcome`].
    pub validation_outcome: SharedValidationOutcome,
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
            telemetry: new_shared_telemetry(),
            validation_outcome: new_shared_validation_outcome(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled() || self.is_deadline_exceeded()
    }

    /// True iff a deadline is set and `Instant::now()` has passed it.
    /// Kept separate from `is_cancelled()` for callers that want to
    /// distinguish a hard timeout from explicit caller cancellation.
    pub fn is_deadline_exceeded(&self) -> bool {
        match self.deadline {
            Some(d) => Instant::now() >= d,
            None => false,
        }
    }
}
