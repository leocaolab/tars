//! `ctx.deadline` is the caller's per-call wall-clock budget, and an HTTP
//! provider honors it as a per-request total timeout.
//!
//! This is the HTTP half of the CLI contract in
//! `backends::cli::streaming::tests`: the same parameter, enforced by whichever
//! leaf owns the resource. It is a *parameter*, not config — `HttpProviderConfig`
//! grows no total-timeout field, because how long a slow-but-progressing stream
//! may run is a property of the work, not of the environment.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tars_provider::backends::openai::OpenAiProviderBuilder;
use tars_provider::{Auth, HttpProviderBase, LlmProvider, basic};
use tars_types::{ChatRequest, RequestContext};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A server that accepts the connection and then stalls far longer than the
/// caller's budget. Without a deadline the client would wait it out (only the
/// SSE idle timeout, minutes long, would ever fire).
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
    let http = HttpProviderBase::default_arc().expect("http base");
    OpenAiProviderBuilder::new("openai_deadline", Auth::inline("k"))
        .base_url(uri)
        .build(http, basic())
}

#[tokio::test]
async fn deadline_bounds_a_stalled_http_request() {
    let server = mount_stalling_server(Duration::from_secs(30)).await;
    let provider = openai_at(&server.uri());

    let mut ctx = RequestContext::test_default();
    ctx.deadline = Some(Instant::now() + Duration::from_millis(300));

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        provider.stream(ChatRequest::user("x"), "gpt-4o", ctx),
    )
    .await
    .expect("the deadline must return control, not hang past it");

    // The abort is the caller's wall-clock deadline firing — reqwest's per-request
    // timeout — so it must surface as `ProviderError::TimedOut`, not a generic
    // Network blip. (The response headers never arrive within 300ms, so the
    // timeout fires at `.send()`.)
    let err = match result {
        Ok(_) => panic!("a stalled server past the deadline must not succeed"),
        Err(e) => e,
    };
    assert!(
        matches!(err, tars_types::ProviderError::TimedOut { .. }),
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

    // `remaining()` saturates to ZERO once the deadline has passed, so the
    // request must fail immediately rather than run a full call it has no
    // time for.
    let mut ctx = RequestContext::test_default();
    ctx.deadline = Some(Instant::now() - Duration::from_secs(1));

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        provider.stream(ChatRequest::user("x"), "gpt-4o", ctx),
    )
    .await
    .expect("an expired deadline must return immediately");

    assert!(result.is_err(), "an expired budget must not produce a response");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "expired deadline must fail fast, took {:?}",
        started.elapsed()
    );
}

/// No deadline ⇒ today's behaviour: only the transport bounds apply
/// (`connect_timeout` + `stream_idle_timeout`), so a 500ms stall is served, not
/// aborted. This pins that config grew no total-timeout knob.
#[tokio::test]
async fn no_deadline_leaves_the_request_unbounded_by_total_time() {
    let server = mount_stalling_server(Duration::from_millis(500)).await;
    let provider = openai_at(&server.uri());

    let ctx = RequestContext::test_default();
    assert!(ctx.deadline.is_none(), "precondition: no caller deadline");

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        provider.stream(ChatRequest::user("x"), "gpt-4o", ctx),
    )
    .await
    .expect("a 500ms stall must be served, not timed out");
    // The body is an empty 200 (no SSE frames) — the point is only that the
    // 500ms delay was *waited out* rather than cut short by a total timeout.
    assert!(result.is_ok(), "no deadline ⇒ the stall is waited out");
}
