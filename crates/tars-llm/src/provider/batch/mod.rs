//! Defines [`BatchSubmitter`], the submit / poll / fetch surface of the vendors' batch APIs
//! (Anthropic `messages/batches`, OpenAI `batches`), which does not fit [`LlmProvider::stream`].
//!
//! It does not poll on a schedule, store job ids, mix batch and direct calls, or retry failed
//! items: each of those is the caller's decision.
//!
//! [`LlmProvider::stream`]: crate::LlmProvider::stream

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use tars_types::{BatchItemId, BatchJobId, Caller, ChatRequest, ChatResponse};

use crate::ProviderError;
use crate::provider::error::{Feature, RequestRefusal};

mod status;

pub use status::{BatchResultItem, BatchStatus};

/// A vendor batch API, which typically halves the price for up to a 24-hour turnaround. A
/// provider that has one returns it from [`crate::LlmProvider::as_batch_submitter`].
#[async_trait]
pub trait BatchSubmitter: Send + Sync + 'static {
    /// Submits `items` against `model` and returns the vendor's job id. Each result carries its
    /// item's [`BatchItemId`], so results match inputs in any order. `ctx` is the caller's, since
    /// the auth resolver may scope the credential to its tenant.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Auth`] for a bad credential, [`ProviderError::InvalidRequest`] for a
    /// malformed item or a batch over the vendor's limits, [`ProviderError::Network`] for a
    /// transport failure.
    async fn submit(
        &self,
        items: Vec<(BatchItemId, ChatRequest)>,
        model: &str,
        ctx: &Caller,
    ) -> Result<BatchJobId, ProviderError>;

    /// Returns the job's current status; it changes nothing, so it may be called as often as the
    /// vendor's rate limit allows.
    async fn status(&self, id: &BatchJobId, ctx: &Caller) -> Result<BatchStatus, ProviderError>;

    /// Returns the per-item results of a finished job. On a job that has not finished an
    /// implementation should return `InvalidRequest`, not wait.
    async fn results(
        &self,
        id: &BatchJobId,
        ctx: &Caller,
    ) -> Result<Vec<BatchResultItem>, ProviderError>;

    /// Cancels the job where the vendor supports it (Anthropic); elsewhere, and by default, it
    /// returns `InvalidRequest`.
    async fn cancel(&self, id: &BatchJobId, ctx: &Caller) -> Result<(), ProviderError> {
        let _ = id;
        let _ = ctx;
        Err(ProviderError::InvalidRequest(RequestRefusal::Unsupported(
            Feature::BatchCancel,
        )))
    }
}

// ─── MockBatchSubmitter — for consumer tests ──────────────────────────

/// An in-memory [`BatchSubmitter`] for testing code that uses one. A submitted job is
/// `Completed` at once, and its results are one `"ok"` response per item, unless a test sets
/// them with [`Self::set_status`] and [`Self::set_results`].
pub struct MockBatchSubmitter {
    state: Mutex<MockState>,
}

struct MockState {
    next_job_seq: u64,
    jobs: std::collections::HashMap<BatchJobId, MockJob>,
}

struct MockJob {
    items: Vec<(BatchItemId, ChatRequest)>,
    /// The model the results name.
    model: String,
    status: BatchStatus,
    /// The results [`MockBatchSubmitter::set_results`] set; `None` means one `"ok"` per item.
    custom_results: Option<Vec<BatchResultItem>>,
}

impl Default for MockBatchSubmitter {
    fn default() -> Self {
        Self {
            state: Mutex::new(MockState {
                next_job_seq: 1,
                jobs: std::collections::HashMap::new(),
            }),
        }
    }
}

impl MockBatchSubmitter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sets the status `status()` reports for `id` from now on; an unknown `id` is ignored.
    pub async fn set_status(&self, id: &BatchJobId, status: BatchStatus) {
        if let Some(job) = self.state.lock().await.jobs.get_mut(id) {
            job.status = status;
        }
    }

    /// Sets what `results()` returns for `id` from now on; an unknown `id` is ignored.
    pub async fn set_results(&self, id: &BatchJobId, results: Vec<BatchResultItem>) {
        if let Some(job) = self.state.lock().await.jobs.get_mut(id) {
            job.custom_results = Some(results);
        }
    }
}

#[async_trait]
impl BatchSubmitter for MockBatchSubmitter {
    async fn submit(
        &self,
        items: Vec<(BatchItemId, ChatRequest)>,
        model: &str,
        _ctx: &Caller,
    ) -> Result<BatchJobId, ProviderError> {
        let mut state = self.state.lock().await;
        let seq = state.next_job_seq;
        state.next_job_seq += 1;
        let job_id: BatchJobId = format!("mock-batch-{seq:04}")
            .parse()
            .expect("the \"mock-batch-\" prefix makes the id non-empty");
        state.jobs.insert(
            job_id.clone(),
            MockJob {
                items,
                model: model.to_string(),
                status: BatchStatus::Completed,
                custom_results: None,
            },
        );
        Ok(job_id)
    }

    async fn status(&self, id: &BatchJobId, _ctx: &Caller) -> Result<BatchStatus, ProviderError> {
        match self.state.lock().await.jobs.get(id) {
            Some(job) => Ok(job.status.clone()),
            None => Err(ProviderError::InvalidRequest(
                RequestRefusal::UnknownBatchJob { id: id.clone() },
            )),
        }
    }

    async fn results(
        &self,
        id: &BatchJobId,
        _ctx: &Caller,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        let state = self.state.lock().await;
        let job = state.jobs.get(id).ok_or_else(|| {
            ProviderError::InvalidRequest(RequestRefusal::UnknownBatchJob { id: id.clone() })
        })?;
        if !job.status.is_terminal() {
            return Err(ProviderError::InvalidRequest(
                RequestRefusal::BatchNotTerminal {
                    id: id.clone(),
                    status: job.status.clone(),
                },
            ));
        }
        if let Some(custom) = &job.custom_results {
            // `ProviderError` is not `Clone`, so each error is rebuilt.
            return Ok(custom
                .iter()
                .map(|item| BatchResultItem {
                    item_id: item.item_id.clone(),
                    result: match &item.result {
                        Ok(resp) => Ok(resp.clone()),
                        Err(e) => Err(clone_provider_error(e)),
                    },
                })
                .collect());
        }
        Ok(job
            .items
            .iter()
            .map(|(item_id, _req)| BatchResultItem {
                item_id: item_id.clone(),
                result: Ok(echo_response(&job.model)),
            })
            .collect())
    }
}

fn echo_response(model: &str) -> ChatResponse {
    use tars_types::{ChatEvent, ChatResponseBuilder, StopReason, Usage};
    let mut acc = ChatResponseBuilder::new();
    acc.apply(ChatEvent::started(model));
    acc.apply(ChatEvent::Delta { text: "ok".into() });
    acc.apply(ChatEvent::Finished {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    });
    acc.finish()
}

fn clone_provider_error(e: &ProviderError) -> ProviderError {
    // Keeps only the text: the variant becomes `Internal`.
    ProviderError::Internal(format!("mock-clone of {}", tars_utils::display_chain(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::ProviderId;
    use tars_types::{Caller, ChatRequest};

    fn req(text: &str) -> ChatRequest {
        ChatRequest::user(text)
    }

    fn ctx() -> Caller {
        Caller::test_default()
    }

    #[tokio::test]
    async fn submit_assigns_unique_job_ids() {
        let m = MockBatchSubmitter::new();
        let id1 = m
            .submit(
                vec![(BatchItemId::new("a"), req("hello"))],
                "test-model",
                &ctx(),
            )
            .await
            .unwrap();
        let id2 = m
            .submit(
                vec![(BatchItemId::new("b"), req("world"))],
                "test-model",
                &ctx(),
            )
            .await
            .unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn status_after_submit_is_completed_by_default() {
        let m = MockBatchSubmitter::new();
        let id = m
            .submit(
                vec![(BatchItemId::new("a"), req("x"))],
                "test-model",
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(m.status(&id, &ctx()).await.unwrap(), BatchStatus::Completed);
    }

    #[tokio::test]
    async fn status_unknown_job_errors() {
        let m = MockBatchSubmitter::new();
        let err = m
            .status(&BatchJobId::new("does-not-exist"), &ctx())
            .await
            .expect_err("must error");
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn results_default_echoes_items_one_to_one() {
        let m = MockBatchSubmitter::new();
        let items = vec![
            (BatchItemId::new("draft-1"), req("input 1")),
            (BatchItemId::new("draft-2"), req("input 2")),
            (BatchItemId::new("draft-3"), req("input 3")),
        ];
        let id = m.submit(items.clone(), "test-model", &ctx()).await.unwrap();
        let results = m.results(&id, &ctx()).await.unwrap();
        assert_eq!(results.len(), 3);
        for (input, output) in items.iter().zip(results.iter()) {
            assert_eq!(input.0, output.item_id);
            assert!(output.result.is_ok());
        }
    }

    #[tokio::test]
    async fn results_non_terminal_status_errors() {
        let m = MockBatchSubmitter::new();
        let id = m
            .submit(
                vec![(BatchItemId::new("a"), req("x"))],
                "test-model",
                &ctx(),
            )
            .await
            .unwrap();
        m.set_status(
            &id,
            BatchStatus::InProgress {
                processed: 0,
                total: Some(1),
                eta: None,
            },
        )
        .await;
        let err = m
            .results(&id, &ctx())
            .await
            .expect_err("must reject results() on non-terminal");
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn set_status_drives_polling_simulation() {
        let m = MockBatchSubmitter::new();
        let id = m
            .submit(
                vec![(BatchItemId::new("a"), req("x"))],
                "test-model",
                &ctx(),
            )
            .await
            .unwrap();
        m.set_status(&id, BatchStatus::Submitted).await;
        assert!(!m.status(&id, &ctx()).await.unwrap().is_terminal());
        m.set_status(
            &id,
            BatchStatus::InProgress {
                processed: 5,
                total: Some(10),
                eta: None,
            },
        )
        .await;
        assert!(!m.status(&id, &ctx()).await.unwrap().is_terminal());
        m.set_status(&id, BatchStatus::Completed).await;
        assert!(m.status(&id, &ctx()).await.unwrap().is_terminal());
    }

    #[tokio::test]
    async fn cancel_default_returns_unsupported() {
        let m = MockBatchSubmitter::new();
        let id = m
            .submit(
                vec![(BatchItemId::new("a"), req("x"))],
                "test-model",
                &ctx(),
            )
            .await
            .unwrap();
        let err = m
            .cancel(&id, &ctx())
            .await
            .expect_err("default unsupported");
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn provider_default_returns_none_for_batch_submitter() {
        use crate::provider::LlmProvider;
        use crate::provider::backends::mock::{CannedResponse, MockProvider};

        let p: Arc<dyn LlmProvider> =
            MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        assert!(p.as_batch_submitter().is_none());
    }
}
