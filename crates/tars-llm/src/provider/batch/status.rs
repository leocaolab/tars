//! Describes a vendor batch job (Anthropic `messages/batches`, OpenAI `batches`) in
//! vendor-neutral terms: the status a caller polls and the per-item results it fetches once the
//! job is done. Submitting and polling are [`BatchSubmitter`](super::BatchSubmitter)'s.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use tars_types::{BatchItemId, ChatResponse};

use crate::ProviderError;

/// The state of a batch job, the union of Anthropic's and OpenAI's statuses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BatchStatus {
    /// Submitted and accepted; not yet started processing.
    Submitted,
    /// The vendor is processing. `processed` is 0 and `total` is `None` when the vendor does not
    /// report them; OpenAI reports them only in some states.
    InProgress {
        #[serde(default)]
        processed: u32,
        #[serde(default)]
        total: Option<u32>,
        /// When the vendor estimates the job will finish; `None` when it does not say.
        #[serde(default)]
        eta: Option<SystemTime>,
    },
    /// Terminal — every item finished, though each may have failed on its own (see
    /// [`BatchResultItem::result`]).
    Completed,
    /// Terminal — the whole job failed before producing per-item results (auth, quota, or a
    /// malformed batch).
    Failed { kind: String, message: String },
    /// Terminal — the job expired unfinished; both vendors expire jobs after 24 h.
    Expired,
    /// Terminal — caller cancelled.
    Cancelled,
}

impl BatchStatus {
    /// Returns whether the job will report no further status.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed { .. } | Self::Expired | Self::Cancelled
        )
    }
}

/// One item's result inside a completed batch. `item_id` is the one the caller supplied at
/// submit, so outputs match inputs without relying on list order.
#[derive(Debug)]
pub struct BatchResultItem {
    pub item_id: BatchItemId,
    /// The item's own outcome; an item can fail while the job stays `Completed`.
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
