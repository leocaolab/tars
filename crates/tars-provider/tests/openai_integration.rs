//! End-to-end OpenAI provider tests against a wiremock-backed server.
//!
//! These exercise the streaming path: we serve a canned SSE stream
//! from wiremock and assert that our adapter decodes it into the right
//! sequence of [`ChatEvent`]s.

use futures::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_provider::auth::{Auth, basic};
use tars_provider::backends::openai::OpenAiProviderBuilder;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::provider::LlmProvider;
use tars_types::{ChatEvent, ChatRequest, ModelHint, RequestContext, StopReason};

/// Standard OpenAI SSE shape: zero or more `data:` chunks ending with `data: [DONE]`.
fn sse_body(events: &[&str]) -> String {
    let mut s = String::new();
    for ev in events {
        s.push_str("data: ");
        s.push_str(ev);
        s.push_str("\n\n");
    }
    s.push_str("data: [DONE]\n\n");
    s
}

#[tokio::test]
async fn streaming_text_response_decodes_to_events() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        // First chunk: model + role
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        // Content chunks
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hello, "},"finish_reason":null}]}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"world!"},"finish_reason":null}]}"#,
        // Final chunk with stop reason
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        // Usage chunk (per stream_options.include_usage)
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new("openai_test", Auth::inline("test-key"))
        .base_url(server.uri())
        .build(http, basic());

    let req = ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "say hi");
    let mut stream = provider
        .stream(req, RequestContext::test_default())
        .await
        .expect("stream open");

    let mut text = String::new();
    let mut saw_finish = None;
    let mut usage = None;
    while let Some(ev) = stream.next().await {
        match ev.expect("event") {
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

    assert_eq!(text, "Hello, world!");
    assert_eq!(saw_finish, Some(StopReason::EndTurn));
    let u = usage.expect("usage seen");
    assert_eq!(u.input_tokens, 12);
    assert_eq!(u.output_tokens, 3);
}

#[tokio::test]
async fn complete_aggregates_streaming_response() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"foo"},"finish_reason":null}]}"#,
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"bar"},"finish_reason":"stop"}]}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new("openai_test", Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "hi"),
            RequestContext::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.text, "foobar");
    assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
}

#[tokio::test]
async fn http_401_maps_to_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"invalid api key"}}"#),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new("openai_test", Auth::inline("bad"))
        .base_url(server.uri())
        .build(http, basic());

    let err = match provider
        .stream(
            ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "hi"),
            RequestContext::test_default(),
        )
        .await
    {
        Ok(_) => panic!("expected error, got Ok stream"),
        Err(e) => e,
    };

    assert!(matches!(err, tars_types::ProviderError::Auth(_)));
}

#[tokio::test]
async fn http_429_maps_to_rate_limited() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new("openai_test", Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let err = match provider
        .stream(
            ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "hi"),
            RequestContext::test_default(),
        )
        .await
    {
        Ok(_) => panic!("expected error, got Ok stream"),
        Err(e) => e,
    };

    assert!(matches!(err, tars_types::ProviderError::RateLimited { .. }));
}

#[tokio::test]
async fn streaming_tool_call_emits_start_delta_end() {
    let server = MockServer::start().await;

    // OpenAI streaming tool call shape: id+name in first delta, args
    // chunked across subsequent deltas, terminated by finish_reason.
    let body = sse_body(&[
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"search","arguments":""}}]},"finish_reason":null}]}"#,
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\""}}]},"finish_reason":null}]}"#,
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"rust\"}"}}]},"finish_reason":null}]}"#,
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new("openai_test", Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "search rust"),
            RequestContext::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].id, "call_abc");
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(
        resp.tool_calls[0].arguments,
        serde_json::json!({"q": "rust"})
    );
    assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
}

/// Verify Arc<MockProvider> wiring works the same way OpenAiProvider does.
/// This is the contract test the trait promises.
#[tokio::test]
async fn mock_provider_satisfies_trait() {
    use tars_provider::backends::mock::{CannedResponse, MockProvider};

    let p = MockProvider::new("mock_t", CannedResponse::text("ok"));
    let r = p
        .clone()
        .complete(
            ChatRequest::user(ModelHint::Explicit("m".into()), "ping"),
            RequestContext::test_default(),
        )
        .await
        .unwrap();
    assert_eq!(r.text, "ok");
    assert_eq!(p.call_count(), 1);
}
