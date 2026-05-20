//! Batch-mode types — `BatchStatus`, `BatchResultItem`.
//!
//! Batch APIs (Anthropic `messages/batches`, OpenAI `batches`) submit
//! many requests for offline processing at ~50% of sync pricing with
//! up to a 24 h SLA. The shape is fundamentally different from
//! streaming chat completion:
//!
//! - **Submit** returns a `BatchJobId`; no streaming, no immediate response.
//! - **Status** is polled — vendor reports progress and terminal state.
//! - **Results** are fetched as a list when the job is done.
//!
//! These types are the cross-vendor abstraction. The trait that
//! consumes them lives in `tars-provider::BatchSubmitter`.
//!
//! See [`docs/roadmap.md §5`](../../../docs/roadmap.md) for the design.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::ids::BatchItemId;
use crate::response::ChatResponse;

/// Terminal and in-flight states a batch job can be in. Mirrors the
/// union of Anthropic + OpenAI vendor-reported statuses, collapsed
/// into a vendor-neutral shape.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BatchStatus {
    /// Submitted and accepted; not yet started processing.
    Submitted,
    /// Vendor is processing. `processed` / `total` populated when the
    /// vendor reports it (Anthropic does, OpenAI does in some states);
    /// `None` for `total` means "unknown / not reported."
    InProgress {
        processed: u32,
        total: Option<u32>,
        /// When the vendor estimates completion. `None` if not reported.
        eta: Option<SystemTime>,
    },
    /// Terminal — all items completed (each item may itself have failed;
    /// check `BatchResultItem::result` per-item).
    Completed,
    /// Terminal — entire job failed before producing per-item results
    /// (auth / quota / malformed input on the batch level).
    Failed { kind: String, message: String },
    /// Terminal — job expired before completion (Anthropic / OpenAI both
    /// expire un-finished jobs after 24 h).
    Expired,
    /// Terminal — caller cancelled.
    Cancelled,
}

impl BatchStatus {
    /// True iff no further status transitions are expected.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed { .. } | Self::Expired | Self::Cancelled
        )
    }
}

/// One item's result inside a completed batch. `item_id` matches what
/// the caller supplied at `submit()` time so callers can correlate
/// outputs to inputs without relying on list order.
#[derive(Debug)]
pub struct BatchResultItem {
    pub item_id: BatchItemId,
    /// Per-item outcome. Vendor batch APIs report per-item failures
    /// (e.g. one bad request in a 10k-item batch) — these surface here
    /// while the overall job status stays `Completed`.
    pub result: Result<ChatResponse, ProviderError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_classification() {
        assert!(BatchStatus::Completed.is_terminal());
        assert!(BatchStatus::Expired.is_terminal());
        assert!(BatchStatus::Cancelled.is_terminal());
        assert!(
            BatchStatus::Failed {
                kind: "auth".into(),
                message: "bad key".into()
            }
            .is_terminal()
        );

        assert!(!BatchStatus::Submitted.is_terminal());
        assert!(
            !BatchStatus::InProgress {
                processed: 1,
                total: Some(10),
                eta: None
            }
            .is_terminal()
        );
    }

    #[test]
    fn serde_roundtrip_in_progress() {
        let s = BatchStatus::InProgress {
            processed: 42,
            total: Some(100),
            eta: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["status"], "in_progress");
        let back: BatchStatus = serde_json::from_value(v).unwrap();
        assert_eq!(s, back);
    }
}
