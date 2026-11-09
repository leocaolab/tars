//! Tests the Anthropic batch methods against a wiremock server.

use std::sync::Arc;
use tars_types::ProviderId;

use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::anthropic::AnthropicProviderBuilder;
use tars_llm::provider::http_base::HttpClient;
use tars_llm::{BatchStatus, ProviderError};
use tars_types::{BatchItemId, BatchJobId, ChatRequest};

fn build_provider(server: &MockServer) -> Arc<dyn LlmProvider> {
    let http = HttpClient::default_arc().unwrap();

    (AnthropicProviderBuilder::new(ProviderId::new("anthropic_test"), Auth::inline("test-key"))
        .base_url(server.uri())
        .build(http, basic())) as _
}

#[tokio::test]
async fn submit_posts_requests_array_and_returns_job_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(serde_json::json!({
            "requests": [
                {"custom_id": "draft-1"},
                {"custom_id": "draft-2"},
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msgbatch_01abc",
            "type": "message_batch",
            "processing_status": "in_progress",
            "request_counts": {
                "processing": 2,
                "succeeded": 0,
                "errored": 0,
                "canceled": 0,
                "expired": 0
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider
        .as_batch_submitter()
        .expect("anthropic supports batch");

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
    assert_eq!(id.as_str(), "msgbatch_01abc");
}

#[tokio::test]
async fn submit_empty_items_is_invalid_request() {
    let server = MockServer::start().await;
    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();

    let err = submitter
        .submit(vec![], "test-model", &tars_types::Caller::test_default())
        .await
        .expect_err("must reject");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn status_translates_in_progress() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_01abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msgbatch_01abc",
            "processing_status": "in_progress",
            "request_counts": {
                "processing": 8,
                "succeeded": 2,
                "errored": 0,
                "canceled": 0,
                "expired": 0
            }
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let st = submitter
        .status(
            &BatchJobId::new("msgbatch_01abc"),
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
            assert_eq!(processed, Some(2));
            assert_eq!(total, Some(10));
        }
        other => panic!("expected InProgress, got {other:?}"),
    }
}

#[tokio::test]
async fn status_translates_ended_to_completed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_done"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msgbatch_done",
            "processing_status": "ended",
            "request_counts": {
                "processing": 0,
                "succeeded": 9,
                "errored": 1,
                "canceled": 0,
                "expired": 0
            }
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    assert_eq!(
        submitter
            .status(
                &BatchJobId::new("msgbatch_done"),
                &tars_types::Caller::test_default()
            )
            .await
            .unwrap(),
        BatchStatus::Completed,
    );
}

#[tokio::test]
async fn status_all_expired_maps_to_expired() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_exp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msgbatch_exp",
            "processing_status": "ended",
            "request_counts": {
                "processing": 0,
                "succeeded": 0,
                "errored": 0,
                "canceled": 0,
                "expired": 5
            }
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    assert_eq!(
        submitter
            .status(
                &BatchJobId::new("msgbatch_exp"),
                &tars_types::Caller::test_default()
            )
            .await
            .unwrap(),
        BatchStatus::Expired,
    );
}

#[tokio::test]
async fn results_parses_jsonl_with_mixed_outcomes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_done"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msgbatch_done",
            "processing_status": "ended",
            "request_counts": {
                "processing": 0,
                "succeeded": 1,
                "errored": 1,
                "canceled": 0,
                "expired": 0
            }
        })))
        .mount(&server)
        .await;

    let jsonl = r#"{"custom_id":"draft-1","result":{"type":"succeeded","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2}}}}
{"custom_id":"draft-2","result":{"type":"errored","error":{"type":"invalid_request_error","message":"bad prompt"}}}"#;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_done/results"))
        .respond_with(ResponseTemplate::new(200).set_body_string(jsonl))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let results = submitter
        .results(
            &BatchJobId::new("msgbatch_done"),
            &tars_types::Caller::test_default(),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].item_id.as_str(), "draft-1");
    let succ = results[0].result.as_ref().expect("draft-1 should succeed");
    assert!(succ.text.contains("hello"));
    assert_eq!(succ.usage.unwrap().input_tokens, 10);
    assert_eq!(succ.usage.unwrap().output_tokens, 2);

    assert_eq!(results[1].item_id.as_str(), "draft-2");
    let err = results[1].result.as_ref().expect_err("draft-2 should fail");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn results_on_non_terminal_returns_invalid_request_without_fetching() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_pending"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "processing_status": "in_progress",
            "request_counts": {
                "processing": 5,
                "succeeded": 0,
                "errored": 0,
                "canceled": 0,
                "expired": 0
            }
        })))
        .mount(&server)
        .await;
    // `/results` is not mocked: `results()` must refuse after `status()`, not fetch.

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .results(
            &BatchJobId::new("msgbatch_pending"),
            &tars_types::Caller::test_default(),
        )
        .await
        .expect_err("must refuse on non-terminal");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn cancel_posts_to_cancel_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches/msgbatch_01abc/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msgbatch_01abc",
            "processing_status": "canceling"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    submitter
        .cancel(
            &BatchJobId::new("msgbatch_01abc"),
            &tars_types::Caller::test_default(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn submit_propagates_http_error_via_classifier() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "type": "error",
            "error": {"type": "authentication_error", "message": "invalid x-api-key"}
        })))
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let submitter = provider.as_batch_submitter().unwrap();
    let err = submitter
        .submit(
            vec![(BatchItemId::new("x"), ChatRequest::user("hi"))],
            "test-model",
            &tars_types::Caller::test_default(),
        )
        .await
        .expect_err("401 should error");
    // Only that it is not a network or parse error; the kind is the classifier's.
    assert!(
        !matches!(err, ProviderError::Network(_) | ProviderError::Parse { .. }),
        "401 should not bubble as Network/Parse, got {err:?}"
    );
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
            .and(header("x-org", "org-1"))
            .and(header("x-api-key", "test-key"))
    };
    configured()
        .and(method("POST"))
        .and(path("/v1/messages/batches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "msgbatch_h"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    configured()
        .and(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_h"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "processing_status": "ended",
            "request_counts": {
                "processing": 0, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    configured()
        .and(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_h/results"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .expect(1)
        .mount(&server)
        .await;

    let http = HttpClient::new(tars_llm::HttpClientConfig::default(), Arc::new(OrgEnv)).unwrap();
    let extras = tars_llm::HttpOverrides {
        http_headers: [("X-Static".into(), "s-1".into())].into_iter().collect(),
        env_http_headers: [("X-Org".into(), "TEST_ORG".into())].into_iter().collect(),
        ..Default::default()
    };
    let provider: Arc<dyn LlmProvider> =
        AnthropicProviderBuilder::new(ProviderId::new("anthropic_test"), Auth::inline("test-key"))
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
        .and(path("/v1/messages/batches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "msgbatch_q"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_q"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "processing_status": "ended",
            "request_counts": {
                "processing": 0, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_q/results"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches/msgbatch_q/cancel"))
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
        AnthropicProviderBuilder::new(ProviderId::new("anthropic_test"), Auth::inline("test-key"))
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
            "/v1/messages/batches",
            "/v1/messages/batches/msgbatch_q",
            "/v1/messages/batches/msgbatch_q/results",
            "/v1/messages/batches/msgbatch_q/cancel",
        ]
        .map(|p| (p.to_string(), Some("a=2&z=1")))
    );
}
