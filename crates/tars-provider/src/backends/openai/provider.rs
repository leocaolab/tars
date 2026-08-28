//! `OpenAiProvider`, its builder, and the `LlmProvider` impl.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use tars_types::{
    BatchItemId, BatchJobId, BatchResultItem, BatchStatus, ChatRequest, ProviderError, ProviderId,
    ProviderProfile, RequestContext,
};

use crate::auth::{Auth, AuthResolver};
use crate::batch::BatchSubmitter;
use crate::http_base::{
    ERROR_BODY_CAP_BYTES, HttpAdapter, HttpProviderBase, HttpProviderExtras, read_bounded_body,
    stream_via_adapter, truncate_utf8,
};
use crate::provider::{LlmEventStream, LlmProvider};

use super::adapter::OpenAiAdapter;
use super::dialect::{DeepSeekDialect, OpenAiDialect, StandardDialect};
use super::mapping::{
    openai_auth_only_headers, parse_openai_batch_results, translate_openai_batch_status,
};

/// Default OpenAI base URL.
pub(super) const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Clone)]
pub struct OpenAiProviderBuilder {
    id: ProviderId,
    base_url: String,
    auth: Auth,
    capabilities: Option<ProviderProfile>,
    extras: HttpProviderExtras,
    /// The behavior seam. `None` = infer from `base_url` at `build()`; set
    /// explicitly via [`OpenAiProviderBuilder::dialect`] to override.
    dialect: Option<Arc<dyn OpenAiDialect>>,
}

impl OpenAiProviderBuilder {
    pub fn new(id: impl Into<ProviderId>, auth: Auth) -> Self {
        Self {
            id: id.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth,
            capabilities: None,
            extras: HttpProviderExtras::default(),
            dialect: None,
        }
    }

    builder_setter! {
        /// Override the OpenAI dialect (per-variant wire behavior). When unset,
        /// `build()` infers it from `base_url` (a `deepseek` host →
        /// [`DeepSeekDialect`], else [`StandardDialect`]).
        dialect: opt Arc<dyn OpenAiDialect>
    }

    builder_setter! {
        /// Override base URL — for vLLM / llama.cpp / Groq / etc.
        base_url: into String
    }

    builder_setter! {
        /// Override capability descriptor. Default is a vanilla
        /// GPT-4o-style profile; OpenAI-compatible local backends
        /// should set their own.
        capabilities: opt ProviderProfile
    }

    builder_setter! {
        ///
        extras: HttpProviderExtras
    }

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<OpenAiProvider> {
        let caps = self
            .capabilities
            .unwrap_or_else(default_openai_capabilities);
        // Resolve the behavior seam: an explicit dialect wins; otherwise infer
        // from the endpoint (a `deepseek` host → `DeepSeekDialect`).
        let dialect: Arc<dyn OpenAiDialect> = self.dialect.unwrap_or_else(|| {
            if self.base_url.contains("deepseek") {
                Arc::new(DeepSeekDialect)
            } else {
                Arc::new(StandardDialect)
            }
        });
        let adapter = Arc::new(
            OpenAiAdapter::new(self.base_url, self.extras, caps.supports_structured_output)
                .with_dialect(dialect.clone()),
        );
        Arc::new(OpenAiProvider {
            id: self.id,
            http,
            auth_resolver,
            auth: self.auth,
            adapter,
            capabilities: caps,
            dialect,
        })
    }
}

/// Default OpenAI capabilities, assembled from the provider DB
/// (`data/provider.toml`) for OpenAI's default model. Used as the builder
/// fallback when the registry doesn't pass an explicit descriptor.
pub fn default_openai_capabilities() -> ProviderProfile {
    tars_config::capabilities_for("openai", "")
}

/// The provider itself.
pub struct OpenAiProvider {
    id: ProviderId,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<OpenAiAdapter>,
    capabilities: ProviderProfile,
    /// The behavior seam, shared (same `Arc`) with `adapter` so the
    /// non-streaming batch results path decodes through the same dialect as
    /// streaming.
    dialect: Arc<dyn OpenAiDialect>,
}

/// The stream the agent actually reads, for a dialect whose text means nothing
/// until it is whole.
///
/// A dialect that answers `false` to [`OpenAiDialect::text_is_only_whole`] gets its
/// own stream back untouched — deltas go out as they arrive, which is what streaming
/// is for. One that answers `true` has its text HELD here and read once by the
/// dialect at the end. What comes out the other side is what every consumer already
/// understands: text, and tool calls.
///
/// Not in the consumer. There are three places that assemble this stream —
/// the fiber, the session, and `llm_common` — and none of them has the dialect
/// or any reason to know what DSML is. `finalize` was reachable only through
/// `complete()`, which none of them calls.
///
/// Free function, not a closure inside `stream()`, so a test can drive it with a
/// hand-built stream and no HTTP: the defect this fixes shipped twice because the
/// only path with no test was the only path the agent takes.
fn whole_text(inner: LlmEventStream, dialect: Arc<dyn OpenAiDialect>) -> LlmEventStream {
    if !dialect.text_is_only_whole() {
        return inner;
    }
    let out = async_stream::stream! {
        use futures::StreamExt as _;
        let mut inner = inner;
        let mut held = String::new();
        while let Some(ev) = inner.next().await {
            let ev = match ev {
                Ok(ev) => ev,
                Err(e) => { yield Err(e); return; }
            };
            match ev {
                tars_types::ChatEvent::Delta { text } => held.push_str(&text),
                tars_types::ChatEvent::Finished { .. } => {
                    let mut r = tars_types::ChatResponse {
                        text: std::mem::take(&mut held),
                        ..Default::default()
                    };
                    dialect.finalize(&mut r);
                    if !r.text.is_empty() {
                        yield Ok(tars_types::ChatEvent::Delta { text: r.text.clone() });
                    }
                    for (index, c) in r.tool_calls.into_iter().enumerate() {
                        yield Ok(tars_types::ChatEvent::ToolCallStart {
                            index,
                            id: c.id.clone(),
                            name: c.name.clone(),
                        });
                        yield Ok(tars_types::ChatEvent::ToolCallEnd {
                            index,
                            id: c.id,
                            parsed_args: c.arguments,
                            thought_signature: None,
                        });
                    }
                    yield Ok(ev);
                }
                other => yield Ok(other),
            }
        }
    };
    Box::pin(out)
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }

    // Boundary log — any Err exit auto-emits a tracing event with
    // provider/model context (see anthropic.stream for the rationale).
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
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, &ctx).await?;
        let inner = stream_via_adapter(
            self.http.clone(),
            self.adapter.clone(),
            auth,
            req,
            model,
            ctx,
        )
        .await?;
        Ok(whole_text(inner, self.dialect.clone()))
    }

    /// The trait default drives the stream and aggregates; this adds the dialect's
    /// last look at the joined result.
    ///
    /// Without it a dialect can only see events, and some of what a dialect must fix
    /// is not visible in one: DeepSeek's tool-call markup arrives split across chunks
    /// and is only recognisable once the text is whole again. `parse_response` covers
    /// the batch path, real runs stream, and for eighteen consecutive turns of one run
    /// the markup went through untouched and the turns did nothing.
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: RequestContext,
    ) -> Result<tars_types::ChatResponse, ProviderError> {
        use futures::StreamExt as _;
        let dialect = self.dialect.clone();
        let mut s = self.stream(req, model, ctx).await?;
        let mut acc = tars_types::ChatResponseBuilder::new();
        while let Some(event) = s.next().await {
            acc.apply(event?);
        }
        let mut r = acc.finish();
        dialect.finalize(&mut r);
        Ok(r)
    }

    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn BatchSubmitter>> {
        Some(self)
    }
}

// ─── BatchSubmitter — OpenAI Batch API ──────────────────────────────
//
// Reference: <https://platform.openai.com/docs/api-reference/batch>
//
// Two-step submission (different from Anthropic's one-step):
//   1) POST /files  (multipart, purpose=batch) → file_id
//   2) POST /batches { input_file_id, endpoint, completion_window } → job
//
// Results come back as a separate output file (output_file_id on the
// batch object); fetch via GET /files/{id}/content. Errors during the
// batch surface in an error_file_id similarly.
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
        ctx: &RequestContext,
    ) -> Result<BatchJobId, ProviderError> {
        if items.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "batch submit: items list must not be empty".into(),
            ));
        }

        // 1) Build the JSONL input file content.
        let mut jsonl = String::with_capacity(items.len() * 256);
        for (item_id, req) in &items {
            let body = self.adapter.translate_request(req, model)?;
            let line = serde_json::to_string(&json!({
                "custom_id": item_id.as_str(),
                "method": "POST",
                "url": "/v1/chat/completions",
                "body": body,
            }))
            .map_err(|e| ProviderError::Internal(format!("batch input serialize: {e}")))?;
            jsonl.push_str(&line);
            jsonl.push('\n');
        }

        // 2) Upload the JSONL as a "batch" purpose file via multipart.
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let auth_only = openai_auth_only_headers(&auth)?;

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
            .headers(auth_only.clone())
            .multipart(form)
            .send()
            .await
            .map_err(ProviderError::from)?;
        if !upload_resp.status().is_success() {
            let status = upload_resp.status();
            let h = upload_resp.headers().clone();
            // Bounded read: a hostile / partial error body must not let
            // `.text()` buffer unboundedly. Mirrors the streaming path.
            let body = read_bounded_body(upload_resp, ERROR_BODY_CAP_BYTES).await;
            let text = truncate_utf8(&body, ERROR_BODY_CAP_BYTES);
            return Err(self.adapter.classify_error(status, &h, text));
        }
        let file_v: Value = upload_resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("file upload: response not JSON: {e}")))?;
        let input_file_id = file_v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Parse("file upload: response missing `id`".into()))?
            .to_string();

        // 3) Create the batch referencing that file.
        //
        // KNOWN LEAK (info): if the batch-create POST below fails (bad
        // `batches_url`, network error, non-2xx), the file we just
        // uploaded stays on OpenAI as an orphaned `purpose=batch` file
        // and counts against the account's storage quota until manually
        // deleted. We deliberately do *not* fire a best-effort
        // DELETE /files/{id} here: it would need its own error handling,
        // could itself fail/hang, and the leaked artifact is small and
        // GC-able by the user. Revisit if quota pressure shows up.
        let create_url = self.adapter.batches_url("")?;
        let headers = self.adapter.build_headers(&auth)?; // JSON content-type
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
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("batch create: response not JSON: {e}")))?;
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Parse("batch create: response missing `id`".into()))?;
        Ok(BatchJobId::new(id))
    }

    async fn status(
        &self,
        id: &BatchJobId,
        ctx: &RequestContext,
    ) -> Result<BatchStatus, ProviderError> {
        let v = self.fetch_batch_object(id, ctx).await?;
        translate_openai_batch_status(&v)
    }

    async fn results(
        &self,
        id: &BatchJobId,
        ctx: &RequestContext,
    ) -> Result<Vec<BatchResultItem>, ProviderError> {
        let v = self.fetch_batch_object(id, ctx).await?;
        let status = translate_openai_batch_status(&v)?;
        if !status.is_terminal() {
            return Err(ProviderError::InvalidRequest(format!(
                "batch results: job {id} is not yet terminal (status: {status:?})"
            )));
        }
        // For Completed: read output_file_id and download it. For
        // Failed/Expired/Cancelled there's typically no output file
        // (errors live in error_file_id which we currently surface as
        // an Err on each item-less response). Return empty for now;
        // callers should branch on status() before results().
        let output_file_id = v.get("output_file_id").and_then(|s| s.as_str());
        let Some(output_file_id) = output_file_id else {
            return Ok(Vec::new());
        };

        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let headers = openai_auth_only_headers(&auth)?;
        let url = self
            .adapter
            .files_url(&format!("/{output_file_id}/content"))?;
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
        let text = resp.text().await.map_err(ProviderError::from)?;
        parse_openai_batch_results(self.dialect.as_ref(), &text)
    }

    async fn cancel(&self, id: &BatchJobId, ctx: &RequestContext) -> Result<(), ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let headers = self.adapter.build_headers(&auth)?;
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
    ///
    async fn fetch_batch_object(
        &self,
        id: &BatchJobId,
        ctx: &RequestContext,
    ) -> Result<Value, ProviderError> {
        let auth = self.auth_resolver.resolve(&self.auth, ctx).await?;
        let headers = self.adapter.build_headers(&auth)?;
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
            .map_err(|e| ProviderError::Parse(format!("batch fetch: response not JSON: {e}")))
    }
}

#[cfg(test)]
mod dialect_seam_tests {
    use super::*;
    use tars_types::Message;

    /// A provider built without an explicit dialect defaults to
    /// `StandardDialect`, and the public request path routes THROUGH it. The
    /// dialect-routed body must be byte-identical to the adapter's own
    /// standard default body — proving the seam is live and behavior-neutral.
    #[test]
    fn provider_defaults_to_standard_dialect_and_routes_through_it() {
        let http =
            HttpProviderBase::default_arc().expect("failed to create default HTTP provider base");
        let provider =
            OpenAiProviderBuilder::new("openai", Auth::None).build(http, crate::auth::basic());

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
            thinking: Default::default(),
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
            thinking: t,
            enable_chat_template_thinking: None,
        }
    }

    /// A provider built from a `deepseek` base_url with no explicit dialect
    /// infers `DeepSeekDialect`, so `req.thinking` maps to the top-level
    /// `thinking: {type}` field.
    #[test]
    fn deepseek_base_url_infers_dialect_and_emits_thinking() {
        use tars_types::ThinkingMode;
        let http = HttpProviderBase::default_arc().expect("http base");
        let provider = OpenAiProviderBuilder::new("deepseek", Auth::None)
            .base_url("https://api.deepseek.com")
            .build(http, crate::auth::basic());

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

    /// A non-DeepSeek endpoint infers `StandardDialect` → no `thinking` field
    /// leaks (would break OpenAI proper).
    #[test]
    fn non_deepseek_base_url_emits_no_thinking() {
        use tars_types::ThinkingMode;
        let http = HttpProviderBase::default_arc().expect("http base");
        let provider =
            OpenAiProviderBuilder::new("openai", Auth::None).build(http, crate::auth::basic());

        let body = provider
            .adapter
            .translate_request(&thinking_req(ThinkingMode::Auto), "gpt-4o")
            .unwrap();
        assert!(body.get("thinking").is_none());
    }
}

/// The STREAM, driven directly.
///
/// `finalize` had unit tests and the batch path had a test; `stream()` — the only
/// path the agent takes — had none, which is why the DSML lift shipped twice on a
/// path that never ran. These drive [`whole_text`] with hand-built events, so the
/// thing under test is the wrapper the agent's stream goes through.
#[cfg(test)]
mod whole_text_stream_tests {
    use super::*;
    use futures::StreamExt as _;
    use tars_types::{ChatEvent, ChatResponse, ChatResponseBuilder, StopReason, Usage};

    /// One DeepSeek answer as it comes off the WIRE: the markup arrives in chunks and
    /// the splits fall inside the markers — `<｜｜D` ends one chunk and `SML｜｜` opens
    /// the next. This is the reason `finalize` alone was not enough: nothing per-event
    /// can see a call here.
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
            usage: Usage::default(),
        })
    }

    /// Drive the wrapper with a hand-built stream — no HTTP, no adapter.
    async fn drive(
        events: Vec<Result<ChatEvent, ProviderError>>,
        dialect: Arc<dyn OpenAiDialect>,
    ) -> Vec<Result<ChatEvent, ProviderError>> {
        let inner: LlmEventStream = Box::pin(futures::stream::iter(events));
        whole_text(inner, dialect).collect().await
    }

    /// What a consumer does with the stream: the fiber, the session and `llm_common`
    /// each assemble it themselves, all of them through this builder.
    fn assemble(events: Vec<Result<ChatEvent, ProviderError>>) -> ChatResponse {
        let mut b = ChatResponseBuilder::new();
        for e in events {
            b.apply(e.expect("this stream carries no error"));
        }
        b.finish()
    }

    /// No single chunk is a call. The dialect reading any one of them sees nothing —
    /// which is the whole reason the text has to be held until it is whole.
    #[test]
    fn no_single_chunk_carries_a_call() {
        for chunk in ONE_CALL {
            let mut r = ChatResponse {
                text: (*chunk).to_string(),
                ..Default::default()
            };
            DeepSeekDialect.finalize(&mut r);
            assert!(
                r.tool_calls.is_empty(),
                "chunk {chunk:?} yielded {:?}",
                r.tool_calls
            );
        }
    }

    /// The measured defect, at the layer it was measured in. A 37-turn run had every
    /// answer from turn 7 on arrive as this and every `tool_calls` empty.
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

    /// Two calls in one answer are two calls, on distinct indexes. The builder keys
    /// its Start/End correlation by index, so a shared index would pair the second
    /// call's arguments with the first call's name.
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

    /// A dialect whose text is readable as it arrives is not touched: its deltas go
    /// out one per chunk, in order. Every other OpenAI-compatible endpoint is this
    /// one, and holding their text would turn streaming into batching.
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

    /// An ordinary DeepSeek answer — no markup — keeps its text and gains no calls.
    /// This runs on every DeepSeek response, so it must change nothing when there is
    /// nothing to lift.
    #[tokio::test]
    async fn an_ordinary_deepseek_answer_survives_intact() {
        let mut events = deltas(&["Let me read ", "the file ", "first."]);
        events.push(finished());
        let r = assemble(drive(events, Arc::new(DeepSeekDialect)).await);

        assert_eq!(r.text, "Let me read the file first.");
        assert!(r.tool_calls.is_empty(), "{:?}", r.tool_calls);
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
    }

    /// An error mid-stream reaches the consumer. The wrapper sits between the
    /// provider and everything above it; swallowing the error here would leave the
    /// caller with a stream that ended for no stated reason.
    #[tokio::test]
    async fn an_error_mid_stream_propagates() {
        let events = vec![
            Ok(ChatEvent::Delta {
                text: "I need the".to_string(),
            }),
            Err(ProviderError::Parse("connection reset mid-answer".into())),
            finished(),
        ];
        let out = drive(events, Arc::new(DeepSeekDialect)).await;

        let errs: Vec<String> = out
            .iter()
            .filter_map(|e| e.as_ref().err().map(|e| e.to_string()))
            .collect();
        assert_eq!(
            errs,
            vec!["parse: connection reset mid-answer".to_string()],
            "out: {out:?}"
        );
    }

    /// An answer cut off mid-markup does not panic and does not invent a call. A
    /// response truncated mid-tag happens, and turning it into a call the model never
    /// finished asking for would be worse than dropping it.
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
