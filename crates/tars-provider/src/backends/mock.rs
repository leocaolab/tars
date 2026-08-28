//! In-memory mock provider for testing.
//!
//! Records the last request, returns a canned response. The canned
//! [`ChatEvent`] sequence is replayed verbatim, so tests can exercise
//! the streaming path.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;

use tars_types::{
    ChatEvent, ChatRequest, Pricing, ProviderError, ProviderId, ProviderProfile, RequestContext,
    StopReason, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};
use std::collections::VecDeque;

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
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

#[derive(Debug, Default)]
pub struct MockHistory {
    pub requests: Vec<ChatRequest>,
}

/// Mutable state held under a single mutex so concurrent stream() calls
/// see a consistent snapshot of (history-after-this-request,
/// response-at-this-instant). Two separate locks would let another thread
/// swap the canned response between the history append and the response
/// read, producing test flakiness.
#[derive(Debug)]
struct MockState {
    response: CannedResponse,
    /// Per-call responses popped front-to-back (one per `stream()` call) —
    /// drives a multi-turn agent (a tool-call turn then a final turn). Empty →
    /// every call gets `response`.
    queue: VecDeque<CannedResponse>,
    history: MockHistory,
}

pub struct MockProvider {
    id: ProviderId,
    capabilities: ProviderProfile,
    state: Mutex<MockState>,
}

/// The mock's code-defined capabilities: a text-only baseline stamped with
/// `InterfaceKind::Mock` (no real call is ever placed). Mock/cassette keep
/// code-defined caps — they are NOT in `data/provider.toml`.
fn mock_capabilities() -> ProviderProfile {
    let mut c = ProviderProfile::text_only_baseline(Pricing::default());
    c.interface = tars_types::InterfaceKind::Mock;
    c
}

impl MockProvider {
    pub fn new(id: impl Into<ProviderId>, response: CannedResponse) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            capabilities: mock_capabilities(),
            state: Mutex::new(MockState {
                response,
                queue: VecDeque::new(),
                history: MockHistory::default(),
            }),
        })
    }

    /// A multi-turn mock: `responses` are returned one per `stream()` call,
    /// front-to-back; once exhausted, every further call repeats the last.
    /// Drives a tool-using agent loop (a tool-call turn → a final turn) without
    /// the test poking `set_response` between turns.
    pub fn with_responses(id: impl Into<ProviderId>, responses: Vec<CannedResponse>) -> Arc<Self> {
        let fallback = responses
            .last()
            .cloned()
            .unwrap_or_else(|| CannedResponse::text(""));
        Arc::new(Self {
            id: id.into(),
            capabilities: mock_capabilities(),
            state: Mutex::new(MockState {
                response: fallback,
                queue: responses.into(),
                history: MockHistory::default(),
            }),
        })
    }

    /// Recovers from a poisoned mutex (`into_inner`) rather than
    /// `unwrap()`-panicking: a prior panic while holding the lock must not
    /// cascade-panic this helper and mask the original test failure.
    pub fn set_response(&self, r: CannedResponse) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .response = r;
    }

    /// Snapshot of the requests recorded so far. Used by
    /// `examples/examples/testing/main.rs` (deterministic-agent-test demo);
    /// the production source tree itself doesn't call it.
    pub fn history_snapshot(&self) -> Vec<ChatRequest> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .requests
            .clone()
    }

    /// Number of `stream()` calls observed. Companion to
    /// [`Self::history_snapshot`] for the deterministic-agent-test
    /// example noted above.
    pub fn call_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
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
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Atomic: append-history + read-canned-response under one lock
        // so concurrent callers can't observe a swap mid-operation. A
        // poisoned mutex maps to a ProviderError rather than a panic, so a
        // prior task's panic doesn't take the MockProvider down permanently.
        let response = {
            let mut state = self
                .state
                .lock()
                .map_err(|e| ProviderError::Internal(format!("mock state poisoned: {e}")))?;
            state.history.requests.push(req.clone());
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

    #[tokio::test]
    async fn text_response_yields_three_events() {
        let p = MockProvider::new("mock", CannedResponse::text("hi"));
        let mut s = p
            .clone()
            .stream(
                ChatRequest::user("ping"),
                "test-model",
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
                ChatRequest::user("ping"),
                "test-model",
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(r.text, "hello world");
        assert!(r.is_finished());
    }

    #[tokio::test]
    async fn records_call_count() {
        // Assert success per call, not just the final count — a per-call
        // error must not pass silently.
        let p = MockProvider::new("mock", CannedResponse::text("hi"));
        for _ in 0..3 {
            let r = p
                .clone()
                .complete(
                    ChatRequest::user("ping"),
                    "test-model",
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
                ChatRequest::user("ping"),
                "test-model",
                RequestContext::test_default(),
            )
            .await;
        assert!(r.is_err());
    }
}
