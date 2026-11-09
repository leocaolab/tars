//! Answers every request with a canned response and records each request, for tests.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;

use crate::ProviderError;
use tars_types::{
    Caller, ChatEvent, ChatRequest, Pricing, ProviderId, ProviderProfile, StopReason, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub enum CannedResponse {
    /// Streams `Started`, one `Delta` of the text, and `Finished`.
    Text(String),
    /// Streams these events as they are.
    Sequence(Vec<ChatEvent>),
    /// Fails `stream()` itself with `Internal`, before any event.
    Error(String),
}

impl CannedResponse {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

/// The mock's state, under one lock so a call's history append and its response read see the
/// same moment; with two locks another thread could swap the response in between.
#[derive(Debug)]
struct MockState {
    response: CannedResponse,
    /// The responses taken one per call, front first; once it is empty every call gets
    /// `response`.
    queue: VecDeque<CannedResponse>,
    requests: Vec<ChatRequest>,
}

pub struct MockProvider {
    id: ProviderId,
    capabilities: ProviderProfile,
    state: Mutex<MockState>,
}

/// Returns the text-only baseline marked `InterfaceKind::Mock`; the mock is not in
/// `data/provider.toml`.
fn mock_capabilities() -> ProviderProfile {
    let mut c = ProviderProfile::text_only_baseline(Pricing::default());
    c.interface = tars_types::InterfaceKind::Mock;
    c
}

impl MockProvider {
    pub fn new(id: ProviderId, response: CannedResponse) -> Arc<Self> {
        Arc::new(Self {
            id,
            capabilities: mock_capabilities(),
            state: Mutex::new(MockState {
                response,
                queue: VecDeque::new(),
                requests: Vec::new(),
            }),
        })
    }

    /// Returns a mock that answers call `n` with `responses[n]` and every call after the last
    /// with the last, so a tool loop runs without the test calling `set_response` between turns.
    /// Given no responses, every call fails with `Internal` rather than answer a made-up text.
    pub fn with_responses(id: ProviderId, responses: Vec<CannedResponse>) -> Arc<Self> {
        let fallback = responses.last().cloned().unwrap_or_else(|| {
            CannedResponse::Error("MockProvider::with_responses was given no responses".into())
        });
        Arc::new(Self {
            id,
            capabilities: mock_capabilities(),
            state: Mutex::new(MockState {
                response: fallback,
                queue: responses.into(),
                requests: Vec::new(),
            }),
        })
    }

    /// Sets the response of every call once the queued ones are used up. It ignores a poisoned
    /// lock, so an earlier panic is not hidden behind a second one.
    pub fn set_response(&self, r: CannedResponse) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .response = r;
    }

    /// Returns the requests received so far.
    pub fn history_snapshot(&self) -> Vec<ChatRequest> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requests
            .clone()
    }

    /// Returns how many `stream()` calls were received.
    pub fn call_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requests
            .len()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        _ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        // A poisoned lock fails this call, and every later one, with `Internal` instead of a
        // panic.
        let response = {
            let mut state = self
                .state
                .lock()
                .map_err(|e| ProviderError::Internal(format!("mock state poisoned: {e}")))?;
            state.requests.push(req.clone());
            state
                .queue
                .pop_front()
                .unwrap_or_else(|| state.response.clone())
        };
        match response {
            CannedResponse::Error(msg) => Err(ProviderError::Internal(msg)),
            CannedResponse::Text(text) => {
                let events: Vec<Result<ChatEvent, ProviderError>> = vec![
                    Ok(ChatEvent::started(model)),
                    Ok(ChatEvent::Delta { text: text.clone() }),
                    Ok(ChatEvent::Finished {
                        stop_reason: StopReason::EndTurn,
                        usage: Some(Usage {
                            input_tokens: 0,
                            output_tokens: tars_types::estimate_tokens(text.len()),
                            ..Default::default()
                        }),
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

    #[tokio::test]
    async fn a_mock_given_no_responses_fails_each_call_instead_of_answering_empty_text() {
        let p = MockProvider::with_responses(ProviderId::new("mock"), Vec::new());
        let result = p
            .stream(ChatRequest::user("ping"), "m", Caller::test_default())
            .await;
        assert!(matches!(result, Err(ProviderError::Internal(_))));
    }

    #[tokio::test]
    async fn text_response_yields_three_events() {
        let p = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hi"));
        let mut s = p
            .clone()
            .stream(
                ChatRequest::user("ping"),
                "test-model",
                Caller::test_default(),
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
        let p = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hello world"));
        let r = p
            .clone()
            .complete(
                ChatRequest::user("ping"),
                "test-model",
                Caller::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(r.text, "hello world");
    }

    #[tokio::test]
    async fn records_call_count() {
        let p = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hi"));
        for _ in 0..3 {
            let r = p
                .clone()
                .complete(
                    ChatRequest::user("ping"),
                    "test-model",
                    Caller::test_default(),
                )
                .await;
            assert!(r.is_ok(), "complete() unexpectedly errored");
        }
        assert_eq!(p.call_count(), 3);
    }

    #[tokio::test]
    async fn error_response_propagates() {
        let p = MockProvider::new(
            ProviderId::new("mock"),
            CannedResponse::Error("boom".into()),
        );
        let r = p
            .clone()
            .complete(
                ChatRequest::user("ping"),
                "test-model",
                Caller::test_default(),
            )
            .await;
        assert!(r.is_err());
    }
}
