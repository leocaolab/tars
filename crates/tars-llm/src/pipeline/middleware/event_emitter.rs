//! Writes one `llm_call` row per [`LlmService::call`](crate::LlmService::call), bodies inline,
//! into the call log's `LogSink`.
//!
//! It sits outside every layer that measures the call or validates its response, so what those
//! layers hand out passes through it: in the stream as `Measured` and `Validated` events, or with
//! the error when no stream opens.
//!
//! Exactly one row per call, whichever way it ends:
//! - the dispatch layer refuses the request before any layer runs → written when it tells this
//!   layer so, through [`Middleware::on_refusal`];
//! - the stream delivers its terminal event → written then, before it is
//!   yielded, so a caller that stops reading at `Finished` still gets its row;
//! - the call fails before a stream opens, or the stream ends without a
//!   terminal event → written when that is known;
//! - the caller drops the call (before the stream opens, or mid-stream) → a
//!   drop guard writes `SentEnd::Cancelled`.
//!
//! Writing is `try_append`: no await, no spawned task. A full buffer drops
//! the row, and the sink counts it (`health()`, `close()` → `Err`).
//!
//! The row's id is handed back to the caller in
//! [`ServiceCall::call_ids`](crate::ServiceCall::call_ids), recorded as soon as the id is
//! minted, so a dropped row's id is handed back too. A call this layer cannot log gets no id.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::StreamExt;
use sha2::{Digest, Sha256};

use crate::LlmError;
use crate::LlmEventStream;
use crate::ProviderError;
use tars_melt::event::call_log::{
    CallBody, CallDispatch, CallFailure, CallResponse, LlmCallRow, LlmCallTable, SentEnd, StreamEnd,
};
use tars_melt::event::log::LogSink;
use tars_types::{
    CallId, CallTelemetry, Caller, ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder,
    CompatibilityReason, Degradation, LlmErrorKind, LoggedCall, NonEmpty, OutputLimit, ProviderId,
    SessionId, TenantId, TraceId, ValidationReason, ValidationSummary,
};
use tars_utils::display_chain;

use crate::pipeline::middleware::Middleware;
use crate::pipeline::service::{Next, OpenFailure, RefusedCall};

/// Writes one `llm_call` row per call.
#[derive(Clone)]
pub struct EventEmitterMiddleware {
    calls: Arc<dyn LogSink<LlmCallTable>>,
    bodies: BodyRetention,
}

/// Whether a call's request and response bodies go into its row — the writer's
/// policy, not the table's: a consumer that must keep its users' code and the
/// model's answers off their disk withholds them, and keeps everything else.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BodyRetention {
    /// Inline, as [`CallBody::Present`].
    #[default]
    Keep,
    /// [`CallBody::Withheld`]; every other column, the request's hash included, as usual.
    Withhold,
}

impl BodyRetention {
    /// Returns `row` as this policy writes it.
    pub fn apply(self, row: LlmCallRow) -> LlmCallRow {
        match self {
            Self::Keep => row,
            Self::Withhold => row.withhold_bodies(),
        }
    }
}

impl EventEmitterMiddleware {
    /// Writes rows into `calls`, keeping bodies.
    pub fn new(calls: Arc<dyn LogSink<LlmCallTable>>) -> Self {
        Self {
            calls,
            bodies: BodyRetention::Keep,
        }
    }

    /// Returns the same emitter, writing bodies as `bodies` says.
    #[must_use]
    pub fn with_bodies(self, bodies: BodyRetention) -> Self {
        Self { bodies, ..self }
    }

    /// Returns the unwritten row of the sent call that starts at `start` and runs through
    /// `chain`.
    fn record(&self, start: CallStart, chain: Chain) -> CallRecord {
        CallRecord {
            sink: Arc::clone(&self.calls),
            bodies: self.bodies,
            measured: CallTelemetry::default(),
            pending: Some((start, chain)),
        }
    }
}

#[async_trait]
impl Middleware for EventEmitterMiddleware {
    fn name(&self) -> &'static str {
        "event_emitter"
    }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: Caller,
        next: Next<'_>,
    ) -> Result<LlmEventStream, OpenFailure> {
        let captured = CallStart::capture(&req, &ctx, next.model(), next.output_limit());
        // Recorded before the rest of the chain runs, so a call that fails before its stream
        // opens still hands its id back.
        if let Some(id) = logged_id(&captured) {
            next.record_call(id);
        }
        let (start, stamp) = match captured {
            Ok(captured) => captured,
            Err(e) => {
                tracing::warn!(error = %display_chain(&e), "event_emitter: call not logged");
                return next.run(req, ctx).await;
            }
        };
        // The transport records each attempt under this call's id, so its
        // wire rows and this row share one key.
        let mut ctx = ctx;
        ctx.call = Some(stamp);
        let chain = Chain {
            degraded: next.unsent().to_vec(),
            provider_id: next.provider_id().clone(),
            layers: std::iter::once(self.name())
                .chain(next.layer_names())
                .map(str::to_owned)
                .collect(),
        };
        let mut record = self.record(start, chain);

        match next.run(req, ctx).await {
            Ok(stream) => Ok(Box::pin(observe(stream, record))),
            Err(failure) => {
                record.measured = CallTelemetry::clone(&failure.measured);
                record.finish(before_stream(&failure.error));
                Err(failure)
            }
        }
    }

    /// Writes the refused call's row now. The check that refused it does not list what the
    /// request would also have been sent without, so the row does not know that.
    fn on_refusal(&self, refusal: &RefusedCall<'_>) {
        let captured = CallStart::capture(
            refusal.request(),
            refusal.ctx(),
            refusal.model(),
            refusal.output_limit(),
        );
        if let Some(id) = logged_id(&captured) {
            refusal.record_call(id);
        }
        let (start, _) = match captured {
            Ok(captured) => captured,
            Err(e) => {
                tracing::warn!(error = %e, "event_emitter: refused call not logged");
                return;
            }
        };
        match refusal_reasons(refusal.error()) {
            Some(reasons) => append(
                &*self.calls,
                self.bodies,
                call_row(start, CallTelemetry::default(), CallEnd::Refused(reasons)),
            ),
            None => tracing::warn!(
                error = %refusal.error(),
                "event_emitter: refused call not logged: the refusal names no reason"
            ),
        }
    }
}

/// Returns the id a call is logged under: its stamp's when it was captured, and none when it
/// could not be, since then no row is written.
fn logged_id(captured: &Result<(CallStart, LoggedCall), CaptureError>) -> Option<CallId> {
    match captured {
        Ok((_, stamp)) => Some(stamp.id),
        Err(_) => None,
    }
}

/// What the row records from the moment the call starts.
struct CallStart {
    call_id: CallId,
    tenant: TenantId,
    session_id: Option<SessionId>,
    trace_id: TraceId,
    model: String,
    request_body: String,
    request_sha256: [u8; 32],
    has_tools: bool,
    has_thinking: bool,
    has_structured_output: bool,
    temperature: Option<f32>,
    max_output_tokens: Option<u32>,
    output_limit: OutputLimit,
    tags: Vec<String>,
}

/// Why a call cannot be logged; it still runs.
#[derive(Debug, thiserror::Error)]
enum CaptureError {
    #[error("request does not serialize")]
    Request(#[from] serde_json::Error),
    #[error("reading the clock")]
    Clock(#[from] tars_types::ClockError),
    #[error(transparent)]
    CallId(#[from] tars_types::CallIdError),
}

impl CallStart {
    /// Reads the clock and stamps the call, the parts of the row that are not pure.
    fn capture(
        req: &ChatRequest,
        ctx: &Caller,
        model: &str,
        output_limit: &OutputLimit,
    ) -> Result<(Self, LoggedCall), CaptureError> {
        let at_ms = tars_types::unix_millis(SystemTime::now())?;
        let request_body = serde_json::to_string(req)?;
        let stamp = LoggedCall::new(at_ms)?;

        let start = Self {
            call_id: stamp.id,
            tenant: ctx.tenant_id.clone(),
            session_id: ctx.session_id,
            trace_id: ctx.trace_id.clone(),
            model: model.to_owned(),
            request_sha256: Sha256::digest(request_body.as_bytes()).into(),
            request_body,
            has_tools: !req.tools.is_empty(),
            has_thinking: matches!(
                req.thinking,
                Some(tars_types::ThinkingMode::Auto | tars_types::ThinkingMode::Budget(_))
            ),
            has_structured_output: req.structured_output.is_some(),
            temperature: req.temperature,
            max_output_tokens: req.max_output_tokens,
            output_limit: output_limit.clone(),
            tags: ctx.tags.clone(),
        };
        Ok((start, stamp))
    }
}

/// What the row records from the moment the call ends.
#[expect(
    clippy::large_enum_variant,
    reason = "almost every call is sent; boxing that variant would allocate per call for the rare refusal"
)]
enum CallEnd {
    Refused(NonEmpty<CompatibilityReason>),
    Sent { chain: Chain, ending: Ending },
}

/// The chain a call the dispatch layer let through runs into: what the request was sent without,
/// the provider at its end, and the names of its layers, outermost first, ending in `provider`.
struct Chain {
    degraded: Vec<Degradation>,
    provider_id: ProviderId,
    layers: Vec<String>,
}

/// How a sent call ended, with the validation summary its stream carried.
struct Ending {
    end: SentEnd,
    validation_summary: ValidationSummary,
}

impl Ending {
    /// Ends a call with no stream to carry a validation summary.
    fn without_stream(end: SentEnd) -> Self {
        Self {
            end,
            validation_summary: ValidationSummary::default(),
        }
    }
}

/// Returns every reason a refusal names; `None` when its error is not a refusal or names none.
fn refusal_reasons(e: &LlmError) -> Option<NonEmpty<CompatibilityReason>> {
    match e {
        LlmError::Incompatible { reasons } => Some(reasons.clone()),
        _ => None,
    }
}

/// Ends a sent call that failed before a stream opened. EventEmitter sits outside
/// ValidationMiddleware, so a reject arrives here still carrying its typed reason.
fn before_stream(e: &LlmError) -> Ending {
    Ending::without_stream(SentEnd::FailedBeforeStream(match e {
        LlmError::ValidationFailed { reason, .. } => CallFailure::ValidationFailed {
            reason: Some(ValidationReason::clone(reason)),
        },
        e => CallFailure::of_kind(e.kind()),
    }))
}

/// Ends a call whose stream is over: `terminal` says whether it delivered `Finished`, `failed`
/// the last error it yielded or the way it broke the event protocol. A stream that delivered
/// `Finished` but does not fold into a response failed as unfinished.
fn after_stream(
    terminal: bool,
    failed: Option<LlmErrorKind>,
    builder: ChatResponseBuilder,
) -> Ending {
    let usage = builder.usage();
    let validation_summary = builder.validation_summary().clone();
    let (failed, response) = match (terminal, failed) {
        (false, failed) => (failed, None),
        // [arc:intentional-handle] reason: the call already failed with `kind`; the response
        // is kept only when it folds, and why it does not adds nothing the failure lacks.
        (true, Some(kind)) => (Some(kind), builder.finish().ok().map(response_of)),
        (true, None) => match builder.finish() {
            Ok(built) => (None, Some(response_of(built))),
            Err(_) => (Some(LlmErrorKind::Unfinished), None),
        },
    };
    let end = match (failed, response) {
        (Some(kind), response) => StreamEnd::Failed {
            failure: CallFailure::of_kind(kind),
            response,
        },
        (None, Some(response)) => StreamEnd::Completed(response),
        (None, None) => StreamEnd::Unterminated,
    };
    Ending {
        end: SentEnd::Streamed { usage, end },
        validation_summary,
    }
}

/// Returns the response as the row keeps it; a response that does not serialize keeps the
/// serializer's error in place of its body.
fn response_of(built: ChatResponse) -> CallResponse {
    let body = match serde_json::to_string(&built) {
        Ok(text) => CallBody::Present(text),
        Err(e) => CallBody::Unserializable {
            error: e.to_string(),
        },
    };
    CallResponse {
        body,
        actual_model: built.actual_model,
        stop_reason: Some(built.stop_reason),
    }
}

/// Builds the row from the call's start, what the layers measured, and its end.
fn call_row(start: CallStart, measured: CallTelemetry, end: CallEnd) -> LlmCallRow {
    let (dispatch, layers, validation_summary) = match end {
        CallEnd::Refused(reasons) => (
            CallDispatch::Refused { reasons },
            Vec::new(),
            ValidationSummary::default(),
        ),
        CallEnd::Sent {
            chain,
            ending: Ending {
                end,
                validation_summary,
            },
        } => (
            CallDispatch::Sent {
                degraded: chain.degraded,
                provider_id: Some(chain.provider_id),
                end,
            },
            chain.layers,
            validation_summary,
        ),
    };
    LlmCallRow {
        call_id: start.call_id,
        tenant: start.tenant,
        session_id: start.session_id,
        trace_id: start.trace_id,
        model: start.model,
        request_body: CallBody::Present(start.request_body),
        request_sha256: start.request_sha256,
        has_tools: start.has_tools,
        has_thinking: start.has_thinking,
        has_structured_output: start.has_structured_output,
        temperature: start.temperature,
        max_output_tokens: start.max_output_tokens,
        output_limit: start.output_limit,
        dispatch,
        retry_attempts: measured.retry_attempts,
        provider_latency_ms: measured.provider_latency_ms,
        pipeline_total_ms: measured.pipeline_total_ms,
        layers,
        validation_summary,
        tags: start.tags,
    }
}

/// Appends `row` as `bodies` writes it; a row the sink refuses is logged as a warning.
fn append(sink: &dyn LogSink<LlmCallTable>, bodies: BodyRetention, row: LlmCallRow) {
    if let Err(e) = sink.try_append(bodies.apply(row)) {
        tracing::warn!(error = %display_chain(&e), "event_emitter: call row not logged");
    }
}

/// The one row of one sent call. Dropped unwritten ⇔ the caller dropped the call.
struct CallRecord {
    sink: Arc<dyn LogSink<LlmCallTable>>,
    bodies: BodyRetention,
    /// What the layers inside measured, as far as the call got.
    measured: CallTelemetry,
    /// `None` once the row is written.
    pending: Option<(CallStart, Chain)>,
}

impl CallRecord {
    fn finish(mut self, ending: Ending) {
        self.write(ending);
    }

    fn write(&mut self, ending: Ending) {
        let Some((start, chain)) = self.pending.take() else {
            return;
        };
        let measured = std::mem::take(&mut self.measured);
        let row = call_row(start, measured, CallEnd::Sent { chain, ending });
        append(&*self.sink, self.bodies, row);
    }
}

impl Drop for CallRecord {
    fn drop(&mut self) {
        self.write(Ending::without_stream(SentEnd::Cancelled));
    }
}

/// Passes the stream through, building the response as it goes, and writes the row at
/// `Finished`, or at the end of a stream that never delivered one.
fn observe(
    inner: LlmEventStream,
    record: CallRecord,
) -> impl futures::Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static {
    async_stream::stream! {
        let mut inner = inner;
        let mut open = Some((record, ChatResponseBuilder::new()));
        let mut failed = None;

        while let Some(item) = inner.next().await {
            if let Some((record, builder)) = open.as_mut() {
                match &item {
                    Ok(ChatEvent::Measured { telemetry }) => {
                        record.measured = std::mem::take(&mut record.measured).merge(telemetry.clone());
                    }
                    Ok(ev) => {
                        if builder.apply(ev.clone()).is_err() {
                            failed = Some(LlmErrorKind::StreamViolation);
                        }
                    }
                    Err(e) => failed = Some(e.kind()),
                }
            }
            if matches!(item, Ok(ChatEvent::Finished { .. }))
                && let Some((record, builder)) = open.take()
            {
                record.finish(after_stream(true, failed, builder));
            }
            yield item;
        }

        if let Some((record, builder)) = open.take() {
            record.finish(after_stream(false, failed, builder));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmService;
    use crate::pipeline::middleware::validation::{OutputValidator, ValidationMiddleware};
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use tars_melt::event::call_log::CALL_LOG_SINK;
    use tars_melt::event::call_log::CallOutcome;
    use tars_melt::event::log::{LogSource, MemLog, TenantScope, TimeWindow};
    use tars_types::Degradation;
    use tars_types::ProviderId;
    use tokio::runtime::Handle;

    async fn drain(s: LlmEventStream) -> Vec<ChatEvent> {
        let mut s = s;
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev.unwrap());
        }
        out
    }

    /// The log a test's emitter writes into, and the sink to close before
    /// reading it.
    struct Logged {
        log: MemLog<LlmCallTable>,
        sink: Arc<dyn LogSink<LlmCallTable>>,
    }

    impl Logged {
        fn new() -> Self {
            let log = MemLog::<LlmCallTable>::new();
            let sink = Arc::new(log.sink(CALL_LOG_SINK, &Handle::current()));
            Self { log, sink }
        }

        /// Emitter outermost, then the given validators, then `provider`.
        fn service(
            &self,
            provider: Arc<MockProvider>,
            validators: Vec<Arc<dyn OutputValidator>>,
        ) -> LlmService {
            let builder = LlmService::builder(provider, "m")
                .layer(EventEmitterMiddleware::new(self.sink.clone()));
            if validators.is_empty() {
                builder.build()
            } else {
                builder.layer(ValidationMiddleware::new(validators)).build()
            }
        }

        /// Seals everything written so far and reads back the calls of the tenant the tests'
        /// requests carry.
        async fn rows(&self) -> Vec<LlmCallRow> {
            self.sink.close().await.unwrap();
            let tenant = Caller::test_default().tenant_id;
            let scope = TenantScope::new(tenant, TimeWindow::all());
            self.log.scan(&scope).await.unwrap()
        }
    }

    #[tokio::test]
    async fn happy_path_emits_one_row_with_bodies() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            vec![],
        );

        let stream = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await
            .result
            .unwrap();
        let _ = drain(stream).await;

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1, "exactly one row");
        let row = &stored[0];
        assert_eq!(row.model, "m");
        assert_eq!(row.outcome(), CallOutcome::Completed);
        assert!(
            row.request_body.text().is_some_and(|b| b.contains("hi")),
            "request body inline"
        );
        let response = row.response().expect("response present on Ok");
        assert!(
            response.body.text().is_some_and(|b| b.contains("hello")),
            "response body inline"
        );
    }

    /// A writer withholding bodies still logs the call — model, outcome, usage, the
    /// request's hash — but neither body reaches the log.
    #[tokio::test]
    async fn a_withholding_emitter_logs_the_call_without_its_bodies() {
        let logged = Logged::new();
        let svc = LlmService::builder(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            "m",
        )
        .layer(
            EventEmitterMiddleware::new(logged.sink.clone()).with_bodies(BodyRetention::Withhold),
        )
        .build();

        let stream = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await
            .result
            .unwrap();
        let _ = drain(stream).await;

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1, "the call is still logged");
        let row = &stored[0];
        assert_eq!(
            (row.model.as_str(), &row.outcome()),
            ("m", &CallOutcome::Completed)
        );
        assert_eq!(row.request_body, CallBody::Withheld);
        let response = row.response().expect("the response is still described");
        assert_eq!(response.body, CallBody::Withheld);
        assert!(row.usage().is_some(), "usage still recorded");
        assert_ne!(
            row.request_sha256, [0; 32],
            "the request is still identified"
        );
    }

    /// A call made outside any session is logged with no session, not under a made-up one every
    /// such call would share.
    #[tokio::test]
    async fn a_call_outside_any_session_logs_no_session() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("ok")),
            vec![],
        );
        let test = Caller::test_default();
        let sessionless = Caller::new(test.tenant_id, test.trace_id);

        let stream = svc
            .call(ChatRequest::user("hi"), sessionless)
            .await
            .result
            .unwrap();
        let _ = drain(stream).await;

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1, "exactly one row");
        assert_eq!(stored[0].session_id, None);
    }

    #[tokio::test]
    async fn a_call_in_a_session_logs_that_session() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("ok")),
            vec![],
        );
        let test = Caller::test_default();
        let session = SessionId::new_v7(1_790_553_600_000).unwrap();
        let in_session = Caller::new(test.tenant_id, test.trace_id).with_session(session);

        let stream = svc
            .call(ChatRequest::user("hi"), in_session)
            .await
            .result
            .unwrap();
        let _ = drain(stream).await;

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1, "exactly one row");
        assert_eq!(stored[0].session_id, Some(session));
    }

    #[tokio::test]
    async fn validation_summary_propagates_into_row() {
        use crate::pipeline::middleware::validation::builtin::MaxLengthValidator;
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello world")),
            vec![Arc::new(MaxLengthValidator::truncate_above(5))],
        );

        let stream = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await
            .result
            .unwrap();
        let _ = drain(stream).await;

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].validation_summary.validators_run,
            vec!["max_length"]
        );
        assert_eq!(
            stored[0].outcome(),
            CallOutcome::Completed,
            "success path → no reject reason"
        );
    }

    #[tokio::test]
    async fn validation_reject_reason_propagates_into_row() {
        use crate::pipeline::middleware::validation::builtin::NotEmptyValidator;
        let logged = Logged::new();
        // Empty text → NotEmpty rejects.
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("")),
            vec![Arc::new(NotEmptyValidator::new())],
        );

        let result = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await
            .result;
        assert!(
            matches!(result, Err(LlmError::ValidationFailed { .. })),
            "expected the reject to surface as Err at the outer boundary"
        );

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1);
        let row = &stored[0];
        match &row.outcome() {
            CallOutcome::Failed(CallFailure::ValidationFailed {
                reason: Some(ValidationReason::NotEmpty { field }),
            }) => assert_eq!(field, "text"),
            other => panic!("expected ValidationFailed with a NotEmpty reason, got {other:?}"),
        }
        assert!(row.response().is_none() && row.usage().is_none());
    }

    #[tokio::test]
    async fn dropping_the_stream_before_it_ends_logs_one_cancelled_row() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            vec![],
        );

        let mut stream = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await
            .result
            .unwrap();
        let first = stream.next().await.expect("one event").unwrap();
        assert!(!matches!(first, ChatEvent::Finished { .. }), "{first:?}");
        drop(stream);

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].outcome(), CallOutcome::Cancelled);
        assert!(stored[0].response().is_none());
    }

    #[tokio::test]
    async fn stopping_at_finished_still_logs_the_completed_call() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            vec![],
        );

        let mut stream = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await
            .result
            .unwrap();
        while let Some(ev) = stream.next().await {
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                break;
            }
        }
        drop(stream);

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].outcome(), CallOutcome::Completed);
    }
    #[tokio::test]
    async fn every_row_the_emitter_writes_is_live() {
        let logged = Logged::new();
        let ok = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            vec![],
        );
        let failing = logged.service(
            MockProvider::new(ProviderId::new("p2"), CannedResponse::Error("boom".into())),
            vec![],
        );

        let _ = drain(
            ok.call(ChatRequest::user("done"), Caller::test_default())
                .await
                .result
                .unwrap(),
        )
        .await;
        let mut cancelled = ok
            .call(ChatRequest::user("dropped"), Caller::test_default())
            .await
            .result
            .unwrap();
        let _ = cancelled.next().await;
        drop(cancelled);
        let _ = failing
            .call(ChatRequest::user("failed"), Caller::test_default())
            .await
            .result;

        let stored = logged.rows().await;
        let outcomes: Vec<CallOutcome> = stored.iter().map(|r| r.outcome()).collect();
        assert_eq!(outcomes.len(), 3, "{outcomes:?}");
        assert!(outcomes.contains(&CallOutcome::Completed), "{outcomes:?}");
        assert!(outcomes.contains(&CallOutcome::Cancelled), "{outcomes:?}");
        assert!(
            outcomes.iter().any(|o| matches!(o, CallOutcome::Failed(_))),
            "{outcomes:?}"
        );
    }

    /// Serves canned text under a profile that claims nothing.
    struct ClaimsNothing {
        inner: Arc<MockProvider>,
        profile: tars_types::ProviderProfile,
    }

    #[async_trait]
    impl crate::LlmProvider for ClaimsNothing {
        fn id(&self) -> &tars_types::ProviderId {
            self.inner.id()
        }
        fn capabilities(&self) -> &tars_types::ProviderProfile {
            &self.profile
        }
        async fn stream(
            self: Arc<Self>,
            req: ChatRequest,
            model: &str,
            ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            self.inner.clone().stream(req, model, ctx).await
        }
    }

    #[tokio::test]
    async fn the_row_records_what_the_request_was_sent_without_and_empty_when_sent_whole() {
        let logged = Logged::new();
        let provider = Arc::new(ClaimsNothing {
            inner: MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            profile: tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http),
        });
        let svc = LlmService::builder(provider, "m")
            .layer(EventEmitterMiddleware::new(logged.sink.clone()))
            .build();

        let mut left_out = ChatRequest::user("left-out");
        left_out.enable_chat_template_thinking = Some(false);
        for req in [left_out, ChatRequest::user("whole")] {
            let _ = drain(svc.call(req, Caller::test_default()).await.result.unwrap()).await;
        }

        let stored = logged.rows().await;
        let degraded_of = |text: &str| {
            stored
                .iter()
                .find(|r| r.request_body.text().is_some_and(|b| b.contains(text)))
                .map(|r| r.degraded().map(<[Degradation]>::to_vec))
        };
        assert_eq!(
            degraded_of("left-out"),
            Some(Some(vec![Degradation::ChatTemplateThinkingNotSent]))
        );
        assert_eq!(degraded_of("whole"), Some(Some(Vec::new())));
    }

    #[tokio::test]
    async fn a_refused_call_gets_a_row_of_its_own_and_a_served_one_its_usual_row() {
        let logged = Logged::new();
        let provider = Arc::new(ClaimsNothing {
            inner: MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            profile: tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http),
        });
        let svc = LlmService::builder(provider, "m")
            .layer(EventEmitterMiddleware::new(logged.sink.clone()))
            .build();

        let mut refused = ChatRequest::user("refused");
        refused.thinking = Some(tars_types::ThinkingMode::Auto);
        let refusal = svc.call(refused, Caller::test_default()).await.result;
        assert!(matches!(refusal, Err(LlmError::Incompatible { .. })));
        let _ = drain(
            svc.call(ChatRequest::user("served"), Caller::test_default())
                .await
                .result
                .unwrap(),
        )
        .await;

        let stored = logged.rows().await;
        assert_eq!(stored.len(), 2, "one row per call");
        let row_of = |text: &str| {
            stored
                .iter()
                .find(|r| r.request_body.text().is_some_and(|b| b.contains(text)))
                .expect("the call has a row")
        };
        let refused = row_of("refused");
        assert!(
            matches!(refused.outcome(), CallOutcome::Refused(_)),
            "{:?}",
            refused.outcome()
        );
        assert_eq!(refused.degraded(), None);
        assert_eq!(refused.model, "m");
        assert!(refused.layers.is_empty(), "{:?}", refused.layers);
        assert!(refused.provider_id().is_none() && refused.response().is_none());
        assert_eq!(refused.max_output_tokens, None, "the caller's own value");

        let served = row_of("served");
        assert_eq!(served.outcome(), CallOutcome::Completed);
        assert_eq!(served.degraded(), Some(&[][..]));
    }

    #[tokio::test]
    async fn a_refused_call_s_row_names_every_reason_it_was_refused_for() {
        let logged = Logged::new();
        let provider = Arc::new(ClaimsNothing {
            inner: MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            profile: tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http),
        });
        let svc = LlmService::builder(provider, "m")
            .layer(EventEmitterMiddleware::new(logged.sink.clone()))
            .build();

        let mut refused = ChatRequest::user("refused");
        refused.thinking = Some(tars_types::ThinkingMode::Auto);
        let result = svc.call(refused, Caller::test_default()).await.result;
        let Err(LlmError::Incompatible { reasons }) = result else {
            panic!("the call is refused");
        };
        let _ = drain(
            svc.call(ChatRequest::user("served"), Caller::test_default())
                .await
                .result
                .unwrap(),
        )
        .await;

        let stored = logged.rows().await;
        let outcome_of = |text: &str| {
            stored
                .iter()
                .find(|r| r.request_body.text().is_some_and(|b| b.contains(text)))
                .expect("the call has a row")
                .outcome()
                .clone()
        };
        assert!(!reasons.is_empty());
        assert_eq!(outcome_of("refused"), CallOutcome::Refused(reasons));
        assert_eq!(outcome_of("served"), CallOutcome::Completed);
    }

    #[tokio::test]
    async fn a_call_with_a_thinking_budget_gets_its_row() {
        let logged = Logged::new();
        let provider = Arc::new(ClaimsNothing {
            inner: MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            profile: tars_types::ProviderProfile {
                supports_thinking: tars_types::Support::Supported,
                ..tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http)
            },
        });
        let svc = LlmService::builder(provider, "m")
            .layer(EventEmitterMiddleware::new(logged.sink.clone()))
            .build();
        let mut budgeted = ChatRequest::user("budgeted");
        budgeted.thinking = Some(tars_types::ThinkingMode::Budget(1024));

        let call = svc.call(budgeted, Caller::test_default()).await;
        let _ = drain(call.result.unwrap()).await;

        let stored = logged.rows().await;
        assert!(
            matches!(stored.as_slice(), [row] if row.outcome() == CallOutcome::Completed),
            "{stored:?}"
        );
        assert_eq!(call.call_ids, [stored[0].call_id]);
    }

    #[tokio::test]
    async fn a_call_with_an_image_gets_its_row_with_the_image_in_its_request() {
        let logged = Logged::new();
        let provider = Arc::new(ClaimsNothing {
            inner: MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            profile: tars_types::ProviderProfile {
                modalities_in: tars_types::Modalities::listed([
                    tars_types::Modality::Text,
                    tars_types::Modality::Image,
                ]),
                ..tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http)
            },
        });
        let svc = LlmService::builder(provider, "m")
            .layer(EventEmitterMiddleware::new(logged.sink.clone()))
            .build();
        let mut pictured = ChatRequest::user("what is this");
        pictured.messages.push(tars_types::Message::User {
            content: vec![tars_types::ContentBlock::Image {
                mime: "image/png".into(),
                data: tars_types::ImageData::Url("https://x/y.png".into()),
            }],
        });

        let call = svc.call(pictured, Caller::test_default()).await;
        let _ = drain(call.result.unwrap()).await;

        let stored = logged.rows().await;
        let [row] = stored.as_slice() else {
            panic!("one row: {stored:?}");
        };
        assert!(
            row.request_body
                .text()
                .is_some_and(|b| b.contains(r#""data":{"url":"https://x/y.png"}"#)),
            "{row:?}"
        );
    }

    #[tokio::test]
    async fn a_call_refused_for_its_image_gets_its_row() {
        let logged = Logged::new();
        let svc = LlmService::builder(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            "m",
        )
        .layer(EventEmitterMiddleware::new(logged.sink.clone()))
        .build();
        let mut pictured = ChatRequest::user("what is this");
        pictured.messages.push(tars_types::Message::User {
            content: vec![tars_types::ContentBlock::Image {
                mime: "image/png".into(),
                data: tars_types::ImageData::Base64("AA==".into()),
            }],
        });

        let call = svc.call(pictured, Caller::test_default()).await;
        assert!(matches!(call.result, Err(LlmError::Incompatible { .. })));

        let stored = logged.rows().await;
        let [row] = stored.as_slice() else {
            panic!("one row: {stored:?}");
        };
        assert_eq!(
            row.outcome().refusal_reasons(),
            Some(&[tars_types::CompatibilityReason::VisionUnsupported][..])
        );
    }

    #[tokio::test]
    async fn a_call_refused_for_its_thinking_budget_names_the_budget_in_its_row() {
        let logged = Logged::new();
        let provider = Arc::new(ClaimsNothing {
            inner: MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            profile: tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http),
        });
        let svc = LlmService::builder(provider, "m")
            .layer(EventEmitterMiddleware::new(logged.sink.clone()))
            .build();
        let mut refused = ChatRequest::user("refused");
        refused.thinking = Some(tars_types::ThinkingMode::Budget(1024));

        let call = svc.call(refused, Caller::test_default()).await;
        assert!(matches!(call.result, Err(LlmError::Incompatible { .. })));

        let stored = logged.rows().await;
        let [row] = stored.as_slice() else {
            panic!("one row: {stored:?}");
        };
        assert_eq!(
            row.outcome().refusal_reasons(),
            Some(
                &[tars_types::CompatibilityReason::ThinkingUnsupported {
                    mode: tars_types::ThinkingMode::Budget(1024),
                }][..]
            )
        );
    }

    /// Returns the id of the one row `logged` holds.
    async fn only_row_id(logged: &Logged) -> CallId {
        let stored = logged.rows().await;
        assert_eq!(stored.len(), 1, "{stored:?}");
        stored[0].call_id
    }

    #[tokio::test]
    async fn a_completed_call_returns_the_id_of_its_row() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            vec![],
        );

        let call = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await;
        let _ = drain(call.result.unwrap()).await;

        assert_eq!(call.call_ids, [only_row_id(&logged).await]);
    }

    #[tokio::test]
    async fn a_call_that_fails_before_its_stream_returns_the_id_of_its_failed_row() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::Error("down".into())),
            vec![],
        );

        let call = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await;
        assert!(call.result.is_err());

        let stored = logged.rows().await;
        assert!(
            matches!(stored.as_slice(), [row] if matches!(row.outcome(), CallOutcome::Failed(_))),
            "{stored:?}"
        );
        assert_eq!(call.call_ids, [stored[0].call_id]);
    }

    #[tokio::test]
    async fn a_refused_call_returns_the_id_of_its_refused_row() {
        let logged = Logged::new();
        let provider = Arc::new(ClaimsNothing {
            inner: MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            profile: tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http),
        });
        let svc = LlmService::builder(provider, "m")
            .layer(EventEmitterMiddleware::new(logged.sink.clone()))
            .build();
        let mut refused = ChatRequest::user("refused");
        refused.thinking = Some(tars_types::ThinkingMode::Auto);

        let call = svc.call(refused, Caller::test_default()).await;
        assert!(matches!(call.result, Err(LlmError::Incompatible { .. })));

        assert_eq!(call.call_ids, [only_row_id(&logged).await]);
    }

    #[tokio::test]
    async fn a_chain_without_an_emitter_returns_no_ids() {
        let svc = LlmService::of(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            "m",
        );

        let call = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await;
        let _ = drain(call.result.unwrap()).await;

        assert!(call.call_ids.is_empty(), "{:?}", call.call_ids);
    }

    /// Runs the rest of the chain twice and returns the second stream, as a hedging or
    /// fallback layer a user puts outside the emitter would.
    struct RunsTwice;

    #[async_trait]
    impl Middleware for RunsTwice {
        fn name(&self) -> &'static str {
            "runs_twice"
        }
        async fn handle(
            &self,
            req: ChatRequest,
            ctx: Caller,
            next: Next<'_>,
        ) -> Result<LlmEventStream, OpenFailure> {
            let _ = drain(next.run(req.clone(), ctx.clone()).await?).await;
            next.run(req, ctx).await
        }
    }

    #[tokio::test]
    async fn a_layer_outside_the_emitter_that_runs_next_twice_returns_both_ids() {
        let logged = Logged::new();
        let svc = LlmService::builder(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            "m",
        )
        .layer(RunsTwice)
        .layer(EventEmitterMiddleware::new(logged.sink.clone()))
        .build();

        let call = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await;
        let _ = drain(call.result.unwrap()).await;

        let mut rows: Vec<CallId> = logged.rows().await.iter().map(|r| r.call_id).collect();
        let mut returned = call.call_ids.clone();
        rows.sort();
        returned.sort();
        assert_eq!(returned.len(), 2, "{returned:?}");
        assert_eq!(returned, rows);
    }

    #[tokio::test]
    async fn two_emitters_return_two_ids_outer_first() {
        let (outer, inner) = (Logged::new(), Logged::new());
        let svc = LlmService::builder(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            "m",
        )
        .layer(EventEmitterMiddleware::new(outer.sink.clone()))
        .layer(EventEmitterMiddleware::new(inner.sink.clone()))
        .build();

        let call = svc
            .call(ChatRequest::user("hi"), Caller::test_default())
            .await;
        let _ = drain(call.result.unwrap()).await;

        assert_eq!(
            call.call_ids,
            [only_row_id(&outer).await, only_row_id(&inner).await]
        );
    }

    #[tokio::test]
    async fn a_captured_call_is_logged_under_its_stamp() {
        let captured = CallStart::capture(
            &ChatRequest::user("hi"),
            &Caller::test_default(),
            "m",
            &OutputLimit::Unknown,
        );
        let stamped = captured.as_ref().map(|(_, stamp)| stamp.id).unwrap();
        assert_eq!(logged_id(&captured), Some(stamped));
    }

    #[test]
    fn a_call_whose_capture_fails_has_no_id() {
        let failed = Err(CaptureError::CallId(
            tars_types::CallIdError::TimeOutOfRange(-1),
        ));
        assert_eq!(logged_id(&failed), None);
    }

    #[tokio::test]
    async fn a_reused_context_does_not_carry_ids_between_calls() {
        let logged = Logged::new();
        let svc = logged.service(
            MockProvider::new(ProviderId::new("p1"), CannedResponse::text("hello")),
            vec![],
        );
        let ctx = Caller::test_default();

        let first = svc.call(ChatRequest::user("one"), ctx.clone()).await;
        let _ = drain(first.result.unwrap()).await;
        let second = svc.call(ChatRequest::user("two"), ctx).await;
        let _ = drain(second.result.unwrap()).await;

        let stored = logged.rows().await;
        let id_of = |text: &str| {
            stored
                .iter()
                .find(|r| r.request_body.text().is_some_and(|b| b.contains(text)))
                .map(|r| r.call_id)
                .unwrap()
        };
        assert_eq!(first.call_ids, [id_of("one")]);
        assert_eq!(second.call_ids, [id_of("two")]);
    }
}
