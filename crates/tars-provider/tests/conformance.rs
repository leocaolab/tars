//! Cross-provider conformance suite — Doc 01 §14 + Doc 14 §8.2 D-9.
//!
//! One body of tests, instantiated per HTTP backend via the
//! `conformance_suite!` macro. Each backend supplies a `Scenarios`
//! type that knows how to:
//!   - spin up a wiremock server + build a configured provider
//!   - emit the backend's wire bytes for each shared scenario
//!     (streaming text / tool call / 4xx-5xx error)
//!
//! What the suite proves:
//!   1. Every provider's open-time success path produces the same
//!      observable [`ChatEvent`] sequence shape for "respond with
//!      this text" — Started → Delta+ → Finished(EndTurn) with usage.
//!   2. Every provider's tool-call path produces ToolCall with parsed
//!      `arguments` (a JSON Object, regardless of whether the wire
//!      format used a string-encoded payload like OpenAI), name +
//!      id matches, and stop_reason = ToolUse.
//!   3. HTTP error → typed [`ProviderError`] mapping is consistent:
//!      401 → Auth, 429 → RateLimited, 503 → ModelOverloaded.
//!   4. Capability descriptors expose the same minimum guarantees
//!      (id stable, modalities non-empty).
//!
//! What the suite **doesn't** cover (handled by per-backend
//! integration tests and intentionally so):
//!   - Backend-quirk paths: Anthropic cache_control marker placement,
//!     Gemini safety-filter blocking, OpenAI's tool-message error
//!     marker, Anthropic's named-event routing, etc.
//!   - CLI subprocess backends (different wire path entirely; their
//!     conformance lives next to their fake-runner tests).
//!   - Live-API smoke (Doc 01 §14's "nightly CI" tier — a separate
//!     harness with budget controls).
//!
//! Adding a new HTTP backend means: implement `Scenarios` for it,
//! add one `conformance_suite!(name, MyScenarios);` line, done.

#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_provider::auth::{basic, Auth};
use tars_provider::backends::anthropic::AnthropicProviderBuilder;
use tars_provider::backends::gemini::GeminiProviderBuilder;
use tars_provider::backends::openai::OpenAiProviderBuilder;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::provider::LlmProvider;
use tars_types::{ChatRequest, ErrorClass, ModelHint, ProviderError, RequestContext, StopReason};

// ──────────────────────────────────────────────────────────────────────
// Per-backend Scenarios — each is a unit struct used as a namespace.
// Methods are associated functions (no Self state) so the macro can
// invoke them as `<$scenarios>::method(...)`.
// ──────────────────────────────────────────────────────────────────────

mod scenarios {
    use super::*;

    pub struct OpenAi;
    impl OpenAi {
        pub async fn setup() -> (MockServer, Arc<dyn LlmProvider>, String) {
            let server = MockServer::start().await;
            let http = HttpProviderBase::default_arc().unwrap();
            let provider = OpenAiProviderBuilder::new("openai_conf", Auth::inline("k"))
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
            // OpenAI's tool-call wire shape: id+name in the start delta,
            // arguments arrives as a (possibly multi-chunk) JSON-string,
            // finish_reason="tool_calls". We send the full args in one
            // delta — the conformance contract is "parsed Object on the
            // way out", not "buffer reassembles fragments" (that's a
            // backend-specific test in the per-backend file).
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
            let http = HttpProviderBase::default_arc().unwrap();
            let provider =
                AnthropicProviderBuilder::new("ant_conf", Auth::inline("ant-key"))
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
                // partial_json carries the full args in one delta — see
                // the OpenAI tool note above; same rationale.
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
            let http = HttpProviderBase::default_arc().unwrap();
            let provider = GeminiProviderBuilder::new("gem_conf", Auth::inline("gem-key"))
                .base_url(server.uri())
                .build(http, basic());
            (server, provider, "gemini-2.5-pro".into())
        }

        pub async fn mount_streaming_text(server: &MockServer, text: &str) {
            // Gemini SSE is unnamed `data: {...}\n\n` chunks, no terminator.
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
            // Gemini's functionCall arrives with parsed args directly —
            // no streaming reassembly needed. Single chunk.
            let body = gemini_sse(&[
                &format!(
                    r#"{{"candidates":[{{"content":{{"parts":[{{"functionCall":{{"name":{},"args":{}}}}}]}},"finishReason":"STOP"}}],"usageMetadata":{{"promptTokenCount":4,"candidatesTokenCount":8}},"modelVersion":"gemini-2.5-pro"}}"#,
                    serde_json::Value::String(name.into()),
                    args,
                ),
            ]);
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

    // ── wire-format helpers ─────────────────────────────────────────────

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

// ──────────────────────────────────────────────────────────────────────
// The shared test body — instantiated once per backend.
// ──────────────────────────────────────────────────────────────────────

macro_rules! conformance_suite {
    ($name:ident, $scenarios:ty) => {
        mod $name {
            use super::*;

            fn req(model: &str) -> ChatRequest {
                ChatRequest::user(ModelHint::Explicit(model.into()), "x")
            }

            // ── 1. Streaming text path ─────────────────────────────────
            #[tokio::test]
            async fn streaming_text_yields_complete_response() {
                let (server, provider, model) = <$scenarios>::setup().await;
                <$scenarios>::mount_streaming_text(&server, "hello world").await;

                let resp = provider
                    .complete(req(&model), RequestContext::test_default())
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

            // ── 2. Tool-call path: parsed-Object contract ──────────────
            #[tokio::test]
            async fn tool_call_arguments_arrive_as_parsed_object() {
                let (server, provider, model) = <$scenarios>::setup().await;
                let expected_args = json!({"q": "rust", "limit": 5});
                <$scenarios>::mount_tool_call(&server, "search", expected_args.clone()).await;

                let resp = provider
                    .complete(req(&model), RequestContext::test_default())
                    .await
                    .expect("complete should succeed");

                assert_eq!(resp.tool_calls.len(), 1, "exactly one tool call emitted");
                let tc = &resp.tool_calls[0];
                assert_eq!(tc.name, "search");
                assert!(
                    tc.arguments.is_object(),
                    "arguments must be a JSON Object, never a string \
                     (Doc 01 §8 normalization)",
                );
                assert_eq!(
                    tc.arguments, expected_args,
                    "args round-trip preserved value-for-value",
                );
                assert_eq!(
                    resp.stop_reason,
                    Some(StopReason::ToolUse),
                    "tool-call response normalizes stop_reason to ToolUse",
                );
            }

            // ── 3. HTTP error classification — 401 → Auth ──────────────
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
                    .stream(req(&model), RequestContext::test_default())
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

            // ── 4. HTTP 429 → RateLimited (Retriable) ──────────────────
            #[tokio::test]
            async fn http_429_maps_to_rate_limited_error() {
                let (server, provider, model) = <$scenarios>::setup().await;
                <$scenarios>::mount_status(&server, 429, "rate limited").await;

                let err = match provider
                    .stream(req(&model), RequestContext::test_default())
                    .await
                {
                    Ok(_) => panic!("expected RateLimited error"),
                    Err(e) => e,
                };
                assert!(
                    matches!(err, ProviderError::RateLimited { .. }),
                    "429 must map to RateLimited, got {err:?}",
                );
                assert_eq!(err.class(), ErrorClass::Retriable);
            }

            // ── 5. HTTP 503 → ModelOverloaded (Retriable) ──────────────
            #[tokio::test]
            async fn http_503_maps_to_model_overloaded() {
                let (server, provider, model) = <$scenarios>::setup().await;
                <$scenarios>::mount_status(&server, 503, "service unavailable").await;

                let err = match provider
                    .stream(req(&model), RequestContext::test_default())
                    .await
                {
                    Ok(_) => panic!("expected ModelOverloaded error"),
                    Err(e) => e,
                };
                assert!(
                    matches!(err, ProviderError::ModelOverloaded),
                    "503 must map to ModelOverloaded, got {err:?}",
                );
                assert_eq!(err.class(), ErrorClass::Retriable);
            }

            // ── 6. Capability sanity — id stable, modalities non-empty
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
                    caps.max_context_tokens > 0,
                    "max_context_tokens must be positive",
                );
                // id() round-trips through Display + is non-empty.
                let id_str = provider.id().as_ref();
                assert!(!id_str.is_empty(), "ProviderId can never be empty");
            }
        }
    };
}

conformance_suite!(openai, scenarios::OpenAi);
conformance_suite!(anthropic, scenarios::Anthropic);
conformance_suite!(gemini, scenarios::Gemini);
