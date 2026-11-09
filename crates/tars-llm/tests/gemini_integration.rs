//! Tests the Gemini provider against a wiremock server.

use futures::StreamExt;
use tars_types::ProviderId;
use wiremock::matchers::{header, method, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::gemini::GeminiProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_types::{Caller, ChatEvent, ChatRequest, StopReason};

/// Returns the events as Gemini sends them: `data:` lines with no event names and no `[DONE]`.
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
        .and(header("x-goog-api-key", "gem-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new(ProviderId::new("gem_test"), Auth::inline("gem-key"))
        .base_url(server.uri())
        .build(http, basic());

    let req = ChatRequest::user("say hi");
    let mut stream = provider
        .stream(req, "test-model", Caller::test_default())
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

    let http = HttpClient::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new(ProviderId::new("gem_test"), Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user("search"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(
        serde_json::Value::Object(resp.tool_calls[0].arguments.clone()),
        serde_json::json!({"q": "rust"})
    );
}

#[tokio::test]
async fn safety_filter_block_returns_content_filtered_error() {
    let server = MockServer::start().await;

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

    let http = HttpClient::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new(ProviderId::new("gem_test"), Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    // The stream opens; the block arrives as an error inside it.
    let mut stream = provider
        .stream(
            ChatRequest::user("blocked"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .expect("stream open");

    let mut saw_filtered = false;
    while let Some(ev) = stream.next().await {
        if let Err(tars_llm::ProviderError::ContentFiltered { category }) = ev {
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

    let http = HttpClient::default_arc().unwrap();
    let provider = GeminiProviderBuilder::new(ProviderId::new("gem_test"), Auth::inline("bad"))
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

const SECRET_KEY: &str = "AIzaSEKRETKEY123";

#[tokio::test]
async fn a_failed_call_names_no_key_in_its_error() {
    // A port just released has no listener, so the request fails before any response.
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let http = HttpClient::default_arc().unwrap();
    let provider =
        GeminiProviderBuilder::new(ProviderId::new("gem_test"), Auth::inline(SECRET_KEY))
            .base_url(format!("http://{addr}"))
            .build(http, basic());

    let err = match provider
        .stream(
            ChatRequest::user("hi"),
            "test-model",
            Caller::test_default(),
        )
        .await
    {
        Ok(_) => panic!("expected a connection error"),
        Err(e) => e,
    };

    let chain = tars_utils::display_chain(&err);
    let debug = format!("{err:?}");
    assert!(chain.contains(&addr.to_string()), "{chain}");
    assert!(!chain.contains(SECRET_KEY), "{chain}");
    assert!(!debug.contains(SECRET_KEY), "{debug}");
}

#[tokio::test]
async fn the_key_rides_in_the_x_goog_api_key_header_and_not_in_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:streamGenerateContent"))
        .and(header("x-goog-api-key", SECRET_KEY))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(&[
                    r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,
                ])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider =
        GeminiProviderBuilder::new(ProviderId::new("gem_test"), Auth::inline(SECRET_KEY))
            .base_url(server.uri())
            .build(http, basic());
    let mut stream = provider
        .stream(
            ChatRequest::user("hi"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .expect("stream open");
    while let Some(ev) = stream.next().await {
        ev.expect("event");
    }

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(
        !received[0].url.as_str().contains(SECRET_KEY),
        "{}",
        received[0].url
    );
}

#[tokio::test]
async fn a_body_that_ends_before_the_finish_reason_is_a_cut_stream() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}],"modelVersion":"gemini-2.5-pro"}"#,
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
    let provider = GeminiProviderBuilder::new(ProviderId::new("gem_test"), Auth::inline("gem-key"))
        .base_url(server.uri())
        .build(HttpClient::default_arc().unwrap(), basic());

    let mut stream = provider
        .stream(
            ChatRequest::user("say hi"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .expect("stream open");
    let mut last = None;
    while let Some(ev) = stream.next().await {
        last = Some(ev);
    }
    assert!(
        matches!(
            last,
            Some(Err(tars_llm::ProviderError::StreamCut { open_call: None }))
        ),
        "{last:?}"
    );
}
