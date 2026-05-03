//! In-memory mock provider for testing.
//!
//! Mirrors the Python `MockClient` (in `arc/app/llm/mock_client.py`) —
//! records the last request, returns a canned response. Adds streaming
//! semantics: the canned [`ChatEvent`] sequence is replayed verbatim,
//! so tests can exercise the streaming path.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;

use tars_types::{
    Capabilities, ChatRequest, ChatEvent, Pricing, ProviderError, ProviderId,
    RequestContext, StopReason, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// A canned response for the mock to replay.
#[derive(Clone, Debug)]
pub enum CannedResponse {
    /// Simple text completion — emits Started + one Delta + Finished.
    Text(String),
    /// Caller-supplied event sequence, replayed verbatim. Useful for
    /// tool-use and structured-output tests.
    Sequence(Vec<ChatEvent>),
    /// Provider error — emitted as the `stream()` failure (not mid-stream).
    Error(String),
}

impl CannedResponse {
    /// Convenience — single text response.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

/// Records calls to the mock so tests can assert on them.
#[derive(Debug, Default)]
pub struct MockHistory {
    pub requests: Vec<ChatRequest>,
}

/// Mutable state held under a single mutex so concurrent stream() calls
/// see a consistent snapshot of (history-after-this-request,
/// response-at-this-instant). Audit `tars-provider-src-backends-mock-5`:
/// previously held two separate locks → another thread could swap the
/// canned response in between the history append and the response read,
/// producing test flakiness.
#[derive(Debug)]
struct MockState {
    response: CannedResponse,
    history: MockHistory,
}

pub struct MockProvider {
    id: ProviderId,
    capabilities: Capabilities,
    state: Mutex<MockState>,
}

impl MockProvider {
    pub fn new(id: impl Into<ProviderId>, response: CannedResponse) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            capabilities: Capabilities::text_only_baseline(Pricing::default()),
            state: Mutex::new(MockState {
                response,
                history: MockHistory::default(),
            }),
        })
    }

    /// Replace the canned response — useful for multi-step tests that
    /// want to vary behavior between invocations.
    pub fn set_response(&self, r: CannedResponse) {
        self.state.lock().unwrap().response = r;
    }

    /// Snapshot of the requests recorded so far.
    pub fn history_snapshot(&self) -> Vec<ChatRequest> {
        self.state.lock().unwrap().history.requests.clone()
    }

    pub fn call_count(&self) -> usize {
        self.state.lock().unwrap().history.requests.len()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Atomic: append-history + read-canned-response under one lock
        // so concurrent callers can't observe a swap mid-operation.
        //
        // Audit `tars-provider-src-backends-mock-8`: the previous
        // `lock().unwrap()` would panic the MockProvider permanently
        // if any prior task panicked while holding the mutex (poison).
        // For a test-only mock the blast radius is limited, but the
        // method signature is `Result<_, ProviderError>` — using `?`
        // semantics here mirrors what the real backends do.
        let response = {
            let mut state = self
                .state
                .lock()
                .map_err(|e| ProviderError::Internal(format!("mock state poisoned: {e}")))?;
            state.history.requests.push(req.clone());
            state.response.clone()
        };
        match response {
            CannedResponse::Error(msg) => Err(ProviderError::Internal(msg)),
            CannedResponse::Text(text) => {
                let model = req.model.label();
                let events: Vec<Result<ChatEvent, ProviderError>> = vec![
                    Ok(ChatEvent::started(model)),
                    Ok(ChatEvent::Delta { text: text.clone() }),
                    Ok(ChatEvent::Finished {
                        stop_reason: StopReason::EndTurn,
                        usage: Usage {
                            input_tokens: 0,
                            output_tokens: text.len() as u64 / 4,
                            ..Default::default()
                        },
                    }),
                ];
                Ok(Box::pin(stream::iter(events)))
            }
            CannedResponse::Sequence(events) => {
                let mapped: Vec<Result<ChatEvent, ProviderError>> =
                    events.into_iter().map(Ok).collect();
                Ok(Box::pin(stream::iter(mapped)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::ModelHint;

    #[tokio::test]
    async fn text_response_yields_three_events() {
        let p = MockProvider::new("mock", CannedResponse::text("hi"));
        let mut s = p
            .clone()
            .stream(
                ChatRequest::user(ModelHint::Explicit("mock-1".into()), "ping"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();

        use futures::StreamExt;
        let mut count = 0;
        let mut saw_finish = false;
        while let Some(ev) = s.next().await {
            let ev = ev.unwrap();
            count += 1;
            if matches!(ev, ChatEvent::Finished { .. }) {
                saw_finish = true;
            }
        }
        assert_eq!(count, 3);
        assert!(saw_finish);
    }

    #[tokio::test]
    async fn complete_aggregates_text() {
        let p = MockProvider::new("mock", CannedResponse::text("hello world"));
        let r = p
            .clone()
            .complete(
                ChatRequest::user(ModelHint::Explicit("mock-1".into()), "ping"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(r.text, "hello world");
        assert!(r.is_finished());
    }

    #[tokio::test]
    async fn records_call_count() {
        // Audit `tars-provider-src-backends-mock-23`: previously used
        // `let _ = ...` and only asserted the count, so this passed
        // even if every call had errored. Assert success per call.
        let p = MockProvider::new("mock", CannedResponse::text("hi"));
        for _ in 0..3 {
            let r = p
                .clone()
                .complete(
                    ChatRequest::user(ModelHint::Explicit("mock-1".into()), "ping"),
                    RequestContext::test_default(),
                )
                .await;
            assert!(r.is_ok(), "complete() unexpectedly errored");
        }
        assert_eq!(p.call_count(), 3);
    }

    #[tokio::test]
    async fn error_response_propagates() {
        let p = MockProvider::new("mock", CannedResponse::Error("boom".into()));
        let r = p
            .clone()
            .complete(
                ChatRequest::user(ModelHint::Explicit("mock-1".into()), "ping"),
                RequestContext::test_default(),
            )
            .await;
        assert!(r.is_err());
    }
}
