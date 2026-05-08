//! `EventEmitterMiddleware` — emits one `LlmCallFinished` per
//! `Pipeline.call` boundary to a `PipelineEventStore` + `BodyStore`.
//! See [Doc 17 §8](../../docs/17-pipeline-event-store.md).
//!
//! Position in the onion: **outermost layer** (added FIRST to the
//! builder so it ends up wrapping everything else). Reads telemetry +
//! validation_summary from the shared context AFTER inner layers
//! populate them.
//!
//! Write semantics: **fire-and-forget**. After the stream drains, the
//! event + bodies are written in a `tokio::spawn`'d task; the caller's
//! response path doesn't block on storage I/O. Write failures degrade
//! silently with a `tracing::warn!` (same pattern as
//! `cache.rs::wrap_stream_for_write`).

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use tars_provider::LlmEventStream;
use tars_storage::{BodyStore, PipelineEventStore};
use tars_types::{
    CallResult, ChatEvent, ChatRequest, ChatResponseBuilder, ContentRef, LlmCallFinished,
    PipelineEvent, ProviderError, RequestContext,
};

use crate::middleware::Middleware;
use crate::service::LlmService;

/// Middleware that emits one `LlmCallFinished` per `Pipeline.call`
/// to the configured stores.
#[derive(Clone)]
pub struct EventEmitterMiddleware {
    events: Arc<dyn PipelineEventStore>,
    bodies: Arc<dyn BodyStore>,
}

impl EventEmitterMiddleware {
    pub fn new(events: Arc<dyn PipelineEventStore>, bodies: Arc<dyn BodyStore>) -> Self {
        Self { events, bodies }
    }
}

impl Middleware for EventEmitterMiddleware {
    fn name(&self) -> &'static str {
        "event_emitter"
    }
    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(EventEmitterService {
            inner,
            events: self.events.clone(),
            bodies: self.bodies.clone(),
        })
    }
}

struct EventEmitterService {
    inner: Arc<dyn LlmService>,
    events: Arc<dyn PipelineEventStore>,
    bodies: Arc<dyn BodyStore>,
}

#[async_trait]
impl LlmService for EventEmitterService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Layer trace.
        if let Ok(mut t) = ctx.telemetry.lock() {
            t.layers.push("event_emitter".into());
        }

        // Snapshot what we need to build the event AFTER the call.
        // We can't move ctx (inner needs it) so clone the handles.
        let telemetry_handle = ctx.telemetry.clone();
        let validation_handle = ctx.validation_outcome.clone();
        let tenant_id = ctx.tenant_id.clone();
        let session_id = Some(ctx.session_id.clone());
        let trace_id = Some(ctx.trace_id.clone());
        let tags = ctx.tags.clone();

        // Capture inline request properties before moving into inner.
        // `provider_id` is set by `ProviderService` onto telemetry; we
        // read it back in `build_event`. The empty default here is a
        // sentinel for "not yet resolved" — never makes it into the
        // emitted event.
        let provider_id = tars_types::ProviderId::new("unresolved");
        let actual_model = req.model.label().to_string();
        let has_tools = !req.tools.is_empty();
        let has_thinking = !req.thinking.is_off();
        let has_structured_output = req.structured_output.is_some();
        let temperature = req.temperature;
        let max_output_tokens = req.max_output_tokens;

        // Serialize the request body once — used for both the
        // ContentRef hash and the BodyStore write.
        let req_body_bytes = match serde_json::to_vec(&req) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "event_emitter: request serialize failed; emitting empty body ref",
                );
                Vec::new()
            }
        };
        let request_fingerprint: [u8; 32] = {
            let mut h = Sha256::new();
            h.update(&req_body_bytes);
            h.finalize().into()
        };
        let request_ref = ContentRef::from_body(tenant_id.clone(), &req_body_bytes);

        let result = self.inner.clone().call(req, ctx).await;

        // Build the failure-path event immediately; success path needs
        // to wait until the stream drains so we capture the response.
        let result_for_error = match &result {
            Ok(_) => None,
            Err(e) => Some(CallResult::Error { kind: e.kind().to_string() }),
        };

        match result {
            Ok(stream) => {
                let observed = wrap_stream_for_emit(
                    stream,
                    StreamCtx {
                        events: self.events.clone(),
                        bodies: self.bodies.clone(),
                        req_body_bytes,
                        request_fingerprint,
                        request_ref,
                        actual_model,
                        provider_id,
                        tenant_id,
                        session_id,
                        trace_id,
                        has_tools,
                        has_thinking,
                        has_structured_output,
                        temperature,
                        max_output_tokens,
                        telemetry_handle,
                        validation_handle,
                        tags,
                    },
                );
                Ok(Box::pin(observed))
            }
            Err(e) => {
                // Failure before the stream opened. Emit synchronously
                // (no stream to wrap). Still fire-and-forget.
                let stores = (self.events.clone(), self.bodies.clone());
                let event = build_event(
                    EventInputs {
                        result: result_for_error.expect("error path"),
                        response_body: None,
                        usage: tars_types::Usage::default(),
                        stop_reason: None,
                        req_body_bytes,
                        request_fingerprint,
                        request_ref: request_ref.clone(),
                        actual_model,
                        provider_id,
                        tenant_id: tenant_id.clone(),
                        session_id,
                        trace_id,
                        has_tools,
                        has_thinking,
                        has_structured_output,
                        temperature,
                        max_output_tokens,
                        telemetry_handle,
                        validation_handle,
                        tags,
                    },
                );
                tokio::spawn(async move {
                    fire_and_forget(stores.0, stores.1, event, tenant_id, request_ref).await;
                });
                Err(e)
            }
        }
    }
}

/// Bundle the per-call data the stream wrapper needs to keep alive
/// across the stream's lifetime. Passing as a struct cuts the function
/// signature noise.
struct StreamCtx {
    events: Arc<dyn PipelineEventStore>,
    bodies: Arc<dyn BodyStore>,
    req_body_bytes: Vec<u8>,
    request_fingerprint: [u8; 32],
    request_ref: ContentRef,
    actual_model: String,
    provider_id: tars_types::ProviderId,
    tenant_id: tars_types::TenantId,
    session_id: Option<tars_types::SessionId>,
    trace_id: Option<tars_types::TraceId>,
    has_tools: bool,
    has_thinking: bool,
    has_structured_output: bool,
    temperature: Option<f32>,
    max_output_tokens: Option<u32>,
    telemetry_handle: tars_types::SharedTelemetry,
    validation_handle: tars_types::SharedValidationOutcome,
    tags: Vec<String>,
}

struct EventInputs {
    result: CallResult,
    response_body: Option<ContentRef>,
    usage: tars_types::Usage,
    stop_reason: Option<tars_types::StopReason>,
    req_body_bytes: Vec<u8>,
    request_fingerprint: [u8; 32],
    request_ref: ContentRef,
    actual_model: String,
    provider_id: tars_types::ProviderId,
    tenant_id: tars_types::TenantId,
    session_id: Option<tars_types::SessionId>,
    trace_id: Option<tars_types::TraceId>,
    has_tools: bool,
    has_thinking: bool,
    has_structured_output: bool,
    temperature: Option<f32>,
    max_output_tokens: Option<u32>,
    telemetry_handle: tars_types::SharedTelemetry,
    validation_handle: tars_types::SharedValidationOutcome,
    tags: Vec<String>,
}

/// Build the `LlmCallFinished` from end-of-call data. Pure — no I/O
/// here so it can be unit-tested without async.
fn build_event(i: EventInputs) -> LlmCallFinished {
    let _ = i.req_body_bytes; // body content already hashed into request_ref + stored

    let telemetry = i.telemetry_handle.lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let validation_summary = i.validation_handle.lock()
        .map(|g| g.summary.clone())
        .unwrap_or_default();

    // Provider id is stamped onto telemetry by `ProviderService` once
    // routing has resolved which provider runs. If telemetry never
    // saw a provider (e.g. cache hit short-circuited), fall back to
    // a sentinel so the event still persists.
    let resolved_provider_id = telemetry
        .provider_id
        .as_deref()
        .map(tars_types::ProviderId::new)
        .unwrap_or_else(|| {
            if i.provider_id.as_ref().is_empty() {
                tars_types::ProviderId::new("unresolved")
            } else {
                i.provider_id.clone()
            }
        });

    LlmCallFinished {
        event_id: Uuid::new_v4(),
        timestamp: SystemTime::now(),
        tenant_id: i.tenant_id,
        session_id: i.session_id,
        trace_id: i.trace_id,
        provider_id: resolved_provider_id,
        actual_model: i.actual_model,
        request_fingerprint: i.request_fingerprint,
        request_ref: i.request_ref,
        has_tools: i.has_tools,
        has_thinking: i.has_thinking,
        has_structured_output: i.has_structured_output,
        temperature: i.temperature,
        max_output_tokens: i.max_output_tokens,
        response_ref: i.response_body,
        usage: i.usage,
        stop_reason: i.stop_reason,
        telemetry,
        validation_summary,
        result: i.result,
        tags: i.tags,
    }
}

/// Fire-and-forget write of one event + bodies. Failures degrade
/// silently; the caller's response path doesn't depend on storage I/O.
async fn fire_and_forget(
    events: Arc<dyn PipelineEventStore>,
    bodies: Arc<dyn BodyStore>,
    event: LlmCallFinished,
    _tenant: tars_types::TenantId,
    _request_ref: ContentRef,
) {
    // Bodies are written by the stream wrapper before this is called
    // (see wrap_stream_for_emit). Here we only persist the event row.
    let to_emit = PipelineEvent::LlmCallFinished(Box::new(event));
    if let Err(e) = events.append(&[to_emit]).await {
        tracing::warn!(
            error = %e,
            "event_emitter: event append failed (degraded silently)",
        );
    }
    let _ = bodies; // bodies already written; keeping the param for future v1.1 retry-write
}

/// Wrap the inner stream so we observe every event, build the
/// `ChatResponse` for body storage, and fire the event after Finished.
fn wrap_stream_for_emit(
    inner: LlmEventStream,
    sc: StreamCtx,
) -> impl futures::Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static {
    async_stream::stream! {
        let mut s = inner;
        let mut builder = ChatResponseBuilder::new();
        let mut saw_terminal = false;
        let mut stream_error_kind: Option<&'static str> = None;

        while let Some(item) = s.next().await {
            match &item {
                Ok(ev) => {
                    if matches!(ev, ChatEvent::Finished { .. }) {
                        saw_terminal = true;
                    }
                    builder.apply(ev.clone());
                }
                Err(e) => {
                    stream_error_kind = Some(e.kind());
                }
            }
            yield item;
        }

        // Stream drained. Build the post-stream pieces.
        let response = builder.finish();
        let response_body_bytes_opt: Option<Vec<u8>> = if saw_terminal {
            match serde_json::to_vec(&response) {
                Ok(b) => Some(b),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "event_emitter: response serialize failed; storing without body ref",
                    );
                    None
                }
            }
        } else {
            None
        };
        let response_ref = response_body_bytes_opt.as_ref().map(|bytes| {
            ContentRef::from_body(sc.tenant_id.clone(), bytes)
        });

        let usage = response.usage;
        let stop_reason = response.stop_reason;

        let result = match (saw_terminal, stream_error_kind) {
            (true, None) => CallResult::Ok,
            (_, Some(kind)) => CallResult::Error { kind: kind.to_string() },
            (false, None) => CallResult::Error { kind: "internal".to_string() },
        };

        // Bodies first: write request + response so the event row's
        // ContentRefs always resolve. Use spawn so the caller's stream
        // doesn't block on storage I/O — main reason the wrapper is
        // here at all.
        let bodies_for_spawn = sc.bodies.clone();
        let events_for_spawn = sc.events.clone();
        let tenant_for_spawn = sc.tenant_id.clone();
        let request_ref_for_spawn = sc.request_ref.clone();
        let response_ref_for_spawn = response_ref.clone();
        let req_body_bytes_for_spawn = sc.req_body_bytes.clone();

        let event = build_event(EventInputs {
            result,
            response_body: response_ref,
            usage,
            stop_reason,
            req_body_bytes: sc.req_body_bytes,
            request_fingerprint: sc.request_fingerprint,
            request_ref: sc.request_ref,
            actual_model: sc.actual_model,
            provider_id: sc.provider_id,
            tenant_id: sc.tenant_id,
            session_id: sc.session_id,
            trace_id: sc.trace_id,
            has_tools: sc.has_tools,
            has_thinking: sc.has_thinking,
            has_structured_output: sc.has_structured_output,
            temperature: sc.temperature,
            max_output_tokens: sc.max_output_tokens,
            telemetry_handle: sc.telemetry_handle,
            validation_handle: sc.validation_handle,
            tags: sc.tags,
        });

        tokio::spawn(async move {
            // Bodies — request always; response only if we have it.
            if let Err(e) = bodies_for_spawn
                .put(&request_ref_for_spawn, Bytes::from(req_body_bytes_for_spawn))
                .await
            {
                tracing::warn!(
                    error = %e,
                    "event_emitter: request body write failed (degraded silently)",
                );
            }
            if let (Some(rref), Some(rbytes)) = (response_ref_for_spawn, response_body_bytes_opt) {
                if let Err(e) = bodies_for_spawn.put(&rref, Bytes::from(rbytes)).await {
                    tracing::warn!(
                        error = %e,
                        "event_emitter: response body write failed (degraded silently)",
                    );
                }
            }
            // Event row — depends on bodies, write last.
            fire_and_forget(events_for_spawn, bodies_for_spawn, event, tenant_for_spawn, request_ref_for_spawn).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_storage::{
        PipelineEventQuery, SqliteBodyStore, SqlitePipelineEventStore,
    };
    use tars_types::{ChatRequest, ModelHint, RequestContext};

    use crate::service::ProviderService;

    async fn drain(s: tars_provider::LlmEventStream) -> Vec<ChatEvent> {
        let mut s = s;
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn happy_path_emits_one_event_with_bodies() {
        let events: Arc<dyn PipelineEventStore> =
            SqlitePipelineEventStore::in_memory().unwrap();
        let bodies: Arc<dyn BodyStore> = SqliteBodyStore::in_memory().unwrap();

        let provider = MockProvider::new("p1", CannedResponse::text("hello"));
        let inner: Arc<dyn LlmService> = ProviderService::new(provider);
        let svc = EventEmitterMiddleware::new(events.clone(), bodies.clone()).wrap(inner);

        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        let _ = drain(svc.call(req, RequestContext::test_default()).await.unwrap()).await;

        // Allow the spawned write task to run.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stored = events.query(&PipelineEventQuery::default()).await.unwrap();
        assert_eq!(stored.len(), 1, "exactly one event emitted");
        match &stored[0] {
            PipelineEvent::LlmCallFinished(e) => {
                assert_eq!(e.actual_model, "m");
                assert!(matches!(e.result, CallResult::Ok));
                // Both bodies should resolve.
                let req_bytes = bodies.fetch(&e.request_ref).await.unwrap();
                assert!(req_bytes.is_some(), "request body fetchable");
                let resp_ref = e.response_ref.as_ref().expect("response_ref present on Ok");
                let resp_bytes = bodies.fetch(resp_ref).await.unwrap();
                assert!(resp_bytes.is_some(), "response body fetchable");
            }
            other => panic!("expected LlmCallFinished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validation_summary_propagates_into_event() {
        use crate::validation::{builtin::MaxLengthValidator, ValidationMiddleware};

        let events: Arc<dyn PipelineEventStore> =
            SqlitePipelineEventStore::in_memory().unwrap();
        let bodies: Arc<dyn BodyStore> = SqliteBodyStore::in_memory().unwrap();

        let provider = MockProvider::new("p1", CannedResponse::text("hello world"));
        let inner: Arc<dyn LlmService> = ProviderService::new(provider);
        // Onion: EventEmitter (outer) → Validation (inner) → Provider.
        let validated = ValidationMiddleware::new(vec![Box::new(
            MaxLengthValidator::truncate_above(5),
        )])
        .wrap(inner);
        let svc = EventEmitterMiddleware::new(events.clone(), bodies.clone()).wrap(validated);

        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        let _ = drain(svc.call(req, RequestContext::test_default()).await.unwrap()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stored = events.query(&PipelineEventQuery::default()).await.unwrap();
        assert_eq!(stored.len(), 1);
        match &stored[0] {
            PipelineEvent::LlmCallFinished(e) => {
                assert_eq!(e.validation_summary.validators_run, vec!["max_length"]);
            }
            other => panic!("expected LlmCallFinished, got {other:?}"),
        }
    }
}
