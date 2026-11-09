//! Runs one set of tests against every HTTP backend, each behind a wiremock server that sends
//! that backend's own wire bytes. A new backend needs a scenarios type and one
//! `conformance_suite!` line.

#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::ProviderError;
use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::anthropic::AnthropicProviderBuilder;
use tars_llm::provider::backends::gemini::GeminiProviderBuilder;
use tars_llm::provider::backends::openai::OpenAiProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_types::{Caller, ChatRequest, ErrorClass, StopReason};

mod scenarios {
    use super::*;
    use tars_types::ProviderId;

    pub struct OpenAi;
    impl OpenAi {
        pub async fn setup() -> (MockServer, Arc<dyn LlmProvider>, String) {
            let server = MockServer::start().await;
            let http = HttpClient::default_arc().unwrap();
            let provider =
                OpenAiProviderBuilder::new(ProviderId::new("openai_conf"), Auth::inline("k"))
                    .base_url(server.uri())
                    .build(http, basic());
            (server, provider, "gpt-4o".into())
        }

        pub async fn mount_streaming_text(server: &MockServer, text: &str) {
            let body = openai_sse(&[
                r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                &format!(
                    r#"{{"id":"c1","model":"gpt-4o","choices":[{{"index":0,"delta":{{"content":{}}},"finish_reason":null}}]}}"#,
                    serde_json::Value::String(text.into()),
                ),
                r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                r#"{"id":"c1","model":"gpt-4o","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#,
            ]);
            mount_sse_at(server, "/chat/completions", body).await;
        }

        pub async fn mount_tool_call(server: &MockServer, name: &str, args: Value) {
            // The arguments come in one delta: the shared contract is a parsed object out;
            // reassembling fragments is each backend's own test.
            let args_str = serde_json::to_string(&args).unwrap();
            let body = openai_sse(&[
                &format!(
                    r#"{{"model":"gpt-4o","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call_x","type":"function","function":{{"name":{},"arguments":""}}}}]}},"finish_reason":null}}]}}"#,
                    serde_json::Value::String(name.into()),
                ),
                &format!(
                    r#"{{"model":"gpt-4o","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":{}}}}}]}},"finish_reason":null}}]}}"#,
                    serde_json::Value::String(args_str),
                ),
                r#"{"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]);
            mount_sse_at(server, "/chat/completions", body).await;
        }

        pub async fn mount_status(server: &MockServer, status: u16, body: &str) {
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(server)
                .await;
        }
    }

    pub struct Anthropic;
    impl Anthropic {
        pub async fn setup() -> (MockServer, Arc<dyn LlmProvider>, String) {
            let server = MockServer::start().await;
            let http = HttpClient::default_arc().unwrap();
            let provider =
                AnthropicProviderBuilder::new(ProviderId::new("ant_conf"), Auth::inline("ant-key"))
                    .base_url(server.uri())
                    .build(http, basic());
            (server, provider, "claude-opus-4-7".into())
        }

        pub async fn mount_streaming_text(server: &MockServer, text: &str) {
            let body = anthropic_named_sse(&[
                ("message_start", r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-4-7","content":[],"usage":{"input_tokens":7,"output_tokens":1}}}"#.to_string()),
                ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string()),
                ("content_block_delta", format!(
                    r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":{}}}}}"#,
                    serde_json::Value::String(text.into()),
                )),
                ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#.to_string()),
                ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":7,"output_tokens":3}}"#.to_string()),
                ("message_stop", r#"{"type":"message_stop"}"#.to_string()),
            ]);
            mount_sse_at(server, "/v1/messages", body).await;
        }

        pub async fn mount_tool_call(server: &MockServer, name: &str, args: Value) {
            let args_str = serde_json::to_string(&args).unwrap();
            let body = anthropic_named_sse(&[
                ("message_start", r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-4-7","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#.to_string()),
                ("content_block_start", format!(
                    r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"tu_x","name":{},"input":{{}}}}}}"#,
                    serde_json::Value::String(name.into()),
                )),
                ("content_block_delta", format!(
                    r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
                    serde_json::Value::String(args_str),
                )),
                ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#.to_string()),
                ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":5,"output_tokens":12}}"#.to_string()),
            ]);
            mount_sse_at(server, "/v1/messages", body).await;
        }

        pub async fn mount_status(server: &MockServer, status: u16, body: &str) {
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(server)
                .await;
        }
    }

    pub struct Gemini;
    impl Gemini {
        pub async fn setup() -> (MockServer, Arc<dyn LlmProvider>, String) {
            let server = MockServer::start().await;
            let http = HttpClient::default_arc().unwrap();
            let provider =
                GeminiProviderBuilder::new(ProviderId::new("gem_conf"), Auth::inline("gem-key"))
                    .base_url(server.uri())
                    .build(http, basic());
            (server, provider, "gemini-2.5-pro".into())
        }

        pub async fn mount_streaming_text(server: &MockServer, text: &str) {
            // Gemini's stream has no event names and no terminator.
            let body = gemini_sse(&[
                &format!(
                    r#"{{"candidates":[{{"content":{{"parts":[{{"text":{}}}]}}}}],"modelVersion":"gemini-2.5-pro"}}"#,
                    serde_json::Value::String(text.into()),
                ),
                r#"{"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#,
            ]);
            mount_gemini_sse(server, body).await;
        }

        pub async fn mount_tool_call(server: &MockServer, name: &str, args: Value) {
            // Gemini sends a call's arguments already parsed, in one chunk.
            let body = gemini_sse(&[&format!(
                r#"{{"candidates":[{{"content":{{"parts":[{{"functionCall":{{"name":{},"args":{}}}}}]}},"finishReason":"STOP"}}],"usageMetadata":{{"promptTokenCount":4,"candidatesTokenCount":8}},"modelVersion":"gemini-2.5-pro"}}"#,
                serde_json::Value::String(name.into()),
                args,
            )]);
            mount_gemini_sse(server, body).await;
        }

        pub async fn mount_status(server: &MockServer, status: u16, body: &str) {
            Mock::given(method("POST"))
                .and(path_regex(r"/v1beta/models/.*:streamGenerateContent"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(server)
                .await;
        }
    }

    fn openai_sse(events: &[&str]) -> String {
        let mut s = String::new();
        for ev in events {
            s.push_str("data: ");
            s.push_str(ev);
            s.push_str("\n\n");
        }
        s.push_str("data: [DONE]\n\n");
        s
    }

    fn anthropic_named_sse(events: &[(&str, String)]) -> String {
        let mut s = String::new();
        for (name, data) in events {
            s.push_str("event: ");
            s.push_str(name);
            s.push('\n');
            s.push_str("data: ");
            s.push_str(data);
            s.push_str("\n\n");
        }
        s
    }

    fn gemini_sse(events: &[&str]) -> String {
        let mut s = String::new();
        for ev in events {
            s.push_str("data: ");
            s.push_str(ev);
            s.push_str("\n\n");
        }
        s
    }

    async fn mount_sse_at(server: &MockServer, path_str: &str, body: String) {
        Mock::given(method("POST"))
            .and(path(path_str))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(server)
            .await;
    }

    async fn mount_gemini_sse(server: &MockServer, body: String) {
        Mock::given(method("POST"))
            .and(path_regex(r"/v1beta/models/.*:streamGenerateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(server)
            .await;
    }
}

macro_rules! conformance_suite {
    ($name:ident, $scenarios:ty) => {
        mod $name {
            use super::*;

            fn req(_model: &str) -> ChatRequest {
                ChatRequest::user("x")
            }

            #[tokio::test]
            async fn streaming_text_yields_complete_response() {
                let (server, provider, model) = <$scenarios>::setup().await;
                <$scenarios>::mount_streaming_text(&server, "hello world").await;

                let resp = provider
                    .complete(req(&model), "test-model", Caller::test_default())
                    .await
                    .expect("complete should succeed");

                assert_eq!(resp.text, "hello world", "text content matches input");
                assert_eq!(
                    resp.stop_reason,
                    Some(StopReason::EndTurn),
                    "stop reason normalizes to EndTurn",
                );
                assert!(
                    resp.usage.input_tokens > 0 || resp.usage.output_tokens > 0,
                    "usage must be populated by the time the stream finishes",
                );
            }

            #[tokio::test]
            async fn tool_call_arguments_arrive_as_parsed_object() {
                let (server, provider, model) = <$scenarios>::setup().await;
                let expected_args = json!({"q": "rust", "limit": 5});
                <$scenarios>::mount_tool_call(&server, "search", expected_args.clone()).await;

                let resp = provider
                    .complete(req(&model), "test-model", Caller::test_default())
                    .await
                    .expect("complete should succeed");

                assert_eq!(resp.tool_calls.len(), 1, "exactly one tool call emitted");
                let tc = &resp.tool_calls[0];
                assert_eq!(tc.name, "search");
                assert_eq!(
                    serde_json::Value::Object(tc.arguments.clone()), expected_args,
                    "args round-trip preserved value-for-value",
                );
                assert_eq!(
                    resp.stop_reason,
                    Some(StopReason::ToolUse),
                    "tool-call response normalizes stop_reason to ToolUse",
                );
            }

            #[tokio::test]
            async fn http_401_maps_to_auth_error() {
                let (server, provider, model) = <$scenarios>::setup().await;
                <$scenarios>::mount_status(
                    &server,
                    401,
                    r#"{"error":{"message":"invalid credentials"}}"#,
                )
                .await;

                let err = match provider
                    .stream(req(&model), "test-model", Caller::test_default())
                    .await
                {
                    Ok(_) => panic!("expected Auth error"),
                    Err(e) => e,
                };
                assert!(
                    matches!(err, ProviderError::Auth(_)),
                    "401 must map to Auth, got {err:?}",
                );
                assert_eq!(err.class(), ErrorClass::Permanent);
            }

            #[tokio::test]
            async fn http_429_maps_to_rate_limited_error() {
                let (server, provider, model) = <$scenarios>::setup().await;
                <$scenarios>::mount_status(&server, 429, "rate limited").await;

                let err = match provider
                    .stream(req(&model), "test-model", Caller::test_default())
                    .await
                {
                    Ok(_) => panic!("expected RateLimited error"),
                    Err(e) => e,
                };
                assert!(
                    matches!(err, ProviderError::RateLimited { .. }),
                    "429 must map to RateLimited, got {err:?}",
                );
                assert_eq!(err.class(), ErrorClass::Transient);
            }

            #[tokio::test]
            async fn http_503_maps_to_model_overloaded() {
                let (server, provider, model) = <$scenarios>::setup().await;
                <$scenarios>::mount_status(&server, 503, "service unavailable").await;

                let err = match provider
                    .stream(req(&model), "test-model", Caller::test_default())
                    .await
                {
                    Ok(_) => panic!("expected ModelOverloaded error"),
                    Err(e) => e,
                };
                assert!(
                    matches!(err, ProviderError::ModelOverloaded),
                    "503 must map to ModelOverloaded, got {err:?}",
                );
                assert_eq!(err.class(), ErrorClass::Transient);
            }

            #[tokio::test]
            async fn capability_descriptor_is_minimally_valid() {
                let (_server, provider, _model) = <$scenarios>::setup().await;
                let caps = provider.capabilities();
                assert!(
                    !caps.modalities_in.is_empty(),
                    "modalities_in must be non-empty (post-67de40d invariant)",
                );
                assert!(
                    !caps.modalities_out.is_empty(),
                    "modalities_out must be non-empty",
                );
                assert!(
                    caps.max_context_tokens.is_none_or(|n| n > 0),
                    "max_context_tokens, when declared, must be positive",
                );
                let id_str = provider.id().as_ref();
                assert!(!id_str.is_empty(), "ProviderId can never be empty");
            }
        }
    };
}

conformance_suite!(openai, scenarios::OpenAi);
conformance_suite!(anthropic, scenarios::Anthropic);
conformance_suite!(gemini, scenarios::Gemini);
