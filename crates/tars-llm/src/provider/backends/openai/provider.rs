//! Builds and runs the OpenAI provider: chat over HTTP, plus the Batch API.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use url::Url;

use crate::{BatchResultItem, BatchStatus, ProviderError};
use tars_types::{
    BatchItemId, BatchJobId, Caller, ChatEvent, ChatRequest, ProviderId, ProviderProfile,
};

use crate::provider::auth::{Auth, AuthResolver};
use crate::provider::batch::BatchSubmitter;
use crate::provider::error::{ParseProblem, RequestRefusal};
use crate::provider::http_base::{
    ERROR_BODY_CAP_BYTES, HttpAdapter, HttpClient, HttpOverrides, insert_extra_headers,
    read_bounded_body, stream_via_adapter, truncate_utf8,
};
use crate::provider::wire::WireRecorder;
use crate::provider::{LlmEventStream, LlmProvider};

use super::adapter::{OpenAiAdapter, tool_name_from_wire};
use super::dialect::{DeepSeekDialect, OpenAiDialect, StandardDialect};
use super::mapping::{
    openai_auth_only_headers, parse_openai_batch_results, translate_openai_batch_status,
};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Clone)]
pub struct OpenAiProviderBuilder {
    id: ProviderId,
    base_url: String,
    auth: Auth,
    capabilities: Option<ProviderProfile>,
    extras: HttpOverrides,
    dialect: Arc<dyn OpenAiDialect>,
    output_limits: tars_config::OutputLimitRule,
    wire: Option<WireRecorder>,
}

impl OpenAiProviderBuilder {
    pub fn new(id: ProviderId, auth: Auth) -> Self {
        Self {
            id,
            base_url: DEFAULT_BASE_URL.to_string(),
            auth,
            capabilities: None,
            extras: HttpOverrides::default(),
            dialect: Arc::new(StandardDialect),
            output_limits: tars_config::OutputLimitRule::catalog("openai"),
            wire: None,
        }
    }

    builder_setter! {
        /// Sets the dialect, by default OpenAI's own. The base URL does not choose it: a
        /// gateway in front of OpenAI is still OpenAI.
        dialect: Arc<dyn OpenAiDialect>
    }

    builder_setter! {
        /// Sets the base URL, for an OpenAI-compatible server.
        base_url: into String
    }

    builder_setter! {
        /// Sets the capabilities, by default those of the `openai` block of
        /// `data/provider.toml`; an OpenAI-compatible server should set its own. The provider
        /// claims only what its adapter sends: tool-use emulation reads as no mechanism.
        capabilities: opt ProviderProfile
    }

    builder_setter! {
        extras: HttpOverrides
    }

    builder_setter! {
        /// Sets how [`LlmProvider::output_limit`] answers, by default from the `openai` catalog
        /// block; an OpenAI-compatible server names its own block and any configured
        /// `max_output_tokens`.
        output_limits: tars_config::OutputLimitRule
    }

    builder_setter! {
        /// Sets where each attempt's raw bytes are recorded; `None` records none.
        wire: Option<WireRecorder>
    }

    pub fn build(
        self,
        http: Arc<HttpClient>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<OpenAiProvider> {
        let caps = self
            .capabilities
            .unwrap_or_else(default_openai_capabilities);
        let dialect = self.dialect;
        let adapter = Arc::new(
            OpenAiAdapter::new(self.base_url, self.extras, caps).with_dialect(dialect.clone()),
        );
        Arc::new(OpenAiProvider {
            id: self.id,
            http,
            auth_resolver,
            auth: self.auth,
            adapter,
            dialect,
            output_limits: self.output_limits,
            wire: self.wire,
        })
    }
}

/// The server an `openai_compat` provider points at, guessed from its base URL, since that
/// config type says only that the server speaks OpenAI's protocol. A `type = "openai"` provider
/// is OpenAI whatever its URL and is never guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompatServer {
    /// DeepSeek's own API (`api.deepseek.com`).
    DeepSeek,
    /// OpenAI or Azure OpenAI, reached through `openai_compat`.
    OpenAi,
    /// A hosted vendor API that serves OpenAI's protocol with no chat template of the caller's
    /// (Groq, OpenRouter, xAI).
    HostedApi,
    /// Any other server, taken to serve its model through a chat template that reads
    /// `chat_template_kwargs`, as vLLM, mlx_lm.server, llama-server and LM Studio do.
    Other,
}

impl CompatServer {
    /// Returns the server at `base_url`, judged by its host: a self-hosted server whose host or
    /// path merely mentions a vendor is `Other`.
    pub(crate) fn at(base_url: &str) -> Self {
        // [arc:intentional-handle] reason: a URL that does not parse has no host to judge; the
        // HTTP call to it fails on its own, so the guess does not matter.
        let host = Url::parse(base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
        match host.as_deref() {
            Some(h) if is_host_or_subdomain(h, "deepseek.com") => Self::DeepSeek,
            Some("api.openai.com") => Self::OpenAi,
            Some(h) if AZURE_OPENAI_HOST_SUFFIXES.iter().any(|s| h.ends_with(s)) => Self::OpenAi,
            Some(h) if HOSTED_API_HOSTS.contains(&h) => Self::HostedApi,
            _ => Self::Other,
        }
    }

    pub(crate) fn dialect(self) -> Arc<dyn OpenAiDialect> {
        match self {
            Self::DeepSeek => Arc::new(DeepSeekDialect),
            Self::OpenAi | Self::HostedApi | Self::Other => Arc::new(StandardDialect),
        }
    }

    pub(crate) fn reads_chat_template(self) -> bool {
        matches!(self, Self::Other)
    }
}

fn is_host_or_subdomain(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// The hosted APIs the README points `openai_compat` at.
const HOSTED_API_HOSTS: &[&str] = &["api.groq.com", "openrouter.ai", "api.x.ai"];

const AZURE_OPENAI_HOST_SUFFIXES: &[&str] = &[
    ".openai.azure.com",
    ".cognitiveservices.azure.com",
    ".services.ai.azure.com",
];

/// Returns the capabilities of the `openai` block of `data/provider.toml`, for a builder given
/// none.
pub fn default_openai_capabilities() -> ProviderProfile {
    tars_config::capabilities_for("openai", "")
}

pub struct OpenAiProvider {
    id: ProviderId,
    http: Arc<HttpClient>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<OpenAiAdapter>,
    /// The same dialect as `adapter`'s, so batch results decode as streamed ones do.
    dialect: Arc<dyn OpenAiDialect>,
    output_limits: tars_config::OutputLimitRule,
    wire: Option<WireRecorder>,
}

/// Gives each `ToolCallStart` the name of the request's tool it was sent as; `tools` is `None`
/// when no tool name changes on the wire.
fn restore_tool_names(
    inner: LlmEventStream,
    tools: Option<Vec<tars_types::ToolSpec>>,
) -> LlmEventStream {
    use futures::StreamExt as _;
    let Some(tools) = tools else {
        return inner;
    };
    Box::pin(inner.map(move |ev| match ev {
        Ok(ChatEvent::ToolCallStart { index, id, name }) => Ok(ChatEvent::ToolCallStart {
            index,
            id,
            name: tool_name_from_wire(&name, &tools),
        }),
        other => other,
    }))
}

/// Returns `inner` untouched, unless the dialect's text is readable only whole
/// ([`OpenAiDialect::text_is_only_whole`]): then the text deltas are held until the stream ends,
/// given to [`OpenAiDialect::finalize`], and sent on as one delta and the tool calls it found,
/// before the `Finished` or the error that ended it, or at the body's end when the server sent
/// neither. This is done here, not in the consumers that assemble the stream, because they do
/// not know the dialect. The first error ends the stream.
fn whole_text(inner: LlmEventStream, dialect: Arc<dyn OpenAiDialect>) -> LlmEventStream {
    if !dialect.text_is_only_whole() {
        return inner;
    }
    let out = async_stream::stream! {
        use futures::StreamExt as _;
        let mut inner = inner;
        let mut held = String::new();
        while let Some(ev) = inner.next().await {
            match ev {
                Ok(tars_types::ChatEvent::Delta { text }) => held.push_str(&text),
                Ok(ev @ tars_types::ChatEvent::Finished { .. }) => {
                    for lifted in finalized_text(dialect.as_ref(), std::mem::take(&mut held)) {
                        yield Ok(lifted);
                    }
                    yield Ok(ev);
                }
                Ok(other) => yield Ok(other),
                Err(e) => {
                    for lifted in finalized_text(dialect.as_ref(), std::mem::take(&mut held)) {
                        yield Ok(lifted);
                    }
                    yield Err(e);
                    return;
                }
            }
        }
        for lifted in finalized_text(dialect.as_ref(), held) {
            yield Ok(lifted);
        }
    };
    Box::pin(out)
}

/// Returns the events of the held `text` once `dialect` has read it whole: one delta of what is
/// left of the text, then each tool call it found. Empty text gives no events.
fn finalized_text(dialect: &dyn OpenAiDialect, text: String) -> Vec<tars_types::ChatEvent> {
    if text.is_empty() {
        return Vec::new();
    }
    let (mut text, mut tool_calls) = (text, Vec::new());
    dialect.finalize(&mut text, &mut tool_calls);
    let mut events = Vec::new();
    if !text.is_empty() {
        events.push(tars_types::ChatEvent::Delta { text });
    }
    for (index, c) in tool_calls.into_iter().enumerate() {
        events.push(tars_types::ChatEvent::ToolCallStart {
            index,
            id: c.id.clone(),
            name: c.name.clone(),
        });
        events.push(tars_types::ChatEvent::ToolCallEnd {
            index,
            id: c.id,
            parsed_args: c.arguments,
            thought_signature: None,
        });
    }
    events
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &ProviderProfile {
        self.adapter.profile()
    }
    fn output_limit(&self, model: &str) -> tars_types::OutputLimit {
        self.output_limits.output_limit(model)
    }

    // `err(Display)` logs each failed call once, with the provider id and model.
    #[tracing::instrument(
        name = "openai.stream",
        skip_all,
        fields(provider = %self.id, model = %model),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, &ctx).await?;
        // Only a name with a `.` changes on the wire, so only then are the tools kept.
        let renamed_tools = req
            .tools
            .iter()
            .any(|t| t.name.contains('.'))
            .then(|| req.tools.clone());
        let inner = stream_via_adapter(
            self.http.clone(),
            self.adapter.clone(),
            auth,
            req,
            model,
            ctx,
            &self.id,
            self.wire.as_ref(),
        )
        .await?;
        Ok(restore_tool_names(
            whole_text(inner, self.dialect.clone()),
            renamed_tools,
        ))
    }

    /// Returns the streamed response assembled, then given to [`OpenAiDialect::finalize`] (a
    /// second time for a dialect whose stream already was).
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<tars_types::ChatResponse, ProviderError> {
        use futures::StreamExt as _;
        let dialect = self.dialect.clone();
        let mut s = self.stream(req, model, ctx).await?;
        let mut acc = tars_types::ChatResponseBuilder::new();
        while let Some(event) = s.next().await {
            acc.apply(event?)?;
        }
        let mut r = acc.finish()?;
        dialect.finalize(&mut r.text, &mut r.tool_calls);
        Ok(r)
    }

    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn BatchSubmitter>> {
        Some(self)
    }
}

// OpenAI's Batch API (<https://platform.openai.com/docs/api-reference/batch>) takes two steps:
// upload the requests as a `purpose=batch` file, then create the batch from that file. Results
// are two more files, `output_file_id` for answered requests and `error_file_id` for failed ones,
// each read with `GET /files/{id}/content`.
//
// Per-line JSONL shape (input):
//   {"custom_id": "...", "method":"POST", "url":"/v1/chat/completions",
//    "body": <chat completion request body>}
//
// Per-line JSONL shape (output):
//   {"custom_id": "...", "response": {"status_code":200,"body":{...}}, "error": null}
//   or {"custom_id": "...", "response": null, "error":{...}}

#[async_trait]
impl BatchSubmitter for OpenAiProvider {
    async fn submit(
        &self,
        items: Vec<(BatchItemId, ChatRequest)>,
        model: &str,
        ctx: &Caller,
    ) -> Result<BatchJobId, ProviderError> {
        if items.is_empty() {
            return Err(ProviderError::InvalidRequest(RequestRefusal::EmptyBatch));
        }

        let mut jsonl = String::with_capacity(items.len() * 256);
        for (item_id, req) in &items {
            let body = self.adapter.translate_request(req, model)?;
            let line = serde_json::to_string(&json!({
                "custom_id": item_id.as_str(),
                "method": "POST",
                "url": "/v1/chat/completions",
                "body": body,
            }))
            .map_err(ProviderError::Encode)?;
            jsonl.push_str(&line);
            jsonl.push('\n');
        }

        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let mut auth_only = openai_auth_only_headers(&auth)?;
        insert_extra_headers(
            self.adapter.extras(),
            self.http.env.as_ref(),
            &mut auth_only,
        )
        .map_err(ProviderError::ExtraHeader)?;

        let file_part = reqwest::multipart::Part::bytes(jsonl.into_bytes())
            .file_name("batch.jsonl")
            .mime_str("application/jsonl")
            .map_err(|e| ProviderError::Internal(format!("multipart part: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("purpose", "batch")
            .part("file", file_part);

        let upload_url = self.adapter.files_url("")?;
        let upload_resp = self
            .http
            .client
            .post(upload_url)
            .headers(auth_only)
            .multipart(form)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !upload_resp.status().is_success() {
            let status = upload_resp.status();
            let h = upload_resp.headers().clone();
            // Bounded: a hostile error body must not be buffered whole.
            let body = read_bounded_body(upload_resp, ERROR_BODY_CAP_BYTES).await;
            let text = truncate_utf8(&body, ERROR_BODY_CAP_BYTES);
            return Err(self.adapter.classify_error(status, &h, text));
        }
        let upload_unparsed = |problem| ProviderError::Parse {
            at: "openai file upload response",
            line: None,
            problem: Box::new(problem),
            raw: None,
        };
        let file_v: Value = upload_resp
            .json()
            .await
            .map_err(|e| upload_unparsed(ParseProblem::JsonBody(e)))?;
        let input_file_id = file_v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| upload_unparsed(ParseProblem::Missing("id")))?
            .to_string();

        // When creating the batch fails, the uploaded file stays on OpenAI and counts against
        // the account's storage until deleted; a cleanup DELETE could fail or hang in turn.
        let create_url = self.adapter.batches_url("")?;
        let mut headers = self.adapter.build_headers(&auth)?;
        insert_extra_headers(self.adapter.extras(), self.http.env.as_ref(), &mut headers)
            .map_err(ProviderError::ExtraHeader)?;
        let body = json!({
            "input_file_id": input_file_id,
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h",
        });
        let resp = self
            .http
            .client
            .post(create_url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            let text = truncate_utf8(&body, ERROR_BODY_CAP_BYTES);
            return Err(self.adapter.classify_error(status, &h, text));
        }
        let unparsed = |problem| ProviderError::Parse {
            at: "openai batch create response",
            line: None,
            problem: Box::new(problem),
            raw: None,
        };
        let v: Value = resp
            .json()
            .await
            .map_err(|e| unparsed(ParseProblem::JsonBody(e)))?;
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| unparsed(ParseProblem::Missing("id")))?;
        id.parse().map_err(|e| unparsed(ParseProblem::JobId(e)))
    }

    async fn status(&self, id: &BatchJobId, ctx: &Caller) -> Result<BatchStatus, ProviderError> {
        let v = self.fetch_batch_object(id, ctx).await?;
        translate_openai_batch_status(&v)
    }

    async fn results(
        &self,
        id: &BatchJobId,
        ctx: &Caller,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        let v = self.fetch_batch_object(id, ctx).await?;
        let status = translate_openai_batch_status(&v)?;
        if !status.is_terminal() {
            return Err(ProviderError::InvalidRequest(
                RequestRefusal::BatchNotTerminal {
                    id: id.clone(),
                    status,
                },
            ));
        }
        // Answered requests are in `output_file_id`, failed ones in `error_file_id`; a batch that
        // failed as a whole, expired or was cancelled may have neither and yields no items.
        let mut items = Vec::new();
        for key in ["output_file_id", "error_file_id"] {
            if let Some(file_id) = v.get(key).and_then(Value::as_str) {
                let text = self.file_content(file_id, ctx).await?;
                items.extend(parse_openai_batch_results(self.dialect.as_ref(), &text)?);
            }
        }
        Ok(items)
    }

    async fn cancel(&self, id: &BatchJobId, ctx: &Caller) -> Result<(), ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let mut headers = self.adapter.build_headers(&auth)?;
        insert_extra_headers(self.adapter.extras(), self.http.env.as_ref(), &mut headers)
            .map_err(ProviderError::ExtraHeader)?;
        let url = self
            .adapter
            .batches_url(&format!("/{}/cancel", id.as_str()))?;
        let resp = self
            .http
            .client
            .post(url)
            .headers(headers)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            let text = truncate_utf8(&body, ERROR_BODY_CAP_BYTES);
            return Err(self.adapter.classify_error(status, &h, text));
        }
        Ok(())
    }
}

impl OpenAiProvider {
    /// Returns the content of the file `file_id`.
    async fn file_content(&self, file_id: &str, ctx: &Caller) -> Result<String, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let mut headers = openai_auth_only_headers(&auth)?;
        insert_extra_headers(self.adapter.extras(), self.http.env.as_ref(), &mut headers)
            .map_err(ProviderError::ExtraHeader)?;
        let url = self.adapter.files_url(&format!("/{file_id}/content"))?;
        let resp = self
            .http
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            let text = truncate_utf8(&body, ERROR_BODY_CAP_BYTES);
            return Err(self.adapter.classify_error(status, &h, text));
        }
        resp.text().await.map_err(ProviderError::from)
    }

    async fn fetch_batch_object(
        &self,
        id: &BatchJobId,
        ctx: &Caller,
    ) -> Result<Value, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let mut headers = self.adapter.build_headers(&auth)?;
        insert_extra_headers(self.adapter.extras(), self.http.env.as_ref(), &mut headers)
            .map_err(ProviderError::ExtraHeader)?;
        let url = self.adapter.batches_url(&format!("/{}", id.as_str()))?;
        let resp = self
            .http
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let h = resp.headers().clone();
            let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
            let text = truncate_utf8(&body, ERROR_BODY_CAP_BYTES);
            return Err(self.adapter.classify_error(status, &h, text));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| ProviderError::Parse {
                at: "openai batch fetch response",
                line: None,
                problem: Box::new(ParseProblem::JsonBody(e)),
                raw: None,
            })
    }
}

#[cfg(test)]
mod dialect_choice_tests {
    use super::*;

    #[test]
    fn a_compat_url_names_deepseek_openai_or_another_server() {
        assert_eq!(
            CompatServer::at("https://api.deepseek.com/v1"),
            CompatServer::DeepSeek
        );
        assert_eq!(
            CompatServer::at("https://api.openai.com/v1"),
            CompatServer::OpenAi
        );
        for azure in [
            "https://x.openai.azure.com/openai",
            "https://x.cognitiveservices.azure.com/openai",
            "https://x.services.ai.azure.com/models",
        ] {
            assert_eq!(CompatServer::at(azure), CompatServer::OpenAi, "{azure}");
        }
        assert_eq!(
            CompatServer::at("http://127.0.0.1:8000/v1"),
            CompatServer::Other
        );
        assert_eq!(
            CompatServer::at("https://llm-gateway.corp.example/openai/v1"),
            CompatServer::Other
        );
        assert!(CompatServer::DeepSeek.dialect().text_is_only_whole());
        assert!(!CompatServer::OpenAi.reads_chat_template());
        assert!(CompatServer::Other.reads_chat_template());
    }

    #[test]
    fn a_self_hosted_server_whose_url_mentions_deepseek_is_not_deepseeks_api() {
        for url in [
            "http://deepseek-r1.gpu.internal:8000/v1",
            "http://10.0.0.5:8000/deepseek/v1",
            "https://notdeepseek.com/v1",
        ] {
            assert_eq!(CompatServer::at(url), CompatServer::Other, "{url}");
        }
        assert_eq!(
            CompatServer::at("https://deepseek.com/v1"),
            CompatServer::DeepSeek
        );
    }

    #[test]
    fn a_hosted_vendor_api_reads_no_chat_template() {
        for url in [
            "https://api.groq.com/openai/v1",
            "https://openrouter.ai/api/v1",
            "https://api.x.ai/v1",
        ] {
            let server = CompatServer::at(url);
            assert_eq!(server, CompatServer::HostedApi, "{url}");
            assert!(!server.reads_chat_template());
        }
    }

    #[test]
    fn an_openai_provider_behind_a_gateway_is_not_sent_chat_template_kwargs() {
        let http = HttpClient::default_arc().unwrap();
        let p = OpenAiProviderBuilder::new(ProviderId::new("openai"), Auth::None)
            .base_url("https://llm-gateway.corp.example/openai/v1".to_string())
            .build(http, crate::provider::auth::basic());
        let mut req = ChatRequest::user("hi");
        req.enable_chat_template_thinking = Some(false);
        let body = p.adapter.build_request_default(&req, "gpt-4o").unwrap();
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
    }
}

#[cfg(test)]
mod dialect_seam_tests {
    use super::*;
    use tars_types::Message;

    #[test]
    fn provider_defaults_to_standard_dialect_and_routes_through_it() {
        let http = HttpClient::default_arc().expect("failed to create default HTTP provider base");
        let provider = OpenAiProviderBuilder::new(ProviderId::new("openai"), Auth::None)
            .build(http, crate::provider::auth::basic());

        let req = ChatRequest {
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: None,
            enable_chat_template_thinking: None,
        };

        let via_dialect = provider.adapter.translate_request(&req, "gpt-4o").unwrap();
        let direct = provider
            .adapter
            .build_request_default(&req, "gpt-4o")
            .unwrap();
        assert_eq!(
            via_dialect, direct,
            "default dialect must produce the standard body byte-for-byte",
        );
        assert_eq!(via_dialect["model"], "gpt-4o");
        assert_eq!(via_dialect["stream"], true);
    }

    fn thinking_req(t: tars_types::ThinkingMode) -> ChatRequest {
        ChatRequest {
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: Some(t),
            enable_chat_template_thinking: None,
        }
    }

    #[test]
    fn the_deepseek_dialect_emits_thinking() {
        use tars_types::ThinkingMode;
        let http = HttpClient::default_arc().expect("http base");
        let provider = OpenAiProviderBuilder::new(ProviderId::new("deepseek"), Auth::None)
            .base_url("https://api.deepseek.com")
            .dialect(CompatServer::at("https://api.deepseek.com").dialect())
            .build(http, crate::provider::auth::basic());

        let auto = provider
            .adapter
            .translate_request(&thinking_req(ThinkingMode::Auto), "gpt-4o")
            .unwrap();
        assert_eq!(auto["thinking"]["type"], "enabled");

        let off = provider
            .adapter
            .translate_request(&thinking_req(ThinkingMode::Off), "gpt-4o")
            .unwrap();
        assert_eq!(off["thinking"]["type"], "disabled");
    }

    /// OpenAI itself rejects a `thinking` field.
    #[test]
    fn non_deepseek_base_url_emits_no_thinking() {
        use tars_types::ThinkingMode;
        let http = HttpClient::default_arc().expect("http base");
        let provider = OpenAiProviderBuilder::new(ProviderId::new("openai"), Auth::None)
            .build(http, crate::provider::auth::basic());

        let body = provider
            .adapter
            .translate_request(&thinking_req(ThinkingMode::Auto), "gpt-4o")
            .unwrap();
        assert!(body.get("thinking").is_none());
    }
}

#[cfg(test)]
mod whole_text_stream_tests {
    use super::*;
    use futures::StreamExt as _;
    use tars_types::{ChatEvent, ChatResponse, ChatResponseBuilder, StopReason, Usage};

    /// One DeepSeek answer as it arrives: chunks split inside the markers, so no single chunk
    /// shows a call.
    const ONE_CALL: &[&str] = &[
        "I need the failure shape first.\n\n<｜｜D",
        "SML｜｜tool_calls>\n<｜｜DSML｜｜inv",
        "oke name=\"fs_read\">\n<｜｜DSML｜｜parameter name=\"pa",
        "th\" string=\"true\">crates/tars-git/src/repo.rs</｜｜DSML｜",
        "｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>",
    ];

    /// Two calls in one answer, split the same way.
    const TWO_CALLS: &[&str] = &[
        "Two things.\n\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"fs_",
        "read\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">a.rs</｜｜DSML｜｜parameter>\n",
        "</｜｜DSML｜｜invoke>\n<｜｜DSML｜｜invoke name=\"fs_gr",
        "ep\">\n<｜｜DSML｜｜parameter name=\"pattern\" string=\"true\">struct Reason</｜",
        "｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>",
    ];

    fn deltas(chunks: &[&str]) -> Vec<Result<ChatEvent, ProviderError>> {
        chunks
            .iter()
            .map(|c| {
                Ok(ChatEvent::Delta {
                    text: (*c).to_string(),
                })
            })
            .collect()
    }

    fn finished() -> Result<ChatEvent, ProviderError> {
        Ok(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::default()),
        })
    }

    async fn drive(
        events: Vec<Result<ChatEvent, ProviderError>>,
        dialect: Arc<dyn OpenAiDialect>,
    ) -> Vec<Result<ChatEvent, ProviderError>> {
        let inner: LlmEventStream = Box::pin(futures::stream::iter(events));
        whole_text(inner, dialect).collect().await
    }

    /// Assembles the stream as every consumer does, through `ChatResponseBuilder`.
    fn assemble(events: Vec<Result<ChatEvent, ProviderError>>) -> ChatResponse {
        let mut b = ChatResponseBuilder::new();
        for e in events {
            b.apply(e.expect("this stream carries no error")).unwrap();
        }
        b.finish().unwrap()
    }

    #[tokio::test]
    async fn text_held_when_the_body_ends_without_finished_is_still_sent() {
        let mut events = vec![Ok(ChatEvent::started("deepseek-chat"))];
        events.extend(deltas(&["hello ", "world"]));
        let out = drive(events, Arc::new(DeepSeekDialect)).await;
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                Ok(ChatEvent::Delta { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello world", "{out:?}");
    }

    #[tokio::test]
    async fn text_held_when_an_error_ends_the_stream_is_sent_before_the_error() {
        let mut events = deltas(&["partial"]);
        events.push(Err(ProviderError::Internal("cut".into())));
        let out = drive(events, Arc::new(DeepSeekDialect)).await;
        assert!(
            matches!(&out[0], Ok(ChatEvent::Delta { text }) if text == "partial"),
            "{out:?}"
        );
        assert!(out[1].is_err(), "{out:?}");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn no_single_chunk_carries_a_call() {
        for chunk in ONE_CALL {
            let (mut text, mut tool_calls) = ((*chunk).to_string(), Vec::new());
            DeepSeekDialect.finalize(&mut text, &mut tool_calls);
            assert!(
                tool_calls.is_empty(),
                "chunk {chunk:?} yielded {:?}",
                tool_calls
            );
        }
    }

    #[tokio::test]
    async fn a_call_split_across_chunks_arrives_as_a_tool_call() {
        let mut events = deltas(ONE_CALL);
        events.push(finished());
        let r = assemble(drive(events, Arc::new(DeepSeekDialect)).await);

        assert_eq!(r.tool_calls.len(), 1, "{:?}", r.tool_calls);
        assert_eq!(r.tool_calls[0].name, "fs_read");
        assert_eq!(
            r.tool_calls[0].arguments["path"],
            "crates/tars-git/src/repo.rs"
        );
        assert!(
            !r.text.contains("DSML"),
            "markup reached the consumer: {:?}",
            r.text
        );
        assert!(
            r.text.contains("I need the failure shape first."),
            "the words the model said did not survive: {:?}",
            r.text
        );
    }

    /// The builder pairs a call's start and end by index, so a shared index would give the
    /// second call's arguments the first call's name.
    #[tokio::test]
    async fn two_calls_in_one_answer_get_distinct_indexes() {
        let mut events = deltas(TWO_CALLS);
        events.push(finished());
        let out = drive(events, Arc::new(DeepSeekDialect)).await;

        let starts: Vec<(usize, String)> = out
            .iter()
            .filter_map(|e| match e {
                Ok(ChatEvent::ToolCallStart { index, name, .. }) => Some((*index, name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![(0, "fs_read".to_string()), (1, "fs_grep".to_string())],
            "starts: {starts:?}"
        );

        let r = assemble(out);
        assert_eq!(r.tool_calls.len(), 2, "{:?}", r.tool_calls);
        assert_eq!(r.tool_calls[0].arguments["path"], "a.rs");
        assert_eq!(r.tool_calls[1].arguments["pattern"], "struct Reason");
    }

    #[tokio::test]
    async fn a_streaming_dialect_passes_its_deltas_through_in_order() {
        assert!(!StandardDialect.text_is_only_whole());
        let mut events = deltas(&["one ", "two ", "three"]);
        events.push(finished());
        let out = drive(events, Arc::new(StandardDialect)).await;

        let texts: Vec<String> = out
            .iter()
            .filter_map(|e| match e {
                Ok(ChatEvent::Delta { text }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["one ", "two ", "three"]);
        assert_eq!(out.len(), 4, "an event was added or dropped");
        assert!(matches!(out[3], Ok(ChatEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn an_ordinary_deepseek_answer_survives_intact() {
        let mut events = deltas(&["Let me read ", "the file ", "first."]);
        events.push(finished());
        let r = assemble(drive(events, Arc::new(DeepSeekDialect)).await);

        assert_eq!(r.text, "Let me read the file first.");
        assert!(r.tool_calls.is_empty(), "{:?}", r.tool_calls);
        assert_eq!(r.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn an_error_mid_stream_propagates() {
        let events = vec![
            Ok(ChatEvent::Delta {
                text: "I need the".to_string(),
            }),
            Err(ProviderError::Parse {
                at: "a test stream",
                line: None,
                problem: Box::new(crate::provider::error::ParseProblem::Missing("delta")),
                raw: None,
            }),
            finished(),
        ];
        let out = drive(events, Arc::new(DeepSeekDialect)).await;

        let errs: Vec<String> = out
            .iter()
            .filter_map(|e| e.as_ref().err().map(|e| e.to_string()))
            .collect();
        assert_eq!(
            errs,
            vec!["parse: a test stream".to_string()],
            "out: {out:?}"
        );
    }

    /// A call the model never finished asking for is worse than none.
    #[tokio::test]
    async fn a_truncated_call_invents_nothing() {
        let mut events = deltas(&[
            "thinking\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"fs_gr",
            "ep\">\n<｜｜DSML｜｜parameter name=\"pattern\" string=\"true\">abc",
        ]);
        events.push(finished());
        let r = assemble(drive(events, Arc::new(DeepSeekDialect)).await);

        assert!(r.tool_calls.is_empty(), "{:?}", r.tool_calls);
        assert!(
            !r.text.contains("DSML"),
            "markup reached the consumer: {:?}",
            r.text
        );
    }
}
