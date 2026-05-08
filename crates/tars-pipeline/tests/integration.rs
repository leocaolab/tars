//! Integration tests — wire `tars-config` → `tars-provider` → `tars-pipeline`
//! → wiremock-backed OpenAI server. The boundary intentionally sits at
//! the HTTP layer: we use the *real* `reqwest` client and *real* SSE
//! parser, so any wiring bug between layers shows up here even though
//! every constituent crate has its own unit tests.
//!
//! Approach: build SSE bodies inline (no fixture files to keep in
//! sync), let wiremock replay them, assert on the parsed `ChatEvent`
//! stream.
//!
//! See the sibling unit tests in `src/retry.rs` and `src/middleware.rs`
//! for trait-level fakes (`FailNTimes`, `TagLayer`); this file
//! deliberately avoids those — the point is to exercise the *real*
//! transport.

use std::sync::Arc;

use futures::StreamExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_config::ConfigManager;
use tars_pipeline::{Pipeline, RetryMiddleware, TelemetryMiddleware};
use tars_provider::auth::basic;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::registry::ProviderRegistry;
use tars_types::{
    ChatEvent, ChatRequest, ModelHint, ProviderId, RequestContext, StopReason,
};

/// OpenAI SSE shape: `data: <json>\n\n` per chunk, terminator `data: [DONE]\n\n`.
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

/// A minimal but realistic OpenAI streaming response: model+role chunk,
/// content chunks, finish_reason, then a usage-only chunk.
fn happy_path_body() -> String {
    sse_body(&[
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"gpt-4o","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}}"#,
    ])
}

/// Build a registry from a TOML snippet pointed at the wiremock URL.
/// Mirrors what the eventual `tars-cli` will do at startup.
fn registry_from_toml(toml_str: &str) -> ProviderRegistry {
    let cfg = ConfigManager::load_from_str(toml_str).expect("config parses");
    let http = HttpProviderBase::default_arc().expect("http base");
    ProviderRegistry::from_config(&cfg.providers, http, basic())
        .expect("registry builds")
}

// ── 1. Happy path ───────────────────────────────────────────────────────────
//
// Wiremock returns one full streaming response. The pipeline (Telemetry +
// Retry + ProviderService → real OpenAI HTTP adapter) parses it and we
// assert the final Finished event carries the usage from the SSE bytes
// (proves the parser ran inside the pipeline, not just on a fake stream).

#[tokio::test]
async fn happy_path_pipeline_parses_real_sse_into_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(happy_path_body()),
        )
        .mount(&server)
        .await;

    let toml = format!(
        r#"
        [providers.openai_test]
        type = "openai_compat"
        base_url = "{}"
        default_model = "gpt-4o"
        auth = {{ kind = "secret", secret = {{ source = "inline", value = "test-key" }} }}
        "#,
        server.uri(),
    );
    let registry = registry_from_toml(&toml);
    let provider = registry
        .get(&ProviderId::new("openai_test"))
        .expect("provider registered");

    let pipeline = Pipeline::builder(provider)
        .layer(TelemetryMiddleware::new())
        .layer(RetryMiddleware::no_backoff(3))
        .build();
    assert_eq!(pipeline.layer_names(), &["telemetry", "retry"]);

    let req = ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "say hi");
    let mut stream = Arc::new(pipeline)
        .call(req, RequestContext::test_default())
        .await
        .expect("stream open");

    let mut text = String::new();
    let mut finish: Option<(StopReason, tars_types::Usage)> = None;
    while let Some(ev) = stream.next().await {
        match ev.expect("event") {
            ChatEvent::Delta { text: t } => text.push_str(&t),
            ChatEvent::Finished { stop_reason, usage } => {
                finish = Some((stop_reason, usage));
            }
            _ => {}
        }
    }

    assert_eq!(text, "hello");
    let (stop, usage) = finish.expect("Finished event");
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 1);

    // Wiremock saw exactly one POST.
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected single upstream POST");
}

// ── 2. RetryMiddleware genuinely retries the HTTP request ───────────────────
//
// First POST: 503 → maps to ProviderError::ModelOverloaded (Retriable).
// Second POST: 200 + valid SSE.
// We assert wiremock saw 2 POSTs and the final stream completes cleanly.

#[tokio::test]
async fn retry_middleware_actually_replays_http_call_on_5xx() {
    let server = MockServer::start().await;

    // Higher-priority mock returns 503 for the first hit only.
    // (wiremock priorities: lower number wins; default is 5.)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("model overloaded"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;

    // Fallback mock returns 200 + SSE for everything else.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(happy_path_body()),
        )
        .with_priority(2)
        .mount(&server)
        .await;

    let toml = format!(
        r#"
        [providers.openai_test]
        type = "openai_compat"
        base_url = "{}"
        default_model = "gpt-4o"
        auth = {{ kind = "secret", secret = {{ source = "inline", value = "k" }} }}
        "#,
        server.uri(),
    );
    let registry = registry_from_toml(&toml);
    let provider = registry.get(&ProviderId::new("openai_test")).unwrap();

    // no_backoff so the test doesn't depend on real wall-clock sleeps.
    let pipeline = Pipeline::builder(provider)
        .layer(TelemetryMiddleware::new())
        .layer(RetryMiddleware::no_backoff(3))
        .build();

    let mut stream = Arc::new(pipeline)
        .call(
            ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "x"),
            RequestContext::test_default(),
        )
        .await
        .expect("retry recovers and opens stream");

    // Drain the stream — must complete without error.
    let mut got_finished = false;
    while let Some(ev) = stream.next().await {
        if matches!(ev.expect("event"), ChatEvent::Finished { .. }) {
            got_finished = true;
        }
    }
    assert!(got_finished, "expected terminal Finished event");

    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        2,
        "RetryMiddleware should have re-issued the POST after 503",
    );
}

// ── 3. Registry → Pipeline wiring (config-driven) ───────────────────────────
//
// Smaller smoke test: the only assertion is that the type chain
// (TOML → ProvidersConfig → ProviderRegistry → Arc<dyn LlmProvider>
// → Pipeline → Arc<dyn LlmService>) actually compiles + runs through
// to a successful pipeline.call(). Catches Arc/dyn/trait-object mismatches
// that compile in isolation but break when composed.

#[tokio::test]
async fn registry_built_from_toml_can_drive_pipeline_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(happy_path_body()),
        )
        .mount(&server)
        .await;

    // Two providers in the registry — pipeline should be able to
    // pick whichever one we ask for by id.
    let toml = format!(
        r#"
        [providers.unused_mock]
        type = "mock"
        canned_response = "should not be called"

        [providers.openai_under_test]
        type = "openai_compat"
        base_url = "{}"
        default_model = "gpt-4o"
        auth = {{ kind = "secret", secret = {{ source = "inline", value = "k" }} }}
        "#,
        server.uri(),
    );

    let registry = registry_from_toml(&toml);
    // The loader merges built-in defaults under user-declared
    // providers (8 builtins + 2 user-declared = 10). The exact number
    // is incidental — what matters is that both user entries resolve
    // and that builtins are also present.
    assert_eq!(registry.len(), 10);
    assert!(registry.get(&ProviderId::new("unused_mock")).is_some());
    assert!(registry.get(&ProviderId::new("openai_under_test")).is_some());
    assert!(registry.get(&ProviderId::new("mlx")).is_some());

    let provider = registry
        .get(&ProviderId::new("openai_under_test"))
        .expect("the OpenAI-compat provider is the one we wired");

    let pipeline = Pipeline::builder(provider)
        .layer(TelemetryMiddleware::new())
        .layer(RetryMiddleware::no_backoff(2))
        .build();

    // Round-trip a single request — proves the whole Arc/dyn chain holds.
    let mut stream = Arc::new(pipeline)
        .call(
            ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "ping"),
            RequestContext::test_default(),
        )
        .await
        .expect("pipeline.call() returns Ok");
    let mut events = 0;
    while let Some(ev) = stream.next().await {
        ev.expect("stream event");
        events += 1;
    }
    assert!(events > 0, "pipeline yielded at least one event");

    // The unused mock provider must not have been touched — wiremock
    // got a hit, not the in-process mock.
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
}
