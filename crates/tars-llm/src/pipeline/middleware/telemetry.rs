//! Opens one span per call, named and shaped as OpenTelemetry GenAI's client inference span, and
//! emits `tracing` events inside it when the call starts, opens, fails or is refused, and when its
//! stream finishes, errors or ends without either; hands out `pipeline_total_ms` when the stream
//! ends.
//! It only speaks `tracing`: whether a span leaves the process, and how it is sampled, belongs to
//! the subscriber the program installed.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tracing::Instrument;
use tracing::field::Empty;

use crate::LlmEventStream;
use crate::ProviderError;
use tars_types::{CallTelemetry, Caller, ChatEvent, ChatRequest, LlmErrorKind, Usage};
use tars_utils::display_chain;

use crate::pipeline::middleware::Middleware;
use crate::pipeline::service::{Next, OpenFailure, RefusedCall, max_output_tokens_sent};

#[derive(Clone, Debug, Default)]
pub struct TelemetryMiddleware;

impl TelemetryMiddleware {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Middleware for TelemetryMiddleware {
    fn name(&self) -> &'static str {
        "telemetry"
    }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: Caller,
        next: Next<'_>,
    ) -> Result<LlmEventStream, OpenFailure> {
        let model = next.model().to_string();
        let messages = req.messages.len();
        let tools = req.tools.len();
        let trace_id = ctx.trace_id.clone();
        let tenant_id = ctx.tenant_id.clone();
        let span = call_span(
            &trace_id,
            &model,
            next.gen_ai_provider_name(),
            max_output_tokens_sent(req.max_output_tokens, next.output_limit()),
        );
        // `pipeline_total_ms` is measured when the stream ends, not when `next.run` returns,
        // which is only when the stream opens.
        let started = Instant::now();

        span.in_scope(|| {
            tracing::info!(
                event = "llm.call.start",
                tars.trace_id = %trace_id,
                tenant_id = %tenant_id,
                model = %model,
                messages,
                tools,
            )
        });

        let result = next.run(req, ctx).instrument(span.clone()).await;
        let opened_at = started.elapsed();

        match result {
            Err(failure) => {
                let e = &failure.error;
                record_error(&span, e.kind());
                span.in_scope(|| {
                    tracing::warn!(
                        event = "llm.call.failed",
                        tars.trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = opened_at.as_millis() as u64,
                        error_class = ?e.class(),
                        error = %display_chain(e),
                    )
                });
                Err(failure)
            }
            Ok(stream) => {
                span.in_scope(|| {
                    tracing::debug!(
                        event = "llm.call.opened",
                        tars.trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = opened_at.as_millis() as u64,
                    )
                });
                let observed =
                    wrap_stream(stream, started, trace_id, tenant_id, model, span.clone());
                Ok(Box::pin(InSpan {
                    inner: Box::pin(observed),
                    span,
                }))
            }
        }
    }

    fn on_refusal(&self, refusal: &RefusedCall<'_>) {
        let ctx = refusal.ctx();
        let error = refusal.error();
        tracing::warn!(
            event = "llm.call.refused",
            tars.trace_id = %ctx.trace_id,
            tenant_id = %ctx.tenant_id,
            model = %refusal.model(),
            error_class = ?error.class(),
            error = %display_chain(&error),
        );
    }
}

/// Returns the call's span. The attributes a sampler may read are set here, as the convention
/// asks; the usage and the failure are declared empty and recorded when the stream ends. A
/// provider tars cannot name is left out rather than guessed.
///
/// `tars.trace_id` is the caller's own correlation id (a session id, a job name), not the
/// OpenTelemetry trace id, which comes from the span context.
fn call_span(
    trace_id: &tars_types::TraceId,
    model: &str,
    provider: Option<&str>,
    max_tokens: Option<u32>,
) -> tracing::Span {
    let name = format!("chat {model}");
    tracing::info_span!(
        "llm.call",
        otel.name = name.as_str(),
        otel.kind = "client",
        otel.status_code = Empty,
        tars.trace_id = %trace_id,
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = provider,
        gen_ai.request.model = model,
        gen_ai.request.stream = true,
        gen_ai.request.max_tokens = max_tokens,
        gen_ai.response.finish_reasons = Empty,
        gen_ai.usage.input_tokens = Empty,
        gen_ai.usage.output_tokens = Empty,
        gen_ai.usage.cache_read.input_tokens = Empty,
        gen_ai.usage.cache_write.input_tokens = Empty,
        gen_ai.usage.reasoning.output_tokens = Empty,
        error.type = Empty,
    )
}

/// One call's token counts in OpenTelemetry GenAI's terms.
#[derive(Debug, PartialEq, Eq)]
struct GenAiUsage {
    /// The whole prompt, cached parts included.
    input: u64,
    /// The whole output, reasoning included.
    output: u64,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
}

/// Returns `usage` in the convention's terms. Its input already holds the cached and
/// cache-creation tokens, while thinking the provider reports apart is not in its output, so it
/// is added there. A breakdown that is zero is absent, as the convention asks.
fn gen_ai_usage(usage: &Usage) -> GenAiUsage {
    let nonzero = |n: u64| (n != 0).then_some(n);
    GenAiUsage {
        input: usage.input_tokens,
        output: usage.output_tokens.saturating_add(usage.thinking_tokens),
        cache_read: nonzero(usage.cached_input_tokens),
        cache_write: nonzero(usage.cache_creation_tokens),
        reasoning: nonzero(usage.thinking_tokens),
    }
}

fn record_usage(span: &tracing::Span, usage: &Usage) {
    let u = gen_ai_usage(usage);
    span.record("gen_ai.usage.input_tokens", u.input);
    span.record("gen_ai.usage.output_tokens", u.output);
    span.record("gen_ai.usage.cache_read.input_tokens", u.cache_read);
    span.record("gen_ai.usage.cache_write.input_tokens", u.cache_write);
    span.record("gen_ai.usage.reasoning.output_tokens", u.reasoning);
}

fn record_error(span: &tracing::Span, kind: LlmErrorKind) {
    span.record("error.type", kind.as_str());
    span.record("otel.status_code", "error");
}

/// Polls the call's stream inside its span, so the events it emits belong to the call and the
/// span closes when the stream is done or dropped.
struct InSpan {
    inner: Pin<Box<dyn Stream<Item = Result<ChatEvent, ProviderError>> + Send>>,
    span: tracing::Span,
}

impl Stream for InSpan {
    type Item = Result<ChatEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let _entered = this.span.enter();
        this.inner.as_mut().poll_next(cx)
    }
}

/// Passes events through untouched, logging the stream's end and recording it on the call's span;
/// `pipeline_total_ms` goes out as a [`ChatEvent::Measured`] just before the stream's first
/// terminal item, or last when it has none.
fn wrap_stream(
    inner: LlmEventStream,
    started: Instant,
    trace_id: tars_types::TraceId,
    tenant_id: tars_types::TenantId,
    model: String,
    span: tracing::Span,
) -> impl futures::Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static {
    async_stream::stream! {
        let mut s = inner;
        let mut saw_terminal = false;
        let mut measured = false;
        let mut spent: Option<Usage> = None;
        while let Some(ev) = s.next().await {
            if let Ok(event) = &ev {
                spent = event.spent_after(spent);
            }
            match &ev {
                Ok(ChatEvent::Finished { stop_reason, usage }) => {
                    saw_terminal = true;
                    if let Some(usage) = usage {
                        record_usage(&span, usage);
                    }
                    span.record("gen_ai.response.finish_reasons", stop_reason.as_str());
                    let total_ms = started.elapsed().as_millis() as u64;
                    tracing::info!(
                        event = "llm.call.finished",
                        tars.trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = total_ms,
                        stop_reason = ?stop_reason,
                        input_tokens = usage.map(|u| u.input_tokens),
                        output_tokens = usage.map(|u| u.output_tokens),
                        cached_input_tokens = usage.map(|u| u.cached_input_tokens),
                        thinking_tokens = usage.map(|u| u.thinking_tokens),
                    );
                    if !measured {
                        measured = true;
                        yield Ok(pipeline_time(total_ms));
                    }
                }
                Err(e) => {
                    saw_terminal = true;
                    record_error(&span, e.kind());
                    if let Some(usage) = &spent {
                        record_usage(&span, usage);
                    }
                    let total_ms = started.elapsed().as_millis() as u64;
                    tracing::warn!(
                        event = "llm.call.stream_error",
                        tars.trace_id = %trace_id,
                        tenant_id = %tenant_id,
                        model = %model,
                        elapsed_ms = total_ms,
                        error_class = ?e.class(),
                        error = %display_chain(&e),
                        input_tokens = spent.map(|u| u.input_tokens),
                        output_tokens = spent.map(|u| u.output_tokens),
                        cached_input_tokens = spent.map(|u| u.cached_input_tokens),
                        thinking_tokens = spent.map(|u| u.thinking_tokens),
                    );
                    if !measured {
                        measured = true;
                        yield Ok(pipeline_time(total_ms));
                    }
                }
                _ => {}
            }
            yield ev;
        }

        // A provider can close the stream after deltas with no `Finished` or error; that end is
        // logged as its own event so it is not missing from the timeline.
        if !saw_terminal {
            let total_ms = started.elapsed().as_millis() as u64;
            tracing::info!(
                event = "llm.call.stream_ended_without_terminal",
                tars.trace_id = %trace_id,
                tenant_id = %tenant_id,
                model = %model,
                elapsed_ms = total_ms,
                "stream closed without a Finished or error event",
            );
            yield Ok(pipeline_time(total_ms));
        }
    }
}

/// Returns the event that hands out the pipeline's wall time.
fn pipeline_time(total_ms: u64) -> ChatEvent {
    ChatEvent::Measured {
        telemetry: CallTelemetry {
            pipeline_total_ms: Some(total_ms),
            ..CallTelemetry::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmError;
    use crate::LlmProvider;
    use crate::LlmService;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tars_types::{Pricing, ProviderId, ProviderProfile, StopReason, Usage};
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    type EventFields = BTreeMap<String, String>;

    /// Holds each event's fields, with the name of the span it was emitted in under `_span`, and
    /// each span's fields as last recorded, with its name under `_name`.
    #[derive(Default, Clone)]
    struct CapturedEvents {
        events: Arc<Mutex<Vec<EventFields>>>,
        spans: Arc<Mutex<BTreeMap<u64, EventFields>>>,
    }

    impl CapturedEvents {
        fn snapshot(&self) -> Vec<EventFields> {
            self.events
                .lock()
                .expect("captured events lock poisoned")
                .clone()
        }

        fn spans(&self) -> Vec<EventFields> {
            self.spans
                .lock()
                .expect("captured spans lock poisoned")
                .values()
                .cloned()
                .collect()
        }

        fn only_call_span(&self) -> EventFields {
            let spans = self.spans();
            assert_eq!(spans.len(), 1, "expected one span, got {spans:?}");
            let span = spans.into_iter().next().expect("length checked above");
            assert_eq!(span.get("_name").map(String::as_str), Some("llm.call"));
            span
        }

        fn find(&self, name: &str) -> Option<EventFields> {
            self.snapshot()
                .into_iter()
                .find(|f| f.get("event").map(String::as_str) == Some(name))
        }
    }

    struct CaptureLayer {
        sink: CapturedEvents,
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CaptureLayer {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: Context<'_, S>,
        ) {
            let mut visitor = FieldCollector::default();
            attrs.record(&mut visitor);
            visitor
                .0
                .insert("_name".into(), attrs.metadata().name().into());
            if let Some(parent) = ctx.span(id).and_then(|span| span.parent()) {
                visitor.0.insert("_parent".into(), parent.name().into());
            }
            self.sink
                .spans
                .lock()
                .expect("captured spans lock poisoned")
                .insert(id.into_u64(), visitor.0);
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: Context<'_, S>,
        ) {
            let mut spans = self
                .sink
                .spans
                .lock()
                .expect("captured spans lock poisoned");
            let fields = spans.entry(id.into_u64()).or_default();
            let mut visitor = FieldCollector(std::mem::take(fields));
            values.record(&mut visitor);
            *fields = visitor.0;
        }

        fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
            let mut visitor = FieldCollector::default();
            event.record(&mut visitor);
            if let Some(span) = ctx.event_span(event) {
                visitor.0.insert("_span".into(), span.name().into());
            }
            self.sink
                .events
                .lock()
                .expect("captured events lock poisoned")
                .push(visitor.0);
        }
    }

    #[derive(Default)]
    struct FieldCollector(EventFields);

    impl Visit for FieldCollector {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
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

    /// Installs a capture subscriber while `f` runs. `set_default` is thread-local, which is
    /// enough because `#[tokio::test]` runs everything on the test thread.
    async fn with_capture<F, Fut, T>(f: F) -> (T, CapturedEvents)
    where
        F: FnOnce(CapturedEvents) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let captured = CapturedEvents::default();
        let subscriber = Registry::default().with(CaptureLayer {
            sink: captured.clone(),
        });
        let _guard = tracing::subscriber::set_default(subscriber);
        let out = f(captured.clone()).await;
        (out, captured)
    }

    #[tokio::test]
    async fn telemetry_passes_events_through_and_hands_out_the_pipeline_time_before_finished() {
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        let pipeline = LlmService::builder(mock, "test-model")
            .layer(TelemetryMiddleware::new())
            .build();
        let mut s = Arc::new(pipeline)
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result
            .unwrap();
        let mut events = Vec::new();
        while let Some(ev) = s.next().await {
            events.push(ev.unwrap());
        }
        // The provider's own events in order, the provider time, then this layer's pipeline
        // time just before `Finished`.
        assert!(
            matches!(
                events.as_slice(),
                [
                    ChatEvent::Started { .. },
                    ChatEvent::Delta { .. },
                    ChatEvent::Measured { telemetry: provider },
                    ChatEvent::Measured { telemetry: pipeline },
                    ChatEvent::Finished { .. },
                ] if provider.provider_latency_ms.is_some()
                    && provider.pipeline_total_ms.is_none()
                    && pipeline.pipeline_total_ms.is_some()
                    && pipeline.provider_latency_ms.is_none()
            ),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn telemetry_propagates_open_time_errors() {
        // Mock returns an Error from stream() — telemetry should
        // surface it and not swallow.
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::Error("boom".into()));
        let pipeline = LlmService::builder(mock, "test-model")
            .layer(TelemetryMiddleware::new())
            .build();
        let err = match Arc::new(pipeline)
            .call(ChatRequest::user("x"), Caller::test_default())
            .await
            .result
        {
            Ok(_) => panic!("expected open-time error"),
            Err(e) => e,
        };
        assert!(matches!(
            err,
            LlmError::Provider(ProviderError::Internal(_))
        ));
    }

    /// The pass-through tests do not see a dropped `tracing` call, so this one captures the
    /// events of a successful call and checks each carries model, tenant and trace id.
    #[tokio::test]
    async fn telemetry_emits_lifecycle_events_on_happy_path() {
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hello"));
        let pipeline = LlmService::builder(mock, "m-happy")
            .layer(TelemetryMiddleware::new())
            .build();

        let ((), captured) = with_capture(|_| async {
            let mut s = Arc::new(pipeline)
                .call(ChatRequest::user("x"), Caller::test_default())
                .await
                .result
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
        assert!(start.contains_key("tars.trace_id"));
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

        assert!(captured.find("llm.call.failed").is_none());
        assert!(captured.find("llm.call.stream_error").is_none());
    }

    #[tokio::test]
    async fn telemetry_emits_failed_event_on_open_error() {
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::Error("boom".into()));
        let pipeline = LlmService::builder(mock, "m-fail")
            .layer(TelemetryMiddleware::new())
            .build();

        let (_err, captured) = with_capture(|_| async {
            Arc::new(pipeline)
                .call(ChatRequest::user("x"), Caller::test_default())
                .await
                .result
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

    /// Yields a fixed sequence of items, `Err` included, which `MockProvider` cannot express; it
    /// drives the mid-stream error path.
    struct ScriptedProvider {
        id: ProviderId,
        caps: ProviderProfile,
        events: Mutex<Option<Vec<Result<ChatEvent, ProviderError>>>>,
    }

    impl ScriptedProvider {
        fn new(events: Vec<Result<ChatEvent, ProviderError>>) -> Arc<Self> {
            Self::serving_as(None, events)
        }

        fn serving_as(
            gen_ai_provider_name: Option<&str>,
            events: Vec<Result<ChatEvent, ProviderError>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                id: ProviderId::new("scripted"),
                caps: ProviderProfile {
                    gen_ai_provider_name: gen_ai_provider_name.map(str::to_owned),
                    ..ProviderProfile::text_only_baseline(Pricing::default())
                },
                events: Mutex::new(Some(events)),
            })
        }
    }

    async fn drain(
        pipeline: LlmService,
        req: ChatRequest,
    ) -> Vec<Result<ChatEvent, ProviderError>> {
        let mut s = Arc::new(pipeline)
            .call(req, Caller::test_default())
            .await
            .result
            .expect("stream should open");
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    fn field<'a>(fields: &'a EventFields, name: &str) -> Option<&'a str> {
        fields.get(name).map(String::as_str)
    }

    #[test]
    fn the_usage_totals_hold_the_cached_input_and_the_reasoning_output() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 40,
            cache_creation_tokens: 10,
            thinking_tokens: 5,
        };
        assert_eq!(
            gen_ai_usage(&usage),
            GenAiUsage {
                input: 100,
                output: 25,
                cache_read: Some(40),
                cache_write: Some(10),
                reasoning: Some(5),
            }
        );
    }

    #[test]
    fn a_zero_breakdown_is_absent_and_the_totals_stay() {
        let usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
            ..Default::default()
        };
        assert_eq!(
            gen_ai_usage(&usage),
            GenAiUsage {
                input: 0,
                output: 0,
                cache_read: None,
                cache_write: None,
                reasoning: None,
            }
        );
    }

    #[test]
    fn the_output_total_saturates_instead_of_wrapping() {
        let usage = Usage {
            output_tokens: u64::MAX,
            thinking_tokens: 1,
            ..Default::default()
        };
        assert_eq!(gen_ai_usage(&usage).output, u64::MAX);
    }

    #[tokio::test]
    async fn a_call_is_one_gen_ai_client_span_holding_its_events_and_usage() {
        let scripted = ScriptedProvider::serving_as(
            Some("deepseek"),
            vec![
                Ok(ChatEvent::started("m-span")),
                Ok(ChatEvent::Delta { text: "ok".into() }),
                Ok(ChatEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                    usage: Some(Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                        cached_input_tokens: 40,
                        thinking_tokens: 5,
                        ..Default::default()
                    }),
                }),
            ],
        );
        let pipeline = LlmService::builder(scripted, "m-span")
            .layer(TelemetryMiddleware::new())
            .build();
        let req = ChatRequest {
            max_output_tokens: Some(256),
            ..ChatRequest::user("x")
        };

        let (_, captured) = with_capture(|_| drain(pipeline, req)).await;

        let span = captured.only_call_span();
        assert_eq!(field(&span, "otel.name"), Some("chat m-span"));
        assert_eq!(field(&span, "otel.kind"), Some("client"));
        assert_eq!(field(&span, "gen_ai.operation.name"), Some("chat"));
        assert_eq!(
            field(&span, "tars.trace_id"),
            Some(Caller::test_default().trace_id.to_string().as_str())
        );
        assert_eq!(field(&span, "gen_ai.provider.name"), Some("deepseek"));
        assert_eq!(field(&span, "gen_ai.request.model"), Some("m-span"));
        assert_eq!(field(&span, "gen_ai.request.stream"), Some("true"));
        assert_eq!(field(&span, "gen_ai.request.max_tokens"), Some("256"));
        assert_eq!(field(&span, "gen_ai.usage.input_tokens"), Some("100"));
        assert_eq!(field(&span, "gen_ai.usage.output_tokens"), Some("25"));
        assert_eq!(
            field(&span, "gen_ai.usage.cache_read.input_tokens"),
            Some("40")
        );
        assert_eq!(field(&span, "gen_ai.usage.cache_write.input_tokens"), None);
        assert_eq!(
            field(&span, "gen_ai.usage.reasoning.output_tokens"),
            Some("5")
        );
        assert_eq!(
            field(&span, "gen_ai.response.finish_reasons"),
            Some("end_turn")
        );
        assert_eq!(field(&span, "error.type"), None);
        assert_eq!(field(&span, "otel.status_code"), None);

        for name in ["llm.call.start", "llm.call.opened", "llm.call.finished"] {
            let ev = captured
                .find(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(field(&ev, "_span"), Some("llm.call"), "{name}");
        }
    }

    #[tokio::test]
    async fn the_call_span_hangs_under_the_span_the_caller_is_in() {
        use tracing::Instrument as _;
        let scripted = ScriptedProvider::new(vec![Ok(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::default()),
        })]);
        let pipeline = LlmService::builder(scripted, "m-child")
            .layer(TelemetryMiddleware::new())
            .build();

        let (_, captured) = with_capture(|_| {
            drain(pipeline, ChatRequest::user("x")).instrument(tracing::info_span!("caller"))
        })
        .await;

        let call = captured
            .spans()
            .into_iter()
            .find(|s| field(s, "_name") == Some("llm.call"))
            .expect("no call span");
        assert_eq!(field(&call, "_parent"), Some("caller"));
    }

    #[tokio::test]
    async fn a_provider_tars_cannot_name_is_left_off_the_span() {
        let scripted = ScriptedProvider::new(vec![Ok(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::default()),
        })]);
        let pipeline = LlmService::builder(scripted, "m-anon")
            .layer(TelemetryMiddleware::new())
            .build();

        let (_, captured) = with_capture(|_| drain(pipeline, ChatRequest::user("x"))).await;

        assert_eq!(
            field(&captured.only_call_span(), "gen_ai.provider.name"),
            None
        );
    }

    #[tokio::test]
    async fn a_mid_stream_failure_marks_the_span_as_an_error_of_its_kind() {
        let scripted = ScriptedProvider::new(vec![
            Ok(ChatEvent::started("m-err")),
            Err(ProviderError::Internal("midstream-boom".into())),
        ]);
        let pipeline = LlmService::builder(scripted, "m-err")
            .layer(TelemetryMiddleware::new())
            .build();

        let (_, captured) = with_capture(|_| drain(pipeline, ChatRequest::user("x"))).await;

        let span = captured.only_call_span();
        assert_eq!(field(&span, "error.type"), Some("internal"));
        assert_eq!(field(&span, "otel.status_code"), Some("error"));
        assert_eq!(field(&span, "gen_ai.usage.input_tokens"), None);
    }

    #[tokio::test]
    async fn a_stream_cut_after_usage_reports_records_their_highest_on_the_span() {
        let report = |input_tokens, output_tokens| {
            Ok(ChatEvent::UsageProgress {
                usage: Usage {
                    input_tokens,
                    output_tokens,
                    ..Default::default()
                },
            })
        };
        let scripted = ScriptedProvider::new(vec![
            Ok(ChatEvent::started("m-cut")),
            report(100, 0),
            Ok(ChatEvent::Delta { text: "ok".into() }),
            report(100, 20),
            Err(ProviderError::StreamCut { open_call: None }),
        ]);
        let pipeline = LlmService::builder(scripted, "m-cut")
            .layer(TelemetryMiddleware::new())
            .build();

        let (_, captured) = with_capture(|_| drain(pipeline, ChatRequest::user("x"))).await;

        let span = captured.only_call_span();
        assert_eq!(field(&span, "error.type"), Some("stream_cut"));
        assert_eq!(field(&span, "gen_ai.usage.input_tokens"), Some("100"));
        assert_eq!(field(&span, "gen_ai.usage.output_tokens"), Some("20"));
        let logged = captured
            .find("llm.call.stream_error")
            .expect("missing llm.call.stream_error");
        assert_eq!(field(&logged, "output_tokens"), Some("20"));
    }

    #[tokio::test]
    async fn a_failure_to_open_marks_the_span_as_an_error_of_its_kind() {
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::Error("boom".into()));
        let pipeline = LlmService::builder(mock, "m-open-fail")
            .layer(TelemetryMiddleware::new())
            .build();

        let (_, captured) = with_capture(|_| async {
            Arc::new(pipeline)
                .call(ChatRequest::user("x"), Caller::test_default())
                .await
                .result
                .err()
                .expect("expected open-time error")
        })
        .await;

        let span = captured.only_call_span();
        assert_eq!(field(&span, "error.type"), Some("internal"));
        assert_eq!(field(&span, "otel.status_code"), Some("error"));
        let failed = captured
            .find("llm.call.failed")
            .expect("missing llm.call.failed");
        assert_eq!(field(&failed, "_span"), Some("llm.call"));
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn capabilities(&self) -> &ProviderProfile {
            &self.caps
        }
        async fn stream(
            self: Arc<Self>,
            _req: ChatRequest,
            _model: &str,
            _ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            let events = self
                .events
                .lock()
                .expect("scripted provider events lock poisoned")
                .take()
                .expect("ScriptedProvider called more than once");
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn telemetry_logs_and_propagates_mid_stream_error() {
        let scripted = ScriptedProvider::new(vec![
            Ok(ChatEvent::started("m-mid")),
            Ok(ChatEvent::Delta {
                text: "partial".into(),
            }),
            Err(ProviderError::Internal("midstream-boom".into())),
        ]);
        let pipeline = LlmService::builder(scripted, "m-mid")
            .layer(TelemetryMiddleware::new())
            .build();

        let (collected, captured) = with_capture(|_| async {
            let mut s = Arc::new(pipeline)
                .call(ChatRequest::user("x"), Caller::test_default())
                .await
                .result
                .expect("stream should open successfully");
            let mut out = Vec::new();
            while let Some(ev) = s.next().await {
                out.push(ev);
            }
            out
        })
        .await;

        // The two successes and the error in order, telemetry neither swallowing nor reordering
        // them; the provider time and then the pipeline time go out just before the error.
        assert_eq!(collected.len(), 5, "{collected:?}");
        assert!(matches!(collected[0], Ok(ChatEvent::Started { .. })));
        assert!(matches!(collected[1], Ok(ChatEvent::Delta { .. })));
        assert!(
            matches!(&collected[2], Ok(ChatEvent::Measured { telemetry }) if telemetry.provider_latency_ms.is_some()),
            "{collected:?}"
        );
        assert!(
            matches!(&collected[3], Ok(ChatEvent::Measured { telemetry }) if telemetry.pipeline_total_ms.is_some()),
            "{collected:?}"
        );
        match &collected[4] {
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

    #[tokio::test]
    async fn telemetry_finished_event_carries_usage_tokens() {
        let scripted = ScriptedProvider::new(vec![
            Ok(ChatEvent::started("m-usage")),
            Ok(ChatEvent::Delta { text: "ok".into() }),
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage {
                    input_tokens: 7,
                    output_tokens: 11,
                    ..Default::default()
                }),
            }),
        ]);
        let pipeline = LlmService::builder(scripted, "test-model")
            .layer(TelemetryMiddleware::new())
            .build();

        let ((), captured) = with_capture(|_| async {
            let mut s = Arc::new(pipeline)
                .call(ChatRequest::user("x"), Caller::test_default())
                .await
                .result
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
        assert_eq!(
            finished.get("output_tokens").map(String::as_str),
            Some("11")
        );
    }

    #[tokio::test]
    async fn a_refused_call_is_logged_as_refused_and_a_served_one_is_not() {
        let provider = Arc::new(ScriptedProvider {
            id: ProviderId::new("scripted"),
            caps: ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http),
            events: Mutex::new(Some(vec![Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage::default()),
            })])),
        });
        let pipeline = Arc::new(
            LlmService::builder(provider, "m-refused")
                .layer(TelemetryMiddleware::new())
                .build(),
        );
        let mut thinking = ChatRequest::user("x");
        thinking.thinking = Some(tars_types::ThinkingMode::Auto);

        let ((), refused) = with_capture(|_| async {
            let refusal = pipeline.call(thinking, Caller::test_default()).await.result;
            assert!(matches!(refusal, Err(LlmError::Incompatible { .. })));
        })
        .await;
        let ((), served) = with_capture(|_| async {
            let mut s = pipeline
                .call(ChatRequest::user("x"), Caller::test_default())
                .await
                .result
                .unwrap();
            while let Some(ev) = s.next().await {
                ev.unwrap();
            }
        })
        .await;

        let line = refused
            .find("llm.call.refused")
            .expect("missing llm.call.refused event");
        assert_eq!(line.get("model").map(String::as_str), Some("m-refused"));
        assert!(
            line.get("error").is_some_and(|e| e.contains("think")),
            "{line:?}"
        );
        assert!(refused.find("llm.call.start").is_none());
        assert!(served.find("llm.call.start").is_some());
        assert!(served.find("llm.call.refused").is_none());
    }
}
