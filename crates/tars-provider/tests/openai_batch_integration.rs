//! End-to-end OpenAI batch tests against a wiremock-backed server.
//!
//! Two-step submit (file upload + batch create), three terminal states
//! (Completed / Failed / Expired / Cancelled), output JSONL parsing.

use std::sync::Arc;

use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_provider::auth::{Auth, basic};
use tars_provider::backends::openai::OpenAiProviderBuilder;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::provider::LlmProvider;
use tars_types::{
    BatchItemId, BatchJobId, BatchStatus, ChatRequest, ModelHint, ProviderError,
};

fn build_provider(server: &MockServer) -> Arc<dyn LlmProvider> {
    let http = HttpProviderBase::default_arc().unwrap();
    OpenAiProviderBuilder::new("openai_test", Auth::inline("sk-test"))
        .base_url(server.uri())
        .build(http, basic())
}

#[tokio::test]
async fn submit_uploads_jsonl_file_then_creates_batch() {
    let server = MockServer::start().await;

    // Step 1: file upload (multipart) returns a file id.
    Mock::given(method("POST"))
        .and(path("/files"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "file-upload-1",
            "object": "file",
            "purpose": "batch"
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Step 2: batch create references the uploaded file id.
    Mock::given(method("POST"))
        .and(path("/batches"))
        .and(body_partial_json(serde_json::json!({
            "input_file_id": "file-upload-1",
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_01abc",
            "object": "batch",
            "status": "validating"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().expect("openai supports batch");
    let id = submitter
        .submit(vec![
            (
                BatchItemId::new("draft-1"),
                ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "draft one"),
            ),
            (
                BatchItemId::new("draft-2"),
                ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "draft two"),
            ),
        ])
        .await
        .unwrap();
    assert_eq!(id.as_str(), "batch_01abc");
}

#[tokio::test]
async fn submit_empty_items_is_invalid_request_before_http() {
    let server = MockServer::start().await;
    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter.submit(vec![]).await.expect_err("reject");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn status_translates_in_progress_with_counts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/batches/batch_xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_xyz",
            "object": "batch",
            "status": "in_progress",
            "request_counts": {"total": 100, "completed": 30, "failed": 2}
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let st = submitter
        .status(&BatchJobId::new("batch_xyz"))
        .await
        .unwrap();
    match st {
        BatchStatus::InProgress {
            processed,
            total,
            eta: _,
        } => {
            assert_eq!(processed, 32);
            assert_eq!(total, Some(100));
        }
        other => panic!("expected InProgress, got {other:?}"),
    }
}

#[tokio::test]
async fn status_failed_surfaces_message_from_errors_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/batches/batch_dead"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_dead",
            "status": "failed",
            "errors": {
                "data": [
                    {"code": "x", "message": "invalid input file format"}
                ]
            }
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let st = submitter
        .status(&BatchJobId::new("batch_dead"))
        .await
        .unwrap();
    match st {
        BatchStatus::Failed { message, .. } => {
            assert!(message.contains("invalid input file format"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn status_expired_and_cancelled() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/batches/batch_exp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_exp",
            "status": "expired"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/batches/batch_can"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_can",
            "status": "cancelled"
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    assert_eq!(
        submitter.status(&BatchJobId::new("batch_exp")).await.unwrap(),
        BatchStatus::Expired,
    );
    assert_eq!(
        submitter.status(&BatchJobId::new("batch_can")).await.unwrap(),
        BatchStatus::Cancelled,
    );
}

#[tokio::test]
async fn results_downloads_output_file_and_parses_jsonl() {
    let server = MockServer::start().await;

    // results() first calls status (Completed) and reads output_file_id.
    Mock::given(method("GET"))
        .and(path("/batches/batch_done"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_done",
            "status": "completed",
            "output_file_id": "file-out-1",
            "request_counts": {"total": 2, "completed": 1, "failed": 1}
        })))
        .mount(&server)
        .await;

    // Then GET /files/{output_file_id}/content returns JSONL bytes.
    let jsonl = r#"{"custom_id":"draft-1","response":{"status_code":200,"body":{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hello batch"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}},"error":null}
{"custom_id":"draft-2","response":null,"error":{"code":"invalid_request","message":"bad input"}}"#;
    Mock::given(method("GET"))
        .and(path("/files/file-out-1/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(jsonl))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let results = submitter
        .results(&BatchJobId::new("batch_done"))
        .await
        .unwrap();
    assert_eq!(results.len(), 2);

    assert_eq!(results[0].item_id.as_str(), "draft-1");
    let succ = results[0].result.as_ref().expect("draft-1 should succeed");
    assert!(succ.text.contains("hello batch"));
    assert_eq!(succ.usage.input_tokens, 10);
    assert_eq!(succ.usage.output_tokens, 2);

    assert_eq!(results[1].item_id.as_str(), "draft-2");
    let err = results[1].result.as_ref().expect_err("draft-2 should fail");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn results_on_non_terminal_refuses_without_fetching_output_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/batches/batch_running"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_running",
            "status": "in_progress",
            "request_counts": {"total": 10, "completed": 1, "failed": 0}
        })))
        .mount(&server)
        .await;
    // file content endpoint NOT mocked — must not be called.

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .results(&BatchJobId::new("batch_running"))
        .await
        .expect_err("must refuse");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn results_completed_without_output_file_is_empty_vec() {
    // Edge case: batch completed but vendor didn't populate output_file_id
    // (cancelled mid-finalization, or all items errored to error_file_id).
    // V1 returns empty rather than fabricating items.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/batches/batch_empty"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_empty",
            "status": "completed"
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let r = submitter
        .results(&BatchJobId::new("batch_empty"))
        .await
        .unwrap();
    assert!(r.is_empty());
}

#[tokio::test]
async fn cancel_posts_to_cancel_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/batches/batch_01abc/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_01abc",
            "status": "cancelling"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    submitter
        .cancel(&BatchJobId::new("batch_01abc"))
        .await
        .unwrap();
}
