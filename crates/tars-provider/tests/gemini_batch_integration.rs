//! Gemini batch surface tests.
//!
//! The Gemini backend exposes `as_batch_submitter()` returning Some,
//! but each method returns a typed `InvalidRequest` because the GenAI
//! API's batch endpoint (Long-Running Operations) and Vertex AI Batch
//! Prediction both require infrastructure we don't ship in V1. These
//! tests pin that contract so future work can't silently regress the
//! "broken-but-honest" surface.

use std::sync::Arc;

use tars_provider::auth::{Auth, basic};
use tars_provider::backends::gemini::GeminiProviderBuilder;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::provider::LlmProvider;
use tars_types::{
    BatchItemId, BatchJobId, ChatRequest, ModelHint, ProviderError,
};

fn build_provider() -> Arc<dyn LlmProvider> {
    let http = HttpProviderBase::default_arc().unwrap();
    GeminiProviderBuilder::new("gemini_test", Auth::inline("AIza_test_key"))
        .build(http, basic())
}

fn assert_not_implemented(err: ProviderError) {
    match err {
        ProviderError::InvalidRequest(msg) => {
            assert!(
                msg.contains("Gemini batch is not yet implemented"),
                "expected not-yet-implemented message, got: {msg}"
            );
        }
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
        .submit(vec![(
            BatchItemId::new("x"),
            ChatRequest::user(ModelHint::Explicit("gemini-2.5-pro".into()), "hi"),
        )], &tars_types::RequestContext::test_default())
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}

#[tokio::test]
async fn status_returns_not_implemented_invalid_request() {
    let provider = build_provider();
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .status(&BatchJobId::new("ignored"), &tars_types::RequestContext::test_default())
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}

#[tokio::test]
async fn results_returns_not_implemented_invalid_request() {
    let provider = build_provider();
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .results(&BatchJobId::new("ignored"), &tars_types::RequestContext::test_default())
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}

#[tokio::test]
async fn cancel_returns_not_implemented_invalid_request() {
    let provider = build_provider();
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .cancel(&BatchJobId::new("ignored"), &tars_types::RequestContext::test_default())
        .await
        .expect_err("must reject");
    assert_not_implemented(err);
}
