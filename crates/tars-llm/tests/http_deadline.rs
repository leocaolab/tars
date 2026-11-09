//! Checks that an HTTP provider ends a call at the caller's `ctx.deadline`. The deadline is per
//! call, not config: how long a slow stream that still makes progress may run depends on the
//! work, not the environment.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tars_types::ProviderId;

use tars_llm::provider::backends::openai::OpenAiProviderBuilder;
use tars_llm::{Auth, HttpClient, LlmProvider, basic};
use tars_types::{Caller, ChatRequest};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Starts a server that delays its answer by `delay`.
async fn mount_stalling_server(delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(delay))
        .mount(&server)
        .await;
    server
}

fn openai_at(uri: &str) -> Arc<dyn LlmProvider> {
    let http = HttpClient::default_arc().expect("http base");
    OpenAiProviderBuilder::new(ProviderId::new("openai_deadline"), Auth::inline("k"))
        .base_url(uri)
        .build(http, basic())
}

#[tokio::test]
async fn deadline_bounds_a_stalled_http_request() {
    let server = mount_stalling_server(Duration::from_secs(30)).await;
    let provider = openai_at(&server.uri());

    let mut ctx = Caller::test_default();
    ctx.deadline = Some(Instant::now() + Duration::from_millis(300));

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        provider.stream(ChatRequest::user("x"), "gpt-4o", ctx),
    )
    .await
    .expect("the deadline must return control, not hang past it");

    // The headers do not arrive within the deadline, so the timeout fires at `send`.
    let err = match result {
        Ok(_) => panic!("a stalled server past the deadline must not succeed"),
        Err(e) => e,
    };
    assert!(
        matches!(err, tars_llm::ProviderError::TimedOut { .. }),
        "expected ProviderError::TimedOut, got: {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must abort near the 300ms budget, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_already_expired_deadline_does_not_run_the_call() {
    let server = mount_stalling_server(Duration::from_secs(30)).await;
    let provider = openai_at(&server.uri());

    let mut ctx = Caller::test_default();
    ctx.deadline = Some(Instant::now() - Duration::from_secs(1));

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        provider.stream(ChatRequest::user("x"), "gpt-4o", ctx),
    )
    .await
    .expect("an expired deadline must return immediately");

    assert!(
        result.is_err(),
        "an expired budget must not produce a response"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "expired deadline must fail fast, took {:?}",
        started.elapsed()
    );
}

// Without a deadline only the connect and idle timeouts apply.
#[tokio::test]
async fn no_deadline_leaves_the_request_unbounded_by_total_time() {
    let server = mount_stalling_server(Duration::from_millis(500)).await;
    let provider = openai_at(&server.uri());

    let ctx = Caller::test_default();
    assert!(ctx.deadline.is_none(), "precondition: no caller deadline");

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        provider.stream(ChatRequest::user("x"), "gpt-4o", ctx),
    )
    .await
    .expect("a 500ms stall must be served, not timed out");
    assert!(result.is_ok(), "no deadline ⇒ the stall is waited out");
}
