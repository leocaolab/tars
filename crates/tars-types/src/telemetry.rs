//! Defines what one call's layers measured on its way through the middleware stack. Each layer
//! hands out only its own measurement, as a [`ChatEvent::Measured`](crate::ChatEvent::Measured)
//! before the stream's terminal event or with the error a call failed to open with; a consumer
//! folds the parts with [`CallTelemetry::merge`]. It holds per-call facts a caller may log or
//! show; trace events go to tracing, not here.

use serde::{Deserialize, Serialize};

/// What the layers of one call measured, or one layer's part of it. Every field defaults to
/// "nothing measured", so a layer's part names only what that layer measured.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CallTelemetry {
    /// Retries after the first attempt; 0 means the first try succeeded.
    pub retry_count: u32,

    /// One entry per failed attempt that was retried, oldest first.
    pub retry_attempts: Vec<RetryAttempt>,

    /// Wall time inside the provider's call, including draining the stream, summed over every
    /// attempt; `None` when the provider terminal did not record it.
    pub provider_latency_ms: Option<u64>,

    /// Wall time through the whole pipeline, including middleware, retries and stream drain;
    /// measured by `TelemetryMiddleware` when it is in the stack.
    pub pipeline_total_ms: Option<u64>,
}

impl CallTelemetry {
    /// Returns the fold of `self` and a later part `next`: attempts and retries add up, provider
    /// time sums over attempts, and the pipeline time is the later one measured.
    #[must_use]
    pub fn merge(self, next: CallTelemetry) -> CallTelemetry {
        let mut retry_attempts = self.retry_attempts;
        retry_attempts.extend(next.retry_attempts);
        CallTelemetry {
            retry_count: self.retry_count.saturating_add(next.retry_count),
            retry_attempts,
            provider_latency_ms: match (self.provider_latency_ms, next.provider_latency_ms) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (a, b) => a.or(b),
            },
            pipeline_total_ms: next.pipeline_total_ms.or(self.pipeline_total_ms),
        }
    }

    /// Returns whether `retry_count` equals `retry_attempts.len()`; the two hold one fact and
    /// drift when a writer updates only one.
    pub fn retries_consistent(&self) -> bool {
        self.retry_count as usize == self.retry_attempts.len()
    }

    /// Returns the retry count from the attempt log; read this rather than `retry_count`.
    pub fn retries(&self) -> usize {
        self.retry_attempts.len()
    }
}

/// One failed attempt that was retried.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryAttempt {
    /// The kind of the provider error that caused the retry.
    pub error_kind: crate::providers::error::LlmErrorKind,
    /// The wait the retry middleware actually slept before the next attempt, whether it came
    /// from its backoff policy or the provider's `Retry-After`.
    pub retry_after_ms: Option<u64>,
}
