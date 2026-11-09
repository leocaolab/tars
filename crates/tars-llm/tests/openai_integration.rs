//! Tests the OpenAI provider against a wiremock server.

use futures::StreamExt;
use tars_types::ProviderId;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::openai::OpenAiProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_types::{Caller, ChatEvent, ChatRequest, StopReason};

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
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hello, "},"finish_reason":null}]}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"world!"},"finish_reason":null}]}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
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

    let http = HttpClient::default_arc().unwrap();
    let provider =
        OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("test-key"))
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
    assert_eq!(u.unwrap().input_tokens, 12);
    assert_eq!(u.unwrap().output_tokens, 3);
}

// DeepSeek's reasoner streams its reasoning in `delta.reasoning_content`, apart from
// `delta.content`; it must come out as `ThinkingDelta`.
#[tokio::test]
async fn deepseek_reasoning_content_surfaces_as_thinking_delta() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"id":"ds1","object":"chat.completion.chunk","model":"deepseek-v4-pro","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Let me think: "},"finish_reason":null}]}"#,
        r#"{"id":"ds1","object":"chat.completion.chunk","model":"deepseek-v4-pro","choices":[{"index":0,"delta":{"reasoning_content":"2+2 is 4."},"finish_reason":null}]}"#,
        r#"{"id":"ds1","object":"chat.completion.chunk","model":"deepseek-v4-pro","choices":[{"index":0,"delta":{"content":"The answer is 4."},"finish_reason":null}]}"#,
        r#"{"id":"ds1","object":"chat.completion.chunk","model":"deepseek-v4-pro","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
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

    let http = HttpClient::default_arc().unwrap();
    let provider =
        OpenAiProviderBuilder::new(ProviderId::new("deepseek"), Auth::inline("test-key"))
            .base_url(server.uri())
            .build(http, basic());

    let req = ChatRequest::user("what is 2+2?");
    let mut stream = provider
        .stream(req, "test-model", Caller::test_default())
        .await
        .expect("stream open");

    let mut thinking = String::new();
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev.expect("event") {
            ChatEvent::ThinkingDelta { text: t } => thinking.push_str(&t),
            ChatEvent::Delta { text: t } => text.push_str(&t),
            _ => {}
        }
    }

    assert_eq!(thinking, "Let me think: 2+2 is 4.");
    assert_eq!(text, "The answer is 4.");
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

    let http = HttpClient::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());

    let resp = provider
        .complete(
            ChatRequest::user("hi"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .expect("complete ok");

    assert_eq!(resp.text, "foobar");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
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

    let http = HttpClient::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("bad"))
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
        Ok(_) => panic!("expected error, got Ok stream"),
        Err(e) => e,
    };

    assert!(matches!(err, tars_llm::ProviderError::Auth(_)));
}

#[tokio::test]
async fn http_429_maps_to_rate_limited() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let http = HttpClient::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("k"))
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
        Ok(_) => panic!("expected error, got Ok stream"),
        Err(e) => e,
    };

    assert!(matches!(err, tars_llm::ProviderError::RateLimited { .. }));
}

#[tokio::test]
async fn streaming_tool_call_emits_start_delta_end() {
    let server = MockServer::start().await;

    // The id and name come in the first delta, the arguments in pieces after it.
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

    let http = HttpClient::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("k"))
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
    assert_eq!(resp.tool_calls[0].id, "call_abc");
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(
        serde_json::Value::Object(resp.tool_calls[0].arguments.clone()),
        serde_json::json!({"q": "rust"})
    );
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn mock_provider_satisfies_trait() {
    use tars_llm::provider::backends::mock::{CannedResponse, MockProvider};

    let p = MockProvider::new(ProviderId::new("mock_t"), CannedResponse::text("ok"));
    let r = p
        .clone()
        .complete(
            ChatRequest::user("ping"),
            "test-model",
            Caller::test_default(),
        )
        .await
        .unwrap();
    assert_eq!(r.text, "ok");
    assert_eq!(p.call_count(), 1);
}

#[tokio::test]
async fn double_encoded_tool_arguments_fail_the_call_instead_of_vanishing() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"search","arguments":""}}]},"finish_reason":null}]}"#,
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"{\\\"q\\\":\\\"rust\\\"}\""}}]},"finish_reason":null}]}"#,
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
    let http = HttpClient::default_arc().unwrap();
    let provider = OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("k"))
        .base_url(server.uri())
        .build(http, basic());
    let resp = provider
        .complete(
            ChatRequest::user("search rust"),
            "test-model",
            Caller::test_default(),
        )
        .await;
    match resp {
        Err(tars_llm::ProviderError::MalformedToolCall {
            raw,
            problem: tars_types::MalformedArguments::NotAnObject(tars_types::JsonKind::String),
            ..
        }) => assert!(raw.contains("rust"), "{raw}"),
        other => {
            panic!("expected the model's malformed call carrying the raw arguments, got {other:?}")
        }
    }
}

const TOOL_START_CHUNK: &str = r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"write_file","arguments":""}}]},"finish_reason":null}]}"#;
const TOOL_HALF_CHUNK: &str = r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.txt\",\"content\":\"lorem ip"}}]},"finish_reason":null}]}"#;

/// Returns what `complete` gives for a stream whose body is `body` as written.
async fn complete_over(body: String) -> Result<tars_types::ChatResponse, tars_llm::ProviderError> {
    let (_server, provider) = provider_over(body).await;
    provider
        .complete(ChatRequest::user("x"), "gpt-4o", Caller::test_default())
        .await
}

async fn events_over(body: String) -> Vec<ChatEvent> {
    let (_server, provider) = provider_over(body).await;
    let stream = provider
        .stream(ChatRequest::user("x"), "gpt-4o", Caller::test_default())
        .await
        .unwrap();
    stream.map(Result::unwrap).collect().await
}

/// Returns a provider served `body` by a mock server, which serves only while it is kept.
async fn provider_over(
    body: String,
) -> (
    MockServer,
    std::sync::Arc<tars_llm::provider::backends::openai::OpenAiProvider>,
) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let provider = OpenAiProviderBuilder::new(ProviderId::new("oa"), Auth::inline("k"))
        .base_url(server.uri())
        .build(HttpClient::default_arc().unwrap(), basic());
    (server, provider)
}

#[tokio::test]
async fn a_body_that_ends_after_tool_deltas_is_a_cut_stream_with_the_raw_arguments() {
    let body = format!("data: {TOOL_START_CHUNK}\n\ndata: {TOOL_HALF_CHUNK}\n\n");
    match complete_over(body).await {
        Err(tars_llm::ProviderError::StreamCut {
            open_call: Some(tars_llm::ReceivedToolCall { id, name, raw }),
        }) => {
            assert_eq!((id.as_str(), name.as_str()), ("call_abc", "write_file"));
            assert!(raw.ends_with("lorem ip"), "{raw}");
        }
        other => panic!("expected a cut stream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_body_that_ends_after_the_tool_start_chunk_is_a_cut_stream_with_no_arguments() {
    let body = format!("data: {TOOL_START_CHUNK}\n\n");
    match complete_over(body).await {
        Err(tars_llm::ProviderError::StreamCut {
            open_call: Some(tars_llm::ReceivedToolCall { id, raw, .. }),
        }) => {
            assert_eq!((id.as_str(), raw.as_str()), ("call_abc", ""));
        }
        other => panic!("expected a cut stream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_body_of_text_that_ends_before_done_is_a_cut_stream() {
    let body = "data: {\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n".to_string();
    assert!(matches!(
        complete_over(body).await,
        Err(tars_llm::ProviderError::StreamCut { open_call: None })
    ));
}

#[tokio::test]
async fn a_done_after_the_tool_start_chunk_is_a_call_without_arguments() {
    let body = format!("data: {TOOL_START_CHUNK}\n\ndata: [DONE]\n\n");
    let events = events_over(body).await;
    let ends: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::ToolCallEnd { parsed_args, .. } => Some(parsed_args),
            _ => None,
        })
        .collect();
    assert!(
        matches!(ends.as_slice(), [args] if args.is_empty()),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_usage_chunk_with_no_finish_reason_before_it_leaves_the_call_unfinished() {
    let body = sse_body(&[
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
        r#"{"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    ]);
    let resp = complete_over(body).await;
    assert!(
        matches!(resp, Err(tars_llm::ProviderError::Unfinished(_))),
        "{resp:?}"
    );
}

#[tokio::test]
async fn a_usage_chunk_after_a_finished_with_usage_does_not_replace_its_stop_reason() {
    let body = sse_body(&[
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        r#"{"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    ]);
    let resp = complete_over(body).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::MaxTokens, "{resp:?}");
}

#[tokio::test]
async fn a_done_without_finish_reason_leaves_a_half_written_call_incomplete() {
    let body = format!("data: {TOOL_START_CHUNK}\n\ndata: {TOOL_HALF_CHUNK}\n\ndata: [DONE]\n\n");
    assert!(matches!(
        complete_over(body).await,
        Err(tars_llm::ProviderError::IncompleteToolCall { .. })
    ));
}

#[tokio::test]
async fn a_usage_chunk_with_no_finish_reason_before_it_keeps_its_usage() {
    let body = sse_body(&[
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
        r#"{"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":500,"total_tokens":1500}}"#,
    ]);
    let events = events_over(body).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ChatEvent::Finished { .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            ChatEvent::UsageProgress { usage }
                if (usage.input_tokens, usage.output_tokens) == (1000, 500)
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_call_with_no_arguments_cut_at_length_is_a_truncation() {
    let body = sse_body(&[
        TOOL_START_CHUNK,
        r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
    ]);
    assert!(matches!(
        complete_over(body).await,
        Err(tars_llm::ProviderError::TruncatedToolCall { ref raw, .. }) if raw.is_empty()
    ));
}

#[tokio::test]
async fn a_braceless_prefix_where_the_body_ends_is_a_cut_stream_not_closed() {
    let start = r#"{"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"write_file","arguments":"\"path\":\"a.txt\""}}]},"finish_reason":null}]}"#;
    assert!(matches!(
        complete_over(format!("data: {start}\n\n")).await,
        Err(tars_llm::ProviderError::StreamCut { open_call: Some(tars_llm::ReceivedToolCall { ref raw, .. }) }) if raw == r#""path":"a.txt""#
    ));
}
