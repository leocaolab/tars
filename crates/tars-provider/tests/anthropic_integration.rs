//! End-to-end Anthropic provider tests against a wiremock-backed server.
//!
//! Anthropic SSE has *named* events (event: message_start, etc.) — the
//! decoder routes on `raw.event`, so wiremock fixtures must emit
//! `event:` headers, not just `data:` like OpenAI.

use futures::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_provider::auth::{basic, Auth};
use tars_provider::backends::anthropic::AnthropicProviderBuilder;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::provider::LlmProvider;
use tars_types::{ChatEvent, ChatRequest, ModelHint, RequestContext, StopReason};

/// Build a named-event SSE body. Each item is `(event_name, json_data)`.
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

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new("ant_test", Auth::inline("ant-key"))
        .base_url(server.uri())
        .build(http, basic());

    let req = ChatRequest::user(ModelHint::Explicit("claude-opus-4-7".into()), "say hi");
    let mut stream = provider
        .stream(req, RequestContext::test_default())
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
                assert_eq!(actual_model, "claude-opus-4-7");
            }
            ChatEvent::Delta { text: t } => text.push_str(&t),
            ChatEvent::Finished { stop_reason, usage: u } => {
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
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 3);
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

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new("ant_test", Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user(ModelHint::Explicit("claude-opus-4-7".into()), "search rust"),
            RequestContext::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].id, "tu_abc");
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(
        resp.tool_calls[0].arguments,
        serde_json::json!({"q": "rust"})
    );
    assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
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

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new("ant_test", Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user(ModelHint::Explicit("claude-opus-4-7".into()), "think then answer"),
            RequestContext::test_default(),
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

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = AnthropicProviderBuilder::new("ant", Auth::inline("bad"))
        .base_url(server.uri())
        .build(http, basic());

    let err = match provider
        .stream(
            ChatRequest::user(ModelHint::Explicit("claude-opus-4-7".into()), "hi"),
            RequestContext::test_default(),
        )
        .await
    {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };
    assert!(matches!(err, tars_types::ProviderError::Auth(_)));
}
