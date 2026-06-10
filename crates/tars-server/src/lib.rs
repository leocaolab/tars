//! Personal-mode HTTP/REST server over the tars pipeline.
//!
//! A thin axum shell that exposes the **already-built** `Pipeline`
//! (cache / retry / telemetry / routing) over HTTP so tars is curl-able.
//! This is the M6 server *subset* with the multi-tenant security stack
//! deliberately left out: **no auth, no IAM, no per-tenant isolation**.
//! Because anyone who can reach the socket can drive the model, the
//! server binds loopback by default and warns loudly on a non-loopback
//! bind. A real deployment needs the M6 `tars-security` integration.
//!
//! Endpoints:
//! - `GET  /healthz`              — liveness
//! - `GET  /v1/providers`         — configured providers + default
//! - `POST /v1/complete`          — synchronous completion (JSON)
//! - `POST /v1/complete/stream`   — streaming completion (SSE)

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use tars_pipeline::{LlmService, Pipeline, PipelineOpts};
use tars_provider::registry::ProviderRegistry;
use tars_provider::{auth::basic, http_base::HttpProviderBase};
use tars_types::{
    ChatEvent, ChatRequest, ChatResponseBuilder, ContentBlock, Message, ModelHint, ProviderError,
    ProviderId, RequestContext, StopReason, TraceId, Usage,
};

/// One configured provider, wrapped in the canonical pipeline.
struct ProviderEntry {
    pipeline: Arc<dyn LlmService>,
    /// `default_model` from config — used when a request omits `model`.
    default_model: Option<String>,
}

/// Shared server state: one pipeline per configured provider.
pub struct AppState {
    providers: HashMap<String, ProviderEntry>,
    /// The provider used when a request omits `provider` — set iff
    /// exactly one is configured.
    default_provider: Option<String>,
}

impl AppState {
    /// Build a pipeline for every provider in `config`. `default_provider`
    /// (e.g. from `--default-provider`) is the provider used when a
    /// request omits `provider`; if `None`, the single configured
    /// provider is the default — but note config always carries the
    /// built-in provider table, so in practice a request must name its
    /// provider unless a default is set. Errors on an empty config or an
    /// unknown default.
    pub fn from_config(
        config: &tars_config::Config,
        default_provider: Option<String>,
    ) -> anyhow::Result<Arc<Self>> {
        let http = HttpProviderBase::default_arc()
            .map_err(|e| anyhow::anyhow!("building HTTP base: {e}"))?;
        let registry = ProviderRegistry::from_config(&config.providers, http, basic())
            .map_err(|e| anyhow::anyhow!("building provider registry: {e}"))?;

        let mut providers = HashMap::new();
        for (id, _) in config.providers.iter() {
            let id = id.to_string();
            let pid = ProviderId::new(id.clone());
            let provider = registry
                .get(&pid)
                .ok_or_else(|| anyhow::anyhow!("provider {id:?} vanished from registry"))?;
            let default_model = registry.default_model(&pid).map(str::to_string);
            let pipeline = Pipeline::default_chain(provider, PipelineOpts::new(pid));
            providers.insert(
                id,
                ProviderEntry {
                    pipeline: Arc::new(pipeline),
                    default_model,
                },
            );
        }

        if providers.is_empty() {
            anyhow::bail!("config has no providers — nothing to serve");
        }
        let default_provider = match default_provider {
            Some(p) if providers.contains_key(&p) => Some(p),
            Some(p) => anyhow::bail!("--default-provider {p:?} is not a configured provider"),
            None => (providers.len() == 1).then(|| providers.keys().next().unwrap().clone()),
        };

        Ok(Arc::new(Self {
            providers,
            default_provider,
        }))
    }

    /// Resolve the (pipeline, model) a request targets.
    fn resolve(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<(Arc<dyn LlmService>, String), AppError> {
        let pid = match provider.or(self.default_provider.as_deref()) {
            Some(p) => p,
            None => {
                let mut ids: Vec<&str> = self.providers.keys().map(String::as_str).collect();
                ids.sort_unstable();
                return Err(AppError::bad_request(format!(
                    "multiple providers configured ({}); specify `provider`",
                    ids.join(", ")
                )));
            }
        };
        let entry = self
            .providers
            .get(pid)
            .ok_or_else(|| AppError::bad_request(format!("unknown provider {pid:?}")))?;
        let model = model
            .map(str::to_string)
            .or_else(|| entry.default_model.clone())
            .ok_or_else(|| {
                AppError::bad_request(format!(
                    "no model: pass `model`, or set `default_model` on provider {pid:?}"
                ))
            })?;
        Ok((entry.pipeline.clone(), model))
    }

    // ── Public reuse seam (Doc 22: tars-desktop drives this in-process) ──

    /// The pipeline-wrapped [`LlmService`] + resolved model for a provider —
    /// the same resolution the HTTP handlers use, exposed for in-process
    /// consumers (the Tauri desktop GUI) that build a `Session` over it.
    /// `provider`/`model` `None` fall back to the configured defaults.
    pub fn llm_for(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<(Arc<dyn LlmService>, String)> {
        self.resolve(provider, model)
            .map_err(|e| anyhow::anyhow!("{}", e.message))
    }

    /// Sorted ids of the configured providers — for a model/provider picker.
    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.providers.keys().cloned().collect();
        ids.sort_unstable();
        ids
    }

    /// The default provider (set iff exactly one is configured or named at
    /// startup), if any.
    pub fn default_provider(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }

    /// The configured `default_model` for a provider id, if known.
    pub fn default_model_for(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|e| e.default_model.as_deref())
    }
}

/// Build the router for `state`.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/providers", get(list_providers))
        .route("/v1/complete", post(complete))
        .route("/v1/complete/stream", post(complete_stream))
        .with_state(state)
}

// ── DTOs ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CompleteBody {
    /// Provider id. Optional when exactly one provider is configured.
    provider: Option<String>,
    /// Model id. Optional → the provider's `default_model`.
    model: Option<String>,
    system: Option<String>,
    /// Single-turn shorthand. Use `messages` for multi-turn.
    user: Option<String>,
    messages: Option<Vec<MessageIn>>,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct MessageIn {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct CompleteResponse {
    text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    thinking: String,
    stop_reason: Option<StopReason>,
    usage: Usage,
}

impl CompleteBody {
    /// Build a `ChatRequest`, resolving the model string into a hint.
    fn into_chat_request(self, model: String) -> Result<ChatRequest, AppError> {
        let hint = ModelHint::Explicit(model);
        let mut req = if let Some(messages) = self.messages {
            if messages.is_empty() {
                return Err(AppError::bad_request("`messages` must be non-empty"));
            }
            let msgs = messages
                .into_iter()
                .map(|m| match m.role.as_str() {
                    "user" => Ok(Message::User {
                        content: vec![ContentBlock::text(m.content)],
                    }),
                    "assistant" => Ok(Message::Assistant {
                        content: vec![ContentBlock::text(m.content)],
                        tool_calls: Vec::new(),
                    }),
                    "system" => Ok(Message::System {
                        content: vec![ContentBlock::text(m.content)],
                    }),
                    other => Err(AppError::bad_request(format!(
                        "unknown message role {other:?} (use user / assistant / system)"
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            ChatRequest {
                messages: msgs,
                ..ChatRequest::user(hint, "")
            }
        } else if let Some(user) = self.user {
            ChatRequest::user(hint, user)
        } else {
            return Err(AppError::bad_request("provide `user` or `messages`"));
        };
        req.system = self.system;
        req.max_output_tokens = self.max_output_tokens;
        req.temperature = self.temperature;
        Ok(req)
    }
}

// ── Handlers ─────────────────────────────────────────────────────────

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn list_providers(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut ids: Vec<&str> = state.providers.keys().map(String::as_str).collect();
    ids.sort_unstable();
    Json(serde_json::json!({
        "providers": ids,
        "default": state.default_provider,
    }))
}

async fn complete(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CompleteBody>,
) -> Result<Json<CompleteResponse>, AppError> {
    let (pipeline, model) = state.resolve(body.provider.as_deref(), body.model.as_deref())?;
    let req = body.into_chat_request(model)?;
    let ctx = RequestContext::personal(TraceId::new(uuid::Uuid::new_v4().to_string()));

    let mut stream = pipeline.call(req, ctx).await?;
    let mut builder = ChatResponseBuilder::new();
    while let Some(item) = stream.next().await {
        builder.apply(item?);
    }
    let resp = builder.finish();
    Ok(Json(CompleteResponse {
        text: resp.text,
        thinking: resp.thinking,
        stop_reason: resp.stop_reason,
        usage: resp.usage,
    }))
}

async fn complete_stream(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CompleteBody>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>, AppError> {
    let (pipeline, model) = state.resolve(body.provider.as_deref(), body.model.as_deref())?;
    let req = body.into_chat_request(model)?;
    let ctx = RequestContext::personal(TraceId::new(uuid::Uuid::new_v4().to_string()));

    // `call()` resolving to Err is a pre-stream failure → real HTTP error.
    let stream = pipeline.call(req, ctx).await?;

    let sse = stream.filter_map(|item| async move {
        let event = match item {
            Ok(ChatEvent::Delta { text }) => sse_json("delta", serde_json::json!({ "text": text })),
            Ok(ChatEvent::ThinkingDelta { text }) => {
                sse_json("thinking", serde_json::json!({ "text": text }))
            }
            Ok(ChatEvent::Finished { stop_reason, usage }) => sse_json(
                "done",
                serde_json::json!({ "stop_reason": stop_reason, "usage": usage }),
            ),
            // Other events (Started / tool-call / usage-progress) are not
            // surfaced in this minimal stream.
            Ok(_) => return None,
            Err(e) => sse_json(
                "error",
                serde_json::json!({ "kind": e.kind().as_str(), "message": e.to_string() }),
            ),
        };
        Some(Ok(event))
    });
    Ok(Sse::new(sse).keep_alive(KeepAlive::default()))
}

/// Build an SSE event with a named type and a JSON payload. Falls back to
/// a comment if serialization somehow fails (it won't for these shapes).
fn sse_json(name: &str, payload: serde_json::Value) -> Event {
    match Event::default().event(name).json_data(payload) {
        Ok(ev) => ev,
        Err(_) => Event::default().comment("serialize_failed"),
    }
}

// ── Errors ───────────────────────────────────────────────────────────

/// HTTP error with a JSON body `{ "error": { kind, message } }`.
pub struct AppError {
    status: StatusCode,
    kind: String,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "bad_request".into(),
            message: message.into(),
        }
    }
}

impl From<ProviderError> for AppError {
    fn from(e: ProviderError) -> Self {
        use tars_types::error::ErrorClass;
        let status = match e.class() {
            // Permanent client-ish errors → 4xx where we can tell, else 502.
            ErrorClass::Permanent => match &e {
                ProviderError::Auth(_) => StatusCode::UNAUTHORIZED,
                ProviderError::InvalidRequest(_)
                | ProviderError::ContextTooLong { .. }
                | ProviderError::UnknownTool { .. }
                | ProviderError::ValidationFailed { .. } => StatusCode::BAD_REQUEST,
                ProviderError::BudgetExceeded => StatusCode::TOO_MANY_REQUESTS,
                _ => StatusCode::BAD_GATEWAY,
            },
            ErrorClass::Retriable => match &e {
                ProviderError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
                _ => StatusCode::BAD_GATEWAY,
            },
            ErrorClass::MaybeRetriable => StatusCode::BAD_GATEWAY,
        };
        Self {
            status,
            kind: e.kind().as_str().to_string(),
            message: e.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": { "kind": self.kind, "message": self.message }
        }));
        (self.status, body).into_response()
    }
}
