//! `OpenAiProvider`, its builder, the `LlmProvider` impl, and the
//! `BatchSubmitter` implementation (OpenAI Batch API). The default
//! capability descriptor lives here too — it's only consumed by the
//! builder fallback, so colocating it avoids a one-item module.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use tars_types::{
    BatchItemId, BatchJobId, BatchResultItem, BatchStatus, Capabilities, ChatRequest,
    ProviderError, ProviderId, RequestContext,
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
    capabilities: Option<Capabilities>,
    extras: HttpProviderExtras,
    /// The behavior seam (Doc 30). `None` = no explicit dialect; `build()` then
    /// infers one from `base_url` (a `deepseek` host → [`DeepSeekDialect`],
    /// else [`StandardDialect`]) so today's base_url-gated behavior is
    /// preserved byte-for-byte without a config-schema change. Set explicitly
    /// via [`OpenAiProviderBuilder::dialect`] to override the inference.
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
        capabilities: opt Capabilities
    }

    builder_setter! {
        /// Attach user-config-supplied http_headers / env_http_headers /
        /// query_params (Doc 01 §6.1).
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
        // Resolve the behavior seam. An explicit dialect wins; otherwise infer
        // from the endpoint. This preserves today's base_url-gated DeepSeek
        // `thinking` behavior byte-for-byte (the quirk moved into the dialect)
        // with no config-schema change. An explicit `dialect` config field is a
        // later, cleaner step (Doc 30 M3).
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
/// (`data/provider.toml`) for OpenAI's default model — no longer a
/// hand-written literal. Used as the builder fallback when the registry
/// doesn't pass an explicit descriptor.
pub fn default_openai_capabilities() -> Capabilities {
    tars_config::capabilities_for("openai", "")
}

/// The provider itself. Cheap to clone (`Arc` everywhere), so the
/// `Arc<Self>` requirement of [`LlmProvider`] is trivial.
pub struct OpenAiProvider {
    id: ProviderId,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
    auth: Auth,
    adapter: Arc<OpenAiAdapter>,
    capabilities: Capabilities,
    /// The behavior seam (Doc 30). Held here so the non-streaming batch
    /// results path decodes through the same dialect as streaming. Defaults
    /// to [`StandardDialect`] and is shared (same `Arc`) with `adapter`.
    dialect: Arc<dyn OpenAiDialect>,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
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
        stream_via_adapter(self.http.clone(), self.adapter.clone(), auth, req, model, ctx).await
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
    /// Shared GET that pulls the batch object JSON for status / results.
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

    /// M0 seam: a provider built without an explicit dialect defaults to
    /// `StandardDialect`, and the public request path routes THROUGH it. The
    /// dialect-routed body must be byte-identical to the adapter's own
    /// standard default body — proving the seam is live and behavior-neutral.
    #[test]
    fn provider_defaults_to_standard_dialect_and_routes_through_it() {
        let http =
            HttpProviderBase::default_arc().expect("failed to create default HTTP provider base");
        let provider = OpenAiProviderBuilder::new("openai", Auth::None)
            .build(http, crate::auth::basic());

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
        let direct = provider.adapter.build_request_default(&req, "gpt-4o").unwrap();
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

    /// Behavior-preservation (M1): a provider built from a `deepseek` base_url
    /// with no explicit dialect infers `DeepSeekDialect`, so `req.thinking`
    /// still maps to the top-level `thinking: {type}` field EXACTLY as the old
    /// base_url-gated adapter branch did.
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
        let provider = OpenAiProviderBuilder::new("openai", Auth::None)
            .build(http, crate::auth::basic());

        let body = provider
            .adapter
            .translate_request(&thinking_req(ThinkingMode::Auto), "gpt-4o")
            .unwrap();
        assert!(body.get("thinking").is_none());
    }
}
