//! An HTTP provider request carries what the trace-header writer in its HTTP settings returns.

use std::sync::Arc;

use futures::StreamExt;
use tars_config::Config;
use tars_llm::provider::registry::ProviderRegistry;
use tars_llm::{HttpClient, HttpClientConfig, LlmProvider, ProcessEnv, basic};
use tars_types::{Caller, ChatRequest, ProviderId};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

fn writer() -> Vec<(String, String)> {
    vec![
        ("traceparent".into(), TRACEPARENT.into()),
        ("not a header name".into(), "x".into()),
    ]
}

#[tokio::test]
async fn a_provider_request_carries_the_configured_trace_headers_and_skips_an_invalid_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("traceparent", TRACEPARENT))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "[providers.local]\ntype = \"openai_compat\"\ngen_ai_provider_name = \"local\"\nbase_url = \"{}/v1\"\ndefault_model = \"m\"\n",
        server.uri()
    );
    let cfg = Config::from_toml(&toml).unwrap();
    let registry = ProviderRegistry::from_config(
        cfg.providers(),
        tars_config::catalog(),
        HttpClient::new(
            HttpClientConfig {
                trace_headers: Some(writer),
                ..HttpClientConfig::default()
            },
            Arc::new(ProcessEnv),
        )
        .unwrap(),
        tars_llm::OutputReserve::default(),
        basic(),
        None,
    )
    .unwrap();
    let provider: Arc<dyn LlmProvider> = registry
        .get(&"local".parse::<ProviderId>().unwrap())
        .unwrap()
        .clone();

    let mut stream = provider
        .stream(ChatRequest::user("x"), "m", Caller::test_default())
        .await
        .expect("the request matched, so the header was sent");
    while let Some(ev) = stream.next().await {
        ev.unwrap();
    }
}
