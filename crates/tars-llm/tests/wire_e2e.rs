//! Checks that a call through the emitter, retry and a real HTTP transport writes one `llm_call`
//! row and one `llm_wire` row per attempt, all under the call's one id
//! (`docs/design/melt-parquet.md`, E2E-1, E2E-2 and E2E-13).

use std::sync::Arc;
use std::time::Duration;
use tars_types::ProviderId;

use futures::StreamExt;
use tokio::runtime::Handle;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::anthropic::AnthropicProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_llm::provider::wire::WireRecorder;
use tars_llm::{EventEmitterMiddleware, LlmService, RetryConfig, RetryMiddleware};
use tars_melt::event::call_log::{CALL_LOG_SINK, CallBody, CallOutcome, LlmCallRow, LlmCallTable};
use tars_melt::event::log::{LogSink, LogSource, MemLog, TenantScope, TimeWindow};
use tars_melt::event::wire::{Transport, WIRE_LOG_SINK, WireOutcome, WireRow, WireTable, attempts};
use tars_types::{Caller, ChatRequest};

const SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":3}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

const OVERLOADED: &str =
    r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

/// The call log and the wire log of one directory, in memory.
struct Logs {
    calls: MemLog<LlmCallTable>,
    calls_sink: Arc<dyn LogSink<LlmCallTable>>,
    wire: MemLog<WireTable>,
    wire_sink: Arc<dyn LogSink<WireTable>>,
}

impl Logs {
    fn new() -> Self {
        let calls = MemLog::<LlmCallTable>::new();
        let wire = MemLog::<WireTable>::new();
        Self {
            calls_sink: Arc::new(calls.sink(CALL_LOG_SINK, &Handle::current())),
            wire_sink: Arc::new(wire.sink(WIRE_LOG_SINK, &Handle::current())),
            calls,
            wire,
        }
    }

    /// Builds the emitter outermost, then retry, then an Anthropic transport recording its
    /// attempts.
    fn service(&self, server: &MockServer) -> LlmService {
        let provider = AnthropicProviderBuilder::new(ProviderId::new("ant"), Auth::inline("k"))
            .base_url(server.uri())
            .wire(Some(WireRecorder::new(self.wire_sink.clone())))
            .build(HttpClient::default_arc().unwrap(), basic());
        LlmService::builder(provider, "claude-x")
            .layer(EventEmitterMiddleware::new(self.calls_sink.clone()))
            .layer(RetryMiddleware::new(RetryConfig {
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(1),
                ..RetryConfig::default()
            }))
            .build()
    }

    /// Seals both logs and returns the one call written and its attempts.
    async fn read(&self) -> (LlmCallRow, Vec<WireRow>) {
        self.calls_sink.close().await.unwrap();
        self.wire_sink.close().await.unwrap();
        let tenant = Caller::test_default().tenant_id;
        let mut calls = self
            .calls
            .scan(&TenantScope::new(tenant.clone(), TimeWindow::all()))
            .await
            .unwrap();
        assert_eq!(calls.len(), 1, "one call, one row");
        let call = calls.remove(0);
        let wire = attempts(&self.wire, &tenant, call.call_id).await.unwrap();
        (call, wire)
    }
}

fn request() -> ChatRequest {
    let mut req = ChatRequest::user("say hi");
    // Set, so the service fills nothing in and the row is the request as made.
    req.max_output_tokens = Some(64);
    req
}

#[tokio::test]
async fn a_call_logs_its_request_and_the_wire_bytes_the_server_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE))
        .mount(&server)
        .await;
    let logs = Logs::new();
    let req = request();

    let mut stream = logs
        .service(&server)
        .call(req.clone(), Caller::test_default())
        .await
        .result
        .unwrap();
    while let Some(ev) = stream.next().await {
        ev.unwrap();
    }
    drop(stream);

    let (call, wire) = logs.read().await;
    let CallBody::Present(body) = &call.request_body else {
        panic!("a live call keeps its request body");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        serde_json::to_value(&req).unwrap()
    );
    assert_eq!(wire.len(), 1);
    assert_eq!(wire[0].call_id, call.call_id);
    assert_eq!(wire[0].response_bytes.as_ref(), SSE.as_bytes());
    let sent = &server.received_requests().await.unwrap()[0];
    assert_eq!(wire[0].request_bytes.as_ref(), sent.body.as_slice());
    assert_eq!(wire[0].outcome, WireOutcome::Completed);
}

/// One retry is two attempts under one call id; the failed one keeps
/// the server's error body and the error as raised, and the attempts number
/// the telemetry's retries plus one.
#[tokio::test]
async fn a_retried_call_logs_each_attempt_under_its_one_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_string(OVERLOADED))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE))
        .with_priority(2)
        .mount(&server)
        .await;
    let logs = Logs::new();
    let ctx = Caller::test_default();

    let mut stream = logs
        .service(&server)
        .call(request(), ctx.clone())
        .await
        .result
        .unwrap();
    let mut measured = tars_types::CallTelemetry::default();
    while let Some(ev) = stream.next().await {
        if let tars_types::ChatEvent::Measured { telemetry } = ev.unwrap() {
            measured = measured.merge(telemetry);
        }
    }
    drop(stream);

    let (call, wire) = logs.read().await;
    let retries = measured.retry_count;
    assert_eq!(retries, 1);
    assert_eq!(wire.len() as u32, retries + 1);
    assert!(wire.iter().all(|w| w.call_id == call.call_id));
    assert_eq!(
        wire.iter().map(|w| w.attempt).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(wire[0].transport, Transport::HttpSse { status: Some(529) });
    assert_eq!(wire[0].response_bytes.as_ref(), OVERLOADED.as_bytes());
    let WireOutcome::Failed { error } = &wire[0].outcome else {
        panic!("the first attempt failed: {:?}", wire[0].outcome);
    };
    // The server's own words are the response bytes above; this is the error as raised.
    assert_eq!(error, "model overloaded", "the error as raised");
    assert_eq!(wire[1].outcome, WireOutcome::Completed);
    assert_eq!(call.outcome(), CallOutcome::Completed);
}

/// A caller that drops the stream halfway leaves a cancelled call
/// row and a cancelled wire row, under the same id.
#[tokio::test]
async fn a_dropped_call_leaves_a_cancelled_call_row_and_its_cancelled_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE))
        .mount(&server)
        .await;
    let logs = Logs::new();

    let mut stream = logs
        .service(&server)
        .call(request(), Caller::test_default())
        .await
        .result
        .unwrap();
    stream.next().await.unwrap().unwrap();
    drop(stream);

    let (call, wire) = logs.read().await;
    assert_eq!(call.outcome(), CallOutcome::Cancelled);
    assert_eq!(wire.len(), 1);
    assert_eq!(wire[0].call_id, call.call_id);
    assert_eq!(wire[0].outcome, WireOutcome::Cancelled);
}

/// A caller that stops reading at `Finished` dropped a call that ended: the
/// call row and the wire row both say completed.
#[tokio::test]
async fn a_call_dropped_after_finished_is_completed_on_both_rows() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE))
        .mount(&server)
        .await;
    let logs = Logs::new();

    let mut stream = logs
        .service(&server)
        .call(request(), Caller::test_default())
        .await
        .result
        .unwrap();
    while let Some(ev) = stream.next().await {
        if matches!(ev.unwrap(), tars_types::ChatEvent::Finished { .. }) {
            break;
        }
    }
    drop(stream);

    let (call, wire) = logs.read().await;
    assert_eq!(call.outcome(), CallOutcome::Completed);
    assert_eq!(wire.len(), 1);
    assert_eq!(wire[0].call_id, call.call_id);
    assert_eq!(wire[0].outcome, WireOutcome::Completed);
}
