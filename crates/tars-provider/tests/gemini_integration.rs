//! Gemini provider integration tests against wiremock.

use futures::StreamExt;
use wiremock::matchers::{method, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_provider::auth::{Auth, basic};
use tars_provider::backends::gemini::GeminiProviderBuilder;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::provider::LlmProvider;
use tars_types::{ChatEvent, ChatRequest, ModelHint, RequestContext, StopReason};

/// Gemini SSE chunks are unnamed `data:` JSON objects, similar to OpenAI
/// shape, but no `[DONE]` terminator — server just closes the stream.
fn sse_body(events: &[&str]) -> String {
    let mut s = String::new();
    for ev in events {
        s.push_str("data: ");
        s.push_str(ev);
        s.push_str("\n\n");
    }
    s
}

#[tokio::test]
async fn streaming_text_response_decodes_to_events() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}],"modelVersion":"gemini-2.5-pro"}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":", world!"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":3}}"#,
    ]);

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(query_param("key", "gem-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new("gem_test", Auth::inline("gem-key"))
        .base_url(server.uri())
        .build(http, basic());

    let req = ChatRequest::user(ModelHint::Explicit("gemini-2.5-pro".into()), "say hi");
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
    assert_eq!(u.input_tokens, 4);
    assert_eq!(u.output_tokens, 3);
}

#[tokio::test]
async fn function_call_decodes_to_tool_call_events() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        // Gemini emits the functionCall as a single part with parsed args.
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"search","args":{"q":"rust"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":8},"modelVersion":"gemini-2.5-pro"}"#,
    ]);

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new("gem_test", Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user(ModelHint::Explicit("gemini-2.5-pro".into()), "search"),
            RequestContext::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(
        resp.tool_calls[0].arguments,
        serde_json::json!({"q": "rust"})
    );
}

#[tokio::test]
async fn safety_filter_block_returns_content_filtered_error() {
    let server = MockServer::start().await;

    // No `candidates` — only `promptFeedback`.
    let body = sse_body(&[r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#]);

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new("gem_test", Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    // Stream open succeeds; the error surfaces mid-stream as the body
    // contains the blocked-prompt notice.
    let mut stream = provider
        .stream(
            ChatRequest::user(ModelHint::Explicit("gemini-2.5-pro".into()), "blocked"),
            RequestContext::test_default(),
        )
        .await
        .expect("stream open");

    let mut saw_filtered = false;
    while let Some(ev) = stream.next().await {
        if let Err(tars_types::ProviderError::ContentFiltered { category }) = ev {
            assert_eq!(category, "SAFETY");
            saw_filtered = true;
            break;
        }
    }
    assert!(saw_filtered, "expected ContentFiltered mid-stream");
}

#[tokio::test]
async fn http_403_maps_to_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(r#"{"error":{"message":"bad key"}}"#),
        )
        .mount(&server)
        .await;

    let http = HttpProviderBase::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new("gem_test", Auth::inline("bad"))
        .base_url(server.uri())
        .build(http, basic());

    let err = match provider
        .stream(
            ChatRequest::user(ModelHint::Explicit("gemini-2.5-pro".into()), "hi"),
            RequestContext::test_default(),
        )
        .await
    {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };
    assert!(matches!(err, tars_types::ProviderError::Auth(_)));
}
