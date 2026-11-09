//! Tests the OpenAI batch methods against a wiremock server.

use std::sync::Arc;
use tars_types::ProviderId;

use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::openai::OpenAiProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_llm::{BatchStatus, ProviderError};
use tars_types::{BatchItemId, BatchJobId, ChatRequest};

fn build_provider(server: &MockServer) -> Arc<dyn LlmProvider> {
    let http = HttpClient::default_arc().unwrap();
    OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("sk-test"))
        .base_url(server.uri())
        .build(http, basic())
}

#[tokio::test]
async fn submit_uploads_jsonl_file_then_creates_batch() {
    let server = MockServer::start().await;

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
    let submitter = provider
        .as_batch_submitter()
        .expect("openai supports batch");
    let id = submitter
        .submit(
            vec![
                (BatchItemId::new("draft-1"), ChatRequest::user("draft one")),
                (BatchItemId::new("draft-2"), ChatRequest::user("draft two")),
            ],
            "test-model",
            &tars_types::Caller::test_default(),
        )
        .await
        .unwrap();
    assert_eq!(id.as_str(), "batch_01abc");
}

#[tokio::test]
async fn submit_empty_items_is_invalid_request_before_http() {
    let server = MockServer::start().await;
    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .submit(vec![], "test-model", &tars_types::Caller::test_default())
        .await
        .expect_err("reject");
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
        .status(
            &BatchJobId::new("batch_xyz"),
            &tars_types::Caller::test_default(),
        )
        .await
        .unwrap();
    match st {
        BatchStatus::InProgress {
            processed,
            total,
            eta: _,
        } => {
            assert_eq!(processed, Some(32));
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
        .status(
            &BatchJobId::new("batch_dead"),
            &tars_types::Caller::test_default(),
        )
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
        submitter
            .status(
                &BatchJobId::new("batch_exp"),
                &tars_types::Caller::test_default()
            )
            .await
            .unwrap(),
        BatchStatus::Expired,
    );
    assert_eq!(
        submitter
            .status(
                &BatchJobId::new("batch_can"),
                &tars_types::Caller::test_default()
            )
            .await
            .unwrap(),
        BatchStatus::Cancelled,
    );
}

#[tokio::test]
async fn results_downloads_output_file_and_parses_jsonl() {
    let server = MockServer::start().await;

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
        .results(
            &BatchJobId::new("batch_done"),
            &tars_types::Caller::test_default(),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 2);

    assert_eq!(results[0].item_id.as_str(), "draft-1");
    let succ = results[0].result.as_ref().expect("draft-1 should succeed");
    assert!(succ.text.contains("hello batch"));
    assert_eq!(succ.usage.unwrap().input_tokens, 10);
    assert_eq!(succ.usage.unwrap().output_tokens, 2);

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
    // The file endpoint is not mocked: it must not be called.

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .results(
            &BatchJobId::new("batch_running"),
            &tars_types::Caller::test_default(),
        )
        .await
        .expect_err("must refuse");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn results_completed_without_output_file_is_empty_vec() {
    // A completed batch whose items all failed has no output file; the error file is not read,
    // so those failures do not appear.
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
        .results(
            &BatchJobId::new("batch_empty"),
            &tars_types::Caller::test_default(),
        )
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
        .cancel(
            &BatchJobId::new("batch_01abc"),
            &tars_types::Caller::test_default(),
        )
        .await
        .unwrap();
}

struct OrgEnv;

impl tars_llm::EnvSource for OrgEnv {
    fn var(&self, name: &str) -> Result<String, std::env::VarError> {
        match name {
            "TEST_ORG" => Ok("org-1".into()),
            _ => Err(std::env::VarError::NotPresent),
        }
    }
}

#[tokio::test]
async fn batch_requests_carry_the_configured_static_and_env_headers() {
    let server = MockServer::start().await;
    let configured = || {
        Mock::given(header("x-static", "s-1"))
            .and(header("openai-organization", "org-1"))
            .and(header("authorization", "Bearer sk-test"))
    };
    configured()
        .and(method("POST"))
        .and(path("/files"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "file-in"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    configured()
        .and(method("POST"))
        .and(path("/batches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "batch_h"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    configured()
        .and(method("GET"))
        .and(path("/batches/batch_h"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_h",
            "status": "completed",
            "output_file_id": "file-out",
            "request_counts": {"total": 0, "completed": 0, "failed": 0}
        })))
        .expect(1)
        .mount(&server)
        .await;
    configured()
        .and(method("GET"))
        .and(path("/files/file-out/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .expect(1)
        .mount(&server)
        .await;

    let http = HttpClient::new(tars_llm::HttpClientConfig::default(), Arc::new(OrgEnv)).unwrap();
    let extras = tars_llm::HttpOverrides {
        http_headers: [("X-Static".into(), "s-1".into())].into_iter().collect(),
        env_http_headers: [("OpenAI-Organization".into(), "TEST_ORG".into())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let provider: Arc<dyn LlmProvider> =
        OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("sk-test"))
            .base_url(server.uri())
            .extras(extras)
            .build(http, basic());
    let submitter = provider.as_batch_submitter().unwrap();
    let ctx = tars_types::Caller::test_default();

    let id = submitter
        .submit(
            vec![(BatchItemId::new("a"), ChatRequest::user("one"))],
            "test-model",
            &ctx,
        )
        .await
        .unwrap();
    let results = submitter.results(&id, &ctx).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn every_batch_request_carries_the_configured_query_params_sorted_by_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/files"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "file-in"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/batches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "batch_q"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/batches/batch_q"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "batch_q",
            "status": "completed",
            "output_file_id": "file-out",
            "request_counts": {"total": 0, "completed": 0, "failed": 0}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/file-out/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/batches/batch_q/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let extras = tars_llm::HttpOverrides {
        query_params: [("z".into(), "1".into()), ("a".into(), "2".into())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let provider: Arc<dyn LlmProvider> =
        OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("sk-test"))
            .base_url(server.uri())
            .extras(extras)
            .build(HttpClient::default_arc().unwrap(), basic());
    let submitter = provider.as_batch_submitter().unwrap();
    let ctx = tars_types::Caller::test_default();

    let id = submitter
        .submit(
            vec![(BatchItemId::new("a"), ChatRequest::user("one"))],
            "test-model",
            &ctx,
        )
        .await
        .unwrap();
    submitter.results(&id, &ctx).await.unwrap();
    submitter.cancel(&id, &ctx).await.unwrap();

    let received = server.received_requests().await.unwrap();
    let sent: Vec<(String, Option<&str>)> = received
        .iter()
        .map(|r| (r.url.path().to_string(), r.url.query()))
        .collect();
    assert_eq!(
        sent,
        [
            "/files",
            "/batches",
            "/batches/batch_q",
            "/files/file-out/content",
            "/batches/batch_q/cancel",
        ]
        .map(|p| (p.to_string(), Some("a=2&z=1")))
    );
}
