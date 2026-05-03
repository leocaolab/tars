//! Telemetry middleware — basic tracing for M1.
//!
//! Doc 02 §4.1 calls for an OTel root span, child spans per layer, and
//! metric emission. M1 keeps it lean: structured `tracing` events at
//! call open / call complete / stream finish / stream error. The full
//! OTel exporter wiring lives in `tars-melt` (M5), which can subscribe
//! to the same `tracing` events without changing this layer.
//!
//! What we record per call (single inbound + a few outbound events):
//!
//! - `llm.call.start` — model, message count, tenant, trace id
//! - `llm.call.opened` — elapsed-to-first-byte in ms (open success)
//! - `llm.call.failed` — elapsed-to-error in ms (open failure)
//! - `llm.call.finished` — elapsed-to-finish, stop_reason, usage tokens
//! - `llm.call.stream_error` — mid-stream provider error

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;

use tars_provider::LlmEventStream;
use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext};

use crate::middleware::Middleware;
use crate::service::LlmService;

#[derive(Clone, Debug, Default)]
pub struct TelemetryMiddleware;

impl TelemetryMiddleware {
    pub fn new() -> Self {
        Self
    }
}

impl Middleware for TelemetryMiddleware {
    fn name(&self) -> &'static str {
        "telemetry"
    }
    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(TelemetryService { inner })
    }
}

struct TelemetryService {
    inner: Arc<dyn LlmService>,
}

#[async_trait]
impl LlmService for TelemetryService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Capture diagnostic fields BEFORE we move req into the inner
        // call — these are cheap clones / references on small data.
        let model = req.model.label().to_string();
        let messages = req.messages.len();
        let tools = req.tools.len();
        let trace_id = ctx.trace_id.clone();
        let tenant_id = ctx.tenant_id.clone();
        let started = Instant::now();

        tracing::info!(
            event = "llm.call.start",
            trace_id = %trace_id,
            tenant_id = %tenant_id,
            model = %model,
            messages,
            tools,
        );

        let result = self.inner.clone().call(req, ctx).await;
        let opened_at = started.elapsed();

        match result {
            Err(e) => {
                tracing::warn!(
                    event = "llm.call.failed",
                    trace_id = %trace_id,
                    tenant_id = %tenant_id,
                    model = %model,
                    elapsed_ms = opened_at.as_millis() as u64,
                    error_class = ?e.class(),
                    error = %e,
                );
                Err(e)
            }
            Ok(stream) => {
                tracing::debug!(
                    event = "llm.call.opened",
                    trace_id = %trace_id,
                    tenant_id = %tenant_id,
                    model = %model,
                    elapsed_ms = opened_at.as_millis() as u64,
                );
                let observed = wrap_stream(stream, started, trace_id, tenant_id, model);
                Ok(Box::pin(observed))
            }
        }
    }
}

/// Pass-through wrapper that fires `llm.call.finished` (or
/// `llm.call.stream_error`) when the underlying stream terminates.
/// Yields all events untouched.
fn wrap_stream(
    inner: LlmEventStream,
    started: Instant,
    trace_id: tars_types::TraceId,
    tenant_id: tars_types::TenantId,
    model: String,
) -> impl futures::Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static {
    async_stream::stream! {
        let mut s = inner;
        while let Some(ev) = s.next().await {
            match &ev {
                Ok(ChatEvent::Finished { stop_reason, usage }) => {
                    tracing::info!(
                        event = "llm.call.finished",
                        trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        stop_reason = ?stop_reason,
                        input_tokens = usage.input_tokens,
                        output_tokens = usage.output_tokens,
                        cached_input_tokens = usage.cached_input_tokens,
                        thinking_tokens = usage.thinking_tokens,
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        event = "llm.call.stream_error",
                        trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error_class = ?e.class(),
                        error = %e,
                    );
                }
                _ => {}
            }
            yield ev;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::ModelHint;

    use crate::Pipeline;

    #[tokio::test]
    async fn telemetry_passes_events_through_unchanged() {
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let pipeline = Pipeline::builder(mock)
            .layer(TelemetryMiddleware::new())
            .build();
        let mut s = Arc::new(pipeline)
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "x"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let mut got_finished = false;
        let mut total = 0;
        while let Some(ev) = s.next().await {
            total += 1;
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                got_finished = true;
            }
        }
        assert!(got_finished);
        assert_eq!(total, 3); // started + delta + finished
    }

    #[tokio::test]
    async fn telemetry_propagates_open_time_errors() {
        // Mock returns an Error from stream() — telemetry should
        // surface it and not swallow.
        let mock = MockProvider::new("p", CannedResponse::Error("boom".into()));
        let pipeline = Pipeline::builder(mock)
            .layer(TelemetryMiddleware::new())
            .build();
        let err = match Arc::new(pipeline)
            .call(
                ChatRequest::user(ModelHint::Explicit("m".into()), "x"),
                RequestContext::test_default(),
            )
            .await
        {
            Ok(_) => panic!("expected open-time error"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::Internal(_)));
    }
}
