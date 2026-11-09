//! A provider the data file has no data on is sent what it is asked: the dispatch check refuses
//! only for a capability the profile knows is missing, and the server answers for itself.

use std::sync::Arc;

use futures::StreamExt;
use tars_config::Config;
use tars_llm::LlmService;
use tars_llm::provider::backends::openai::OpenAiProviderBuilder;
use tars_llm::provider::registry::ProviderRegistry;
use tars_llm::{Auth, HttpClient, LlmProvider, basic};
use tars_llm::{LlmError, ProviderError};
use tars_types::{
    Caller, ChatEvent, ChatRequest, JsonSchema, ProviderId, ThinkingMode, ToolSpec, json_object,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(chunks: &[&str]) -> String {
    let mut s = String::new();
    for c in chunks {
        s.push_str("data: ");
        s.push_str(c);
        s.push_str("\n\n");
    }
    s.push_str("data: [DONE]\n\n");
    s
}

async fn server_replying(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

async fn ok_server() -> MockServer {
    server_replying(sse(&[
        r#"{"model":"m","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#,
        r#"{"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]))
    .await
}

fn registry_provider(toml: &str, id: &str) -> Arc<dyn LlmProvider> {
    let cfg = Config::from_toml(toml).unwrap();
    let reg = ProviderRegistry::from_config(
        cfg.providers(),
        tars_config::catalog(),
        HttpClient::default_arc().unwrap(),
        basic(),
        None,
    )
    .unwrap();
    reg.get(&id.parse::<ProviderId>().unwrap()).unwrap().clone()
}

fn local_block(kind: &str, uri: &str, extra: &str) -> String {
    format!(
        "[providers.local]\ntype = \"{kind}\"\nbase_url = \"{uri}/v1\"\ndefault_model = \"qwen3\"\n{extra}"
    )
}

async fn call(p: Arc<dyn LlmProvider>, req: ChatRequest) -> Result<Vec<ChatEvent>, LlmError> {
    let mut stream = LlmService::of(p, "qwen3")
        .call(req, Caller::test_default())
        .await
        .result?;
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev?);
    }
    Ok(events)
}

/// tars-node `complete({ thinking: true })` builds this request.
fn thinking_request() -> ChatRequest {
    let mut req = ChatRequest::user("ping");
    req.thinking = ThinkingMode::Auto;
    req.enable_chat_template_thinking = Some(true);
    req
}

fn tool_request() -> ChatRequest {
    let mut req = ChatRequest::user("ping");
    req.tools.push(
        ToolSpec::new(
            "lookup",
            "look it up",
            JsonSchema::loose(json_object! {"type": "object"}),
        )
        .unwrap(),
    );
    req
}

async fn requests_received(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn a_local_server_is_sent_thinking_it_has_no_data_on() {
    for kind in ["vllm", "mlx", "llamacpp"] {
        let server = ok_server().await;
        let p = registry_provider(&local_block(kind, &server.uri(), ""), "local");
        assert!(call(p, thinking_request()).await.is_ok(), "{kind}");
        assert_eq!(requests_received(&server).await, 1, "{kind}");
    }
}

#[tokio::test]
async fn a_local_server_declared_without_thinking_is_refused_before_sending() {
    let server = ok_server().await;
    let p = registry_provider(
        &local_block("vllm", &server.uri(), "supports_thinking = false"),
        "local",
    );
    assert!(matches!(
        call(p, thinking_request()).await,
        Err(LlmError::Incompatible { .. })
    ));
    assert_eq!(requests_received(&server).await, 0);
}

#[tokio::test]
async fn an_undescribed_compatible_server_is_sent_tools_and_a_long_prompt() {
    let server = ok_server().await;
    let p = registry_provider(
        &local_block(
            "openai_compat",
            &server.uri(),
            "gen_ai_provider_name = \"local\"",
        ),
        "local",
    );
    assert!(call(p.clone(), tool_request()).await.is_ok());
    assert!(
        call(p, ChatRequest::user("x".repeat(150_000)))
            .await
            .is_ok()
    );
    assert_eq!(requests_received(&server).await, 2);
}

#[tokio::test]
async fn an_undescribed_compatible_server_is_sent_the_schema_and_the_call_is_not_degraded() {
    let server = ok_server().await;
    let p = registry_provider(
        &local_block(
            "openai_compat",
            &server.uri(),
            "gen_ai_provider_name = \"local\"",
        ),
        "local",
    );
    let mut req = ChatRequest::user("give json");
    req.structured_output = Some(JsonSchema::strict("R", json_object! {"type": "object"}));
    let events = call(p, req).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ChatEvent::Degraded { .. })),
        "{events:?}"
    );
    let sent: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["response_format"]["type"], "json_schema");
}

#[tokio::test]
async fn a_compatible_server_declared_without_tool_use_is_refused_before_sending() {
    let server = ok_server().await;
    let p = registry_provider(
        &local_block(
            "openai_compat",
            &server.uri(),
            "supports_tool_use = false\ngen_ai_provider_name = \"local\"",
        ),
        "local",
    );
    assert!(matches!(
        call(p, tool_request()).await,
        Err(LlmError::Incompatible { .. })
    ));
    assert_eq!(requests_received(&server).await, 0);
}

#[tokio::test]
async fn a_tool_call_cut_off_by_the_output_limit_is_reported_as_truncated() {
    let start = r#"{"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":""}}]},"finish_reason":null}]}"#;
    let half = r#"{"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.txt\",\"content\":\"lorem ip"}}]},"finish_reason":null}]}"#;
    let cut = r#"{"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#;
    let server = server_replying(sse(&[start, half, cut])).await;
    let p = OpenAiProviderBuilder::new(ProviderId::new("openai_test"), Auth::inline("k"))
        .base_url(format!("{}/v1", server.uri()))
        .build(HttpClient::default_arc().unwrap(), basic());
    match p
        .complete(
            ChatRequest::user("write it"),
            "gpt-4o",
            Caller::test_default(),
        )
        .await
    {
        Err(ProviderError::TruncatedToolCall { name, raw, .. }) => {
            assert_eq!(name, "write_file");
            assert!(raw.ends_with("lorem ip"), "{raw}");
        }
        other => panic!("expected a truncated tool call, got {other:?}"),
    }
}

/// Returns whether a call that sets the chat template's thinking switch sent
/// `chat_template_kwargs` to an `openai_compat` provider named `id`, with
/// `supports_chat_template_thinking` declared as `declared`.
async fn sends_chat_template_kwargs(id: &str, default_model: &str, declared: bool) -> bool {
    let server = ok_server().await;
    let toml = format!(
        "[providers.{id}]\ntype = \"openai_compat\"\ngen_ai_provider_name = \"local\"\n\
         base_url = \"{}/v1\"\ndefault_model = \"{default_model}\"\n\
         supports_chat_template_thinking = {declared}\n",
        server.uri()
    );
    let mut req = ChatRequest::user("ping");
    req.enable_chat_template_thinking = Some(true);
    call(registry_provider(&toml, id), req).await.unwrap();
    let sent: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    sent.get("chat_template_kwargs").is_some()
}

#[tokio::test]
async fn a_declared_chat_template_switch_decides_whether_chat_template_kwargs_is_sent() {
    // `local` is undescribed at a self-hosted URL, so it reads the chat template by default;
    // the data file's `xai` block says it does not.
    for (id, model) in [("local", "qwen3"), ("xai", "grok-4.5")] {
        assert!(!sends_chat_template_kwargs(id, model, false).await, "{id}");
        assert!(sends_chat_template_kwargs(id, model, true).await, "{id}");
    }
}
