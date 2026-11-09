//! Checks that the Gemini backend offers a batch submitter whose every method refuses with
//! `InvalidRequest`, since tars implements neither Gemini's long-running batch operations nor
//! Vertex AI batch prediction.

use std::sync::Arc;
use tars_types::ProviderId;

use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::gemini::GeminiProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_llm::{Feature, ProviderError, RequestRefusal};
use tars_types::{BatchItemId, BatchJobId, ChatRequest};

fn build_provider() -> Arc<dyn LlmProvider> {
    let http = HttpClient::default_arc().unwrap();
    GeminiProviderBuilder::new(
        ProviderId::new("gemini_test"),
        Auth::inline("AIza_test_key"),
    )
    .build(http, basic())
}

fn assert_not_implemented(err: ProviderError) {
    match err {
        ProviderError::InvalidRequest(RequestRefusal::Unsupported(Feature::Batch)) => {}
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn as_batch_submitter_returns_some_for_uniform_surface() {
    let provider = build_provider();
    assert!(
        provider.as_batch_submitter().is_some(),
        "Gemini exposes the surface so callers pattern-match uniformly",
    );
}

#[tokio::test]
async fn submit_returns_not_implemented_invalid_request() {
    let provider = build_provider();
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .submit(
            vec![(BatchItemId::new("x"), ChatRequest::user("hi"))],
            "test-model",
            &tars_types::Caller::test_default(),
        )
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}

#[tokio::test]
async fn status_returns_not_implemented_invalid_request() {
    let provider = build_provider();
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .status(
            &BatchJobId::new("ignored"),
            &tars_types::Caller::test_default(),
        )
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}

#[tokio::test]
async fn results_returns_not_implemented_invalid_request() {
    let provider = build_provider();
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .results(
            &BatchJobId::new("ignored"),
            &tars_types::Caller::test_default(),
        )
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}

#[tokio::test]
async fn cancel_returns_not_implemented_invalid_request() {
    let provider = build_provider();
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .cancel(
            &BatchJobId::new("ignored"),
            &tars_types::Caller::test_default(),
        )
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}
