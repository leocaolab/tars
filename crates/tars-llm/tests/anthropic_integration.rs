//! Tests the Anthropic provider against a wiremock server. The decoder routes on the SSE event
//! name, so every fixture carries `event:` lines, not just `data:`.

use futures::StreamExt;
use tars_types::ProviderId;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::anthropic::AnthropicProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_types::{Caller, ChatEvent, ChatRequest, StopReason};

fn sse_named(events: &[(&str, &str)]) -> String {
    let mut s = String::new();
    for (ev, data) in events {
        s.push_str("event: ");
        s.push_str(ev);
        s.push('\n');
        s.push_str("data: ");
        s.push_str(data);
        s.push_str("\n\n");
    }
    s
}

#[tokio::test]
async fn streaming_text_response_decodes_to_events() {
    let server = MockServer::start().await;

    let body = sse_named(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","model":"claude-opus-4-7","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello, "}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world!"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":3}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "ant-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider =
        AnthropicProviderBuilder::new(ProviderId::new("ant_test"), Auth::inline("ant-key"))
            .base_url(server.uri())
            .build(http, basic());

    let req = ChatRequest::user("say hi");
    let mut stream = provider
        .stream(req, "test-model", Caller::test_default())
        .await
        .expect("stream open");

    let mut text = String::new();
    let mut saw_started = false;
    let mut saw_finish = None;
    let mut usage = None;
    while let Some(ev) = stream.next().await {
        match ev.expect("event") {
            ChatEvent::Started { actual_model, .. } => {
                saw_started = true;
                assert_eq!(actual_model.as_deref(), Some("claude-opus-4-7"));
            }
            ChatEvent::Delta { text: t } => text.push_str(&t),
            ChatEvent::Finished {
                stop_reason,
                usage: u,
            } => {
                saw_finish = Some(stop_reason);
                usage = Some(u);
            }
            _ => {}
        }
    }

    assert!(saw_started);
    assert_eq!(text, "Hello, world!");
    assert_eq!(saw_finish, Some(StopReason::EndTurn));
    let u = usage.expect("usage seen");
    assert_eq!(u.unwrap().input_tokens, 10);
    assert_eq!(u.unwrap().output_tokens, 3);
}

#[tokio::test]
async fn streaming_tool_call_assembles_args_from_partial_json() {
    let server = MockServer::start().await;

    let body = sse_named(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-4-7","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_abc","name":"search","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"rust\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":5,"output_tokens":12}}"#,
        ),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new(ProviderId::new("ant_test"), Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user("search rust"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].id, "tu_abc");
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(
        serde_json::Value::Object(resp.tool_calls[0].arguments.clone()),
        serde_json::json!({"q": "rust"})
    );
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn thinking_blocks_decode_as_thinking_deltas() {
    let server = MockServer::start().await;

    let body = sse_named(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-4-7","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Answer"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":5,"output_tokens":10}}"#,
        ),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new(ProviderId::new("ant_test"), Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user("think then answer"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.thinking, "Let me think...");
    assert_eq!(resp.text, "Answer");
}

#[tokio::test]
async fn http_401_maps_to_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"bad key"}}"#),
        )
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new(ProviderId::new("ant"), Auth::inline("bad"))
        .base_url(server.uri())
        .build(http, basic());

    let err = match provider
        .stream(
            ChatRequest::user("hi"),
            "test-model",
            Caller::test_default(),
        )
        .await
    {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };
    assert!(matches!(err, tars_llm::ProviderError::Auth(_)));
}

const START: (&str, &str) = (
    "message_start",
    r#"{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","model":"claude-x","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
);
const TOOL_START: (&str, &str) = (
    "content_block_start",
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"write_file","input":{}}}"#,
);
const TOOL_HALF: (&str, &str) = (
    "content_block_delta",
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\",\"content\":\"lorem ip"}}"#,
);
const TOOL_STOP: (&str, &str) = (
    "content_block_stop",
    r#"{"type":"content_block_stop","index":0}"#,
);
const USAGE_ONLY_DELTA: (&str, &str) = (
    "message_delta",
    r#"{"type":"message_delta","delta":{},"usage":{"output_tokens":5}}"#,
);
const MAX_TOKENS_DELTA: (&str, &str) = (
    "message_delta",
    r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":6}}"#,
);
const MESSAGE_STOP: (&str, &str) = ("message_stop", r#"{"type":"message_stop"}"#);

/// Streams `events` from a mock Anthropic server and returns the events and the errors apart.
async fn stream_of(events: &[(&str, &str)]) -> (Vec<ChatEvent>, Vec<tars_llm::ProviderError>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_named(events)),
        )
        .mount(&server)
        .await;
    let provider = AnthropicProviderBuilder::new(ProviderId::new("ant"), Auth::inline("k"))
        .base_url(server.uri())
        .build(HttpClient::default_arc().unwrap(), basic());
    let mut stream = provider
        .stream(ChatRequest::user("x"), "claude-x", Caller::test_default())
        .await
        .expect("stream opens");
    let (mut ok, mut err) = (Vec::new(), Vec::new());
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(e) => ok.push(e),
            Err(e) => err.push(e),
        }
    }
    (ok, err)
}

fn has_finished(events: &[ChatEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ChatEvent::Finished { .. }))
}

#[tokio::test]
async fn a_half_written_call_at_max_tokens_is_truncated() {
    let (ok, err) = stream_of(&[
        START,
        TOOL_START,
        TOOL_HALF,
        TOOL_STOP,
        MAX_TOKENS_DELTA,
        MESSAGE_STOP,
    ])
    .await;
    assert!(!has_finished(&ok));
    assert!(matches!(
        err.as_slice(),
        [tars_llm::ProviderError::TruncatedToolCall { .. }]
    ));
}

#[tokio::test]
async fn a_usage_only_delta_leaves_the_held_call_for_the_stop_reason_after_it() {
    let (ok, err) = stream_of(&[
        START,
        TOOL_START,
        TOOL_HALF,
        TOOL_STOP,
        USAGE_ONLY_DELTA,
        MAX_TOKENS_DELTA,
        MESSAGE_STOP,
    ])
    .await;
    assert!(!has_finished(&ok), "{ok:?}");
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::TruncatedToolCall { raw, .. }] if raw.ends_with("lorem ip")
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_held_call_that_never_meets_a_stop_reason_is_incomplete_at_message_stop() {
    let (ok, err) = stream_of(&[
        START,
        TOOL_START,
        TOOL_HALF,
        TOOL_STOP,
        USAGE_ONLY_DELTA,
        MESSAGE_STOP,
    ])
    .await;
    assert!(!has_finished(&ok), "{ok:?}");
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::IncompleteToolCall { raw, .. }] if raw.ends_with("lorem ip")
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_body_that_ends_after_the_bad_block_is_a_cut_stream_with_the_raw_arguments() {
    let (ok, err) = stream_of(&[START, TOOL_START, TOOL_HALF, TOOL_STOP]).await;
    assert!(!has_finished(&ok), "{ok:?}");
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::StreamCut { open_call: Some(tars_llm::ReceivedToolCall { id, name, raw }) }]
                if id == "toolu_1" && name == "write_file" && raw.ends_with("lorem ip")
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_body_that_ends_with_only_text_before_message_stop_is_a_cut_stream() {
    let (ok, err) = stream_of(&[START]).await;
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::StreamCut { open_call: None }]
        ),
        "{err:?}"
    );
    assert!(!has_finished(&ok));
}

#[tokio::test]
async fn a_body_that_ends_right_after_the_tool_block_starts_is_a_cut_stream_with_no_arguments() {
    let (ok, err) = stream_of(&[START, TOOL_START]).await;
    assert!(
        !ok.iter()
            .any(|e| matches!(e, ChatEvent::ToolCallEnd { .. })),
        "{ok:?}"
    );
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::StreamCut { open_call: Some(tars_llm::ReceivedToolCall { id, raw, .. }) }]
                if id == "toolu_1" && raw.is_empty()
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_message_stop_after_no_stop_reason_ends_the_stream_without_finished() {
    let (ok, err) = stream_of(&[START, USAGE_ONLY_DELTA, MESSAGE_STOP]).await;
    assert!(err.is_empty(), "{err:?}");
    assert!(!has_finished(&ok), "{ok:?}");
}

#[tokio::test]
async fn a_message_stop_after_no_stop_reason_still_reports_the_usage() {
    let (ok, _) = stream_of(&[START, USAGE_ONLY_DELTA, MESSAGE_STOP]).await;
    assert!(
        ok.iter()
            .any(|ev| matches!(ev, ChatEvent::UsageProgress { usage } if usage.output_tokens > 0)),
        "{ok:?}"
    );
}

#[tokio::test]
async fn an_open_call_with_no_arguments_at_max_tokens_is_a_truncation() {
    let (ok, err) = stream_of(&[START, TOOL_START, MAX_TOKENS_DELTA, MESSAGE_STOP]).await;
    assert!(!has_finished(&ok), "{ok:?}");
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::TruncatedToolCall { raw, .. }] if raw.is_empty()
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_body_that_ends_inside_the_tool_block_is_a_cut_stream_with_the_raw_arguments() {
    let (ok, err) = stream_of(&[START, TOOL_START, TOOL_HALF]).await;
    assert!(!has_finished(&ok), "{ok:?}");
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::StreamCut { open_call: Some(tars_llm::ReceivedToolCall { id, name, raw }) }]
                if id == "toolu_1" && name == "write_file" && raw.ends_with("lorem ip")
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_stop_reason_before_the_block_closes_still_says_the_call_was_truncated() {
    const MAX_TOKENS_NO_USAGE: (&str, &str) = (
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
    );
    let (ok, err) = stream_of(&[
        START,
        TOOL_START,
        TOOL_HALF,
        MAX_TOKENS_NO_USAGE,
        TOOL_STOP,
        MESSAGE_STOP,
    ])
    .await;
    assert!(!has_finished(&ok), "{ok:?}");
    assert!(
        matches!(
            err.as_slice(),
            [tars_llm::ProviderError::TruncatedToolCall { raw, .. }] if raw.ends_with("lorem ip")
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_batch_error_body_is_read_only_up_to_the_cap() {
    let server = MockServer::start().await;
    let huge = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(4 << 20));
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string(huge))
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new(ProviderId::new("ant"), Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());
    let batch = provider.as_batch_submitter().expect("anthropic batches");
    let err = batch
        .submit(
            vec![(tars_types::BatchItemId::new("a"), ChatRequest::user("hi"))],
            "test-model",
            &Caller::test_default(),
        )
        .await
        .expect_err("a 500 fails the submit");
    let said = format!("{err:?}");
    assert!(
        said.len() < 64 * 1024,
        "the error holds {} bytes",
        said.len()
    );
}
