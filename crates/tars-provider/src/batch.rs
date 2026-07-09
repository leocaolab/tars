//! [`BatchSubmitter`] trait — sibling to [`LlmProvider`] for batch APIs.
//!
//! Anthropic `messages/batches` and OpenAI `batches` both expose an
//! async submit / poll / fetch shape that doesn't fit the streaming
//! `LlmProvider::stream`. This trait is the cross-vendor abstraction;
//! types ([`BatchStatus`], [`BatchResultItem`]) live in `tars-types`.
//!
//! Phase 1 (roadmap §5) ships the trait + a [`MockBatchSubmitter`] for
//! consumers to test against. Vendor implementations (Anthropic,
//! OpenAI) follow in Phase 2/3 — see
//! [`docs/roadmap.md §5`](../../../../docs/roadmap.md).
//!
//! # What this trait does *not* do
//!
//! - **No scheduling.** Caller polls `status()` on whatever cadence
//!   they want; tars doesn't run a poll loop for you.
//! - **No persistence of `BatchJobId`.** Caller is responsible for
//!   storing the ID so they can fetch results later (DB, Redis, file).
//! - **No mixing batch + sync in one logical call.** Two surfaces, two
//!   call paths. Callers route between them.
//! - **No auto-retry of failed jobs.** Per-item retry is a caller
//!   decision; the trait surfaces what the vendor reported.
//!
//! Per `docs/architecture/01-llm-provider.md §17` (the agent-runtime
//! scope discipline), these all belong to the caller's app layer, not
//! the runtime.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use tars_types::{
    BatchItemId, BatchJobId, BatchResultItem, BatchStatus, ChatRequest, ChatResponse,
    ProviderError, RequestContext,
};

/// Submit batches of [`ChatRequest`] for offline processing.
///
/// Vendor APIs (Anthropic `messages/batches`, OpenAI `batches`)
/// typically promise ~50% discount with up to a 24 h SLA. Implementations
/// are opt-in per provider — get a handle via
/// [`crate::LlmProvider::as_batch_submitter`].
#[async_trait]
pub trait BatchSubmitter: Send + Sync + 'static {
    /// Submit `items` and return the vendor's job ID. Items carry a
    /// caller-chosen [`BatchItemId`] which is echoed back in results
    /// so each output can be matched to its input regardless of order.
    ///
    /// `ctx` carries the per-request principal / tenant / trace the
    /// auth resolver needs to mint a real (e.g. Vault-issued, IAM-scoped)
    /// credential — production resolvers reject a test fixture, so it
    /// must be threaded from the caller rather than fabricated here.
    ///
    /// Errors:
    /// - [`ProviderError::Auth`] — bad credential
    /// - [`ProviderError::InvalidRequest`] — exceeded vendor batch size
    ///   limits, malformed item, etc.
    /// - [`ProviderError::Network`] — transport failure
    async fn submit(
        &self,
        items: Vec<(BatchItemId, ChatRequest)>,
        model: &str,
        ctx: &RequestContext,
    ) -> Result<BatchJobId, ProviderError>;

    /// Poll one job's current status. Idempotent / safe to call as
    /// often as the vendor's rate limit allows.
    async fn status(
        &self,
        id: &BatchJobId,
        ctx: &RequestContext,
    ) -> Result<BatchStatus, ProviderError>;

    /// Fetch the per-item results for a `Completed` job. Calling
    /// on a non-terminal job is a caller error — implementations
    /// SHOULD return `InvalidRequest` rather than blocking.
    async fn results(
        &self,
        id: &BatchJobId,
        ctx: &RequestContext,
    ) -> Result<Vec<BatchResultItem>, ProviderError>;

    /// Optional cancel — vendors that support it (Anthropic) move the
    /// job to `Cancelled`; vendors that don't (some OpenAI states)
    /// return `InvalidRequest`. Default: not supported.
    async fn cancel(&self, id: &BatchJobId, ctx: &RequestContext) -> Result<(), ProviderError> {
        let _ = id;
        let _ = ctx;
        Err(ProviderError::InvalidRequest(
            "cancel not supported by this provider's batch backend".into(),
        ))
    }
}

// ─── MockBatchSubmitter — for consumer tests ──────────────────────────

/// In-memory `BatchSubmitter` for testing batch-aware caller code.
///
/// Behavior:
/// - `submit()` stores items and immediately transitions to
///   [`BatchStatus::Completed`] (no async simulation by default).
/// - `status()` returns whatever the test most-recently set; defaults
///   to `Completed` post-submit.
/// - `results()` echoes back the input as text-only responses (one
///   item per submitted request). Tests that need richer mock results
///   call [`MockBatchSubmitter::set_results`].
///
/// For tests that need to simulate progress, use
/// [`MockBatchSubmitter::set_status`] to manually drive transitions.
pub struct MockBatchSubmitter {
    state: Mutex<MockState>,
}

struct MockState {
    next_job_seq: u64,
    /// Each submitted job keeps its current status and (later) results.
    jobs: std::collections::HashMap<BatchJobId, MockJob>,
}

struct MockJob {
    /// Caller-supplied items, indexed by their `BatchItemId`.
    items: Vec<(BatchItemId, ChatRequest)>,
    /// Concrete model the batch was submitted against (echoed in the
    /// synthesized results — the request itself is model-agnostic).
    model: String,
    status: BatchStatus,
    /// Override results set by `set_results`. If `None`, `results()`
    /// synthesizes text-only responses from input items.
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

    /// Override the status that the next `status()` call will report
    /// for `id`. Useful for testing the caller's polling loop.
    pub async fn set_status(&self, id: &BatchJobId, status: BatchStatus) {
        if let Some(job) = self.state.lock().await.jobs.get_mut(id) {
            job.status = status;
        }
    }

    /// Override what the next `results()` call will return.
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
        _ctx: &RequestContext,
    ) -> Result<BatchJobId, ProviderError> {
        let mut state = self.state.lock().await;
        let seq = state.next_job_seq;
        state.next_job_seq += 1;
        let job_id = BatchJobId::new(format!("mock-batch-{seq:04}"));
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

    async fn status(
        &self,
        id: &BatchJobId,
        _ctx: &RequestContext,
    ) -> Result<BatchStatus, ProviderError> {
        match self.state.lock().await.jobs.get(id) {
            Some(job) => Ok(job.status.clone()),
            None => Err(ProviderError::InvalidRequest(format!(
                "mock: unknown batch job id: {id}"
            ))),
        }
    }

    async fn results(
        &self,
        id: &BatchJobId,
        _ctx: &RequestContext,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        let state = self.state.lock().await;
        let job = state.jobs.get(id).ok_or_else(|| {
            ProviderError::InvalidRequest(format!("mock: unknown batch job id: {id}"))
        })?;
        if !job.status.is_terminal() {
            return Err(ProviderError::InvalidRequest(format!(
                "mock: results() called on non-terminal job (status: {:?})",
                job.status
            )));
        }
        if let Some(custom) = &job.custom_results {
            // Clone results — BatchResultItem holds a Result<ChatResponse, ...>
            // which isn't Clone, so we have to fabricate per-item clones via re-marshal.
            // Tests calling set_results get their results back unchanged here is the contract,
            // but since Result<ChatResponse, _> isn't Clone we serialize/deserialize.
            return custom
                .iter()
                .map(|item| {
                    let result = match &item.result {
                        Ok(resp) => Ok(clone_via_serde(resp)?),
                        Err(e) => Err(clone_provider_error(e)),
                    };
                    Ok(BatchResultItem {
                        item_id: item.item_id.clone(),
                        result,
                    })
                })
                .collect();
        }
        // Default: echo each input as a text response.
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
    // Echo "ok" — keeps the response stream-shape valid for downstream
    // consumers that aggregate.
    acc.apply(ChatEvent::Delta { text: "ok".into() });
    acc.apply(ChatEvent::Finished {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    });
    acc.finish()
}

fn clone_via_serde(resp: &ChatResponse) -> Result<ChatResponse, ProviderError> {
    // Don't `expect` here: this runs inside `results()` while the state
    // lock is held, so a panic would unwind through the guard. Surface a
    // typed error instead and let the caller decide.
    let v = serde_json::to_value(resp).map_err(|e| {
        ProviderError::Internal(format!("mock results: ChatResponse serialize: {e}"))
    })?;
    serde_json::from_value(v)
        .map_err(|e| ProviderError::Internal(format!("mock results: ChatResponse round-trip: {e}")))
}

fn clone_provider_error(e: &ProviderError) -> ProviderError {
    // ProviderError isn't Clone, but for mock-test purposes we
    // reconstruct via Display. Lossy on inner detail but adequate.
    ProviderError::Internal(format!("mock-clone of {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{ChatRequest, ModelHint, RequestContext};

    fn req(text: &str) -> ChatRequest {
        ChatRequest::user(text)
    }

    fn ctx() -> RequestContext {
        RequestContext::test_default()
    }

    #[tokio::test]
    async fn submit_assigns_unique_job_ids() {
        let m = MockBatchSubmitter::new();
        let id1 = m
            .submit(vec![(BatchItemId::new("a"), req("hello"))], "test-model", &ctx())
            .await
            .unwrap();
        let id2 = m
            .submit(vec![(BatchItemId::new("b"), req("world"))], "test-model", &ctx())
            .await
            .unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn status_after_submit_is_completed_by_default() {
        let m = MockBatchSubmitter::new();
        let id = m
            .submit(vec![(BatchItemId::new("a"), req("x"))], "test-model", &ctx())
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
            .submit(vec![(BatchItemId::new("a"), req("x"))], "test-model", &ctx())
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
            .submit(vec![(BatchItemId::new("a"), req("x"))], "test-model", &ctx())
            .await
            .unwrap();
        // Simulate progress polling.
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
            .submit(vec![(BatchItemId::new("a"), req("x"))], "test-model", &ctx())
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
        // Default LlmProvider impl returns None — backends that don't
        // override stay sync-only. (Phase 2/3 will add real overrides.)
        use crate::backends::mock::{CannedResponse, MockProvider};
        use crate::provider::LlmProvider;

        let p: Arc<dyn LlmProvider> = MockProvider::new("p", CannedResponse::text("hi"));
        assert!(p.as_batch_submitter().is_none());
    }
}
