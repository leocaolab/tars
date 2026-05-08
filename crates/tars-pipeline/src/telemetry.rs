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
        // Telemetry: layer trace + share the accumulator handle so the
        // wrapped stream can finalise pipeline_total_ms when the stream
        // completes (not just when call() returns, which is when the
        // stream OPENS).
        let telemetry = ctx.telemetry.clone();
        if let Ok(mut t) = telemetry.lock() {
            t.layers.push("telemetry".into());
        }

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
                let observed = wrap_stream(stream, started, trace_id, tenant_id, model, telemetry);
                Ok(Box::pin(observed))
            }
        }
    }
}

/// Pass-through wrapper that fires `llm.call.finished` (or
/// `llm.call.stream_error`) when the underlying stream terminates.
/// Also writes `pipeline_total_ms` into the telemetry accumulator on
/// stream end (success or error). Yields all events untouched.
fn wrap_stream(
    inner: LlmEventStream,
    started: Instant,
    trace_id: tars_types::TraceId,
    tenant_id: tars_types::TenantId,
    model: String,
    telemetry: tars_types::SharedTelemetry,
) -> impl futures::Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static {
    async_stream::stream! {
        let mut s = inner;
        while let Some(ev) = s.next().await {
            match &ev {
                Ok(ChatEvent::Finished { stop_reason, usage }) => {
                    let total_ms = started.elapsed().as_millis() as u64;
                    tracing::info!(
                        event = "llm.call.finished",
                        trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = total_ms,
                        stop_reason = ?stop_reason,
                        input_tokens = usage.input_tokens,
                        output_tokens = usage.output_tokens,
                        cached_input_tokens = usage.cached_input_tokens,
                        thinking_tokens = usage.thinking_tokens,
                    );
                    if let Ok(mut t) = telemetry.lock() {
                        t.pipeline_total_ms = Some(total_ms);
                    }
                }
                Err(e) => {
                    let total_ms = started.elapsed().as_millis() as u64;
                    tracing::warn!(
                        event = "llm.call.stream_error",
                        trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = total_ms,
                        error_class = ?e.class(),
                        error = %e,
                    );
                    if let Ok(mut t) = telemetry.lock() {
                        t.pipeline_total_ms = Some(total_ms);
                    }
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
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ModelHint, StopReason, Usage};
    use tracing::field::{Field, Visit};
    use tracing::Subscriber;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::Registry;

    use crate::service::LlmService;
    use crate::Pipeline;

    /// Captured fields for one tracing event.
    type EventFields = BTreeMap<String, String>;

    #[derive(Default, Clone)]
    struct CapturedEvents(Arc<Mutex<Vec<EventFields>>>);

    impl CapturedEvents {
        fn snapshot(&self) -> Vec<EventFields> {
            self.0.lock().unwrap().clone()
        }
        /// Returns the first event whose `event` field matches `name`.
        fn find(&self, name: &str) -> Option<EventFields> {
            self.snapshot()
                .into_iter()
                .find(|f| f.get("event").map(String::as_str) == Some(name))
        }
    }

    struct CaptureLayer {
        sink: CapturedEvents,
    }

    impl<S: Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldCollector::default();
            event.record(&mut visitor);
            self.sink.0.lock().unwrap().push(visitor.0);
        }
    }

    #[derive(Default)]
    struct FieldCollector(EventFields);

    impl Visit for FieldCollector {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_string(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    /// Install a capture subscriber for the duration of `f`. Uses
    /// `set_default` (thread-local) — works because `#[tokio::test]`
    /// defaults to the current-thread runtime, so all awaited work
    /// runs on the test thread.
    async fn with_capture<F, Fut, T>(f: F) -> (T, CapturedEvents)
    where
        F: FnOnce(CapturedEvents) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let captured = CapturedEvents::default();
        let subscriber = Registry::default().with(CaptureLayer { sink: captured.clone() });
        let _guard = tracing::subscriber::set_default(subscriber);
        let out = f(captured.clone()).await;
        (out, captured)
    }

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

    /// Telemetry's primary contract is *emitting* observability events.
    /// Behavioral pass-through tests above don't catch silent drops of
    /// the `tracing::info!` / `tracing::warn!` calls, so this test
    /// installs a capture subscriber and asserts the full happy-path
    /// lifecycle: start → opened → finished, with model + tenant +
    /// trace id stamped on each event and usage rolled up at finish.
    #[tokio::test]
    async fn telemetry_emits_lifecycle_events_on_happy_path() {
        let mock = MockProvider::new("p", CannedResponse::text("hello"));
        let pipeline = Pipeline::builder(mock)
            .layer(TelemetryMiddleware::new())
            .build();

        let ((), captured) = with_capture(|_| async {
            let mut s = Arc::new(pipeline)
                .call(
                    ChatRequest::user(ModelHint::Explicit("m-happy".into()), "x"),
                    RequestContext::test_default(),
                )
                .await
                .unwrap();
            while let Some(ev) = s.next().await {
                ev.unwrap();
            }
        })
        .await;

        let start = captured
            .find("llm.call.start")
            .expect("missing llm.call.start event");
        assert_eq!(start.get("model").map(String::as_str), Some("m-happy"));
        assert_eq!(start.get("messages").map(String::as_str), Some("1"));
        assert_eq!(start.get("tools").map(String::as_str), Some("0"));
        assert!(start.contains_key("trace_id"));
        assert!(start.contains_key("tenant_id"));

        let opened = captured
            .find("llm.call.opened")
            .expect("missing llm.call.opened event");
        assert_eq!(opened.get("model").map(String::as_str), Some("m-happy"));
        assert!(opened.contains_key("elapsed_ms"));

        let finished = captured
            .find("llm.call.finished")
            .expect("missing llm.call.finished event");
        assert_eq!(finished.get("model").map(String::as_str), Some("m-happy"));
        assert!(finished.contains_key("stop_reason"));
        assert!(finished.contains_key("input_tokens"));
        assert!(finished.contains_key("output_tokens"));
        assert!(finished.contains_key("elapsed_ms"));

        // No error events on the happy path.
        assert!(captured.find("llm.call.failed").is_none());
        assert!(captured.find("llm.call.stream_error").is_none());
    }

    #[tokio::test]
    async fn telemetry_emits_failed_event_on_open_error() {
        let mock = MockProvider::new("p", CannedResponse::Error("boom".into()));
        let pipeline = Pipeline::builder(mock)
            .layer(TelemetryMiddleware::new())
            .build();

        let (_err, captured) = with_capture(|_| async {
            Arc::new(pipeline)
                .call(
                    ChatRequest::user(ModelHint::Explicit("m-fail".into()), "x"),
                    RequestContext::test_default(),
                )
                .await
                .err()
                .expect("expected open-time error")
        })
        .await;

        let failed = captured
            .find("llm.call.failed")
            .expect("missing llm.call.failed event");
        assert_eq!(failed.get("model").map(String::as_str), Some("m-fail"));
        assert!(failed.contains_key("elapsed_ms"));
        assert!(failed.contains_key("error_class"));
        assert!(failed.contains_key("error"));

        // We never opened, so no opened/finished/stream_error.
        assert!(captured.find("llm.call.opened").is_none());
        assert!(captured.find("llm.call.finished").is_none());
        assert!(captured.find("llm.call.stream_error").is_none());
    }

    /// A test-only [`LlmService`] that yields a fixed sequence of
    /// `Result<ChatEvent, ProviderError>` items — including `Err`
    /// values, which `MockProvider`'s canned responses can't express.
    /// This lets us drive the *mid-stream* error path (the stream
    /// opens successfully, yields a few events, then errors).
    struct ScriptedService {
        events: Mutex<Option<Vec<Result<ChatEvent, ProviderError>>>>,
    }

    #[async_trait]
    impl LlmService for ScriptedService {
        async fn call(
            self: Arc<Self>,
            _req: ChatRequest,
            _ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            let events = self
                .events
                .lock()
                .unwrap()
                .take()
                .expect("ScriptedService called more than once");
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn telemetry_logs_and_propagates_mid_stream_error() {
        let scripted: Arc<dyn LlmService> = Arc::new(ScriptedService {
            events: Mutex::new(Some(vec![
                Ok(ChatEvent::started("m-mid")),
                Ok(ChatEvent::Delta { text: "partial".into() }),
                Err(ProviderError::Internal("midstream-boom".into())),
            ])),
        });
        let pipeline = Pipeline::builder_with_inner(scripted)
            .layer(TelemetryMiddleware::new())
            .build();

        let (collected, captured) = with_capture(|_| async {
            let mut s = Arc::new(pipeline)
                .call(
                    ChatRequest::user(ModelHint::Explicit("m-mid".into()), "x"),
                    RequestContext::test_default(),
                )
                .await
                .expect("stream should open successfully");
            let mut out = Vec::new();
            while let Some(ev) = s.next().await {
                out.push(ev);
            }
            out
        })
        .await;

        // Stream emitted three items: two successes, then one error —
        // telemetry must not swallow or reorder them.
        assert_eq!(collected.len(), 3);
        assert!(matches!(collected[0], Ok(ChatEvent::Started { .. })));
        assert!(matches!(collected[1], Ok(ChatEvent::Delta { .. })));
        match &collected[2] {
            Err(ProviderError::Internal(msg)) => assert_eq!(msg, "midstream-boom"),
            other => panic!("expected mid-stream Internal error, got {other:?}"),
        }

        // Stream opened cleanly → start + opened, *not* failed.
        assert!(captured.find("llm.call.start").is_some());
        assert!(captured.find("llm.call.opened").is_some());
        assert!(captured.find("llm.call.failed").is_none());

        // Mid-stream error must surface as `llm.call.stream_error`.
        let stream_err = captured
            .find("llm.call.stream_error")
            .expect("missing llm.call.stream_error event");
        assert_eq!(stream_err.get("model").map(String::as_str), Some("m-mid"));
        assert!(stream_err.contains_key("elapsed_ms"));
        assert!(stream_err.contains_key("error_class"));
        let err_str = stream_err.get("error").expect("error field");
        assert!(
            err_str.contains("midstream-boom"),
            "error field should include cause; got {err_str:?}"
        );

        // Stream errored before Finished → no `llm.call.finished` event.
        assert!(captured.find("llm.call.finished").is_none());
    }

    /// Sanity: a stream that *does* finish after deltas still produces
    /// the finished event with usage tokens correctly stamped — guards
    /// against regressions where the visitor or the field set drifts.
    #[tokio::test]
    async fn telemetry_finished_event_carries_usage_tokens() {
        let scripted: Arc<dyn LlmService> = Arc::new(ScriptedService {
            events: Mutex::new(Some(vec![
                Ok(ChatEvent::started("m-usage")),
                Ok(ChatEvent::Delta { text: "ok".into() }),
                Ok(ChatEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 7,
                        output_tokens: 11,
                        ..Default::default()
                    },
                }),
            ])),
        });
        let pipeline = Pipeline::builder_with_inner(scripted)
            .layer(TelemetryMiddleware::new())
            .build();

        let ((), captured) = with_capture(|_| async {
            let mut s = Arc::new(pipeline)
                .call(
                    ChatRequest::user(ModelHint::Explicit("m-usage".into()), "x"),
                    RequestContext::test_default(),
                )
                .await
                .unwrap();
            while let Some(ev) = s.next().await {
                ev.unwrap();
            }
        })
        .await;

        let finished = captured
            .find("llm.call.finished")
            .expect("missing llm.call.finished event");
        assert_eq!(finished.get("input_tokens").map(String::as_str), Some("7"));
        assert_eq!(finished.get("output_tokens").map(String::as_str), Some("11"));
    }
}
