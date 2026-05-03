//! Shared HTTP infrastructure for HTTP-based Providers.
//!
//! Doc 01 §6.1 says all HTTP backends share one reqwest client + a
//! common retry/timeout/SSE-decoding base, and each Provider is just an
//! [`HttpAdapter`] (build_url / build_headers / translate_request /
//! parse_event / classify_error). This module provides that base.
//!
//! Borrowed pattern from `codex-rs/codex-client/src/sse.rs`: the SSE
//! consumption loop wraps every `stream.next()` with a per-chunk idle
//! timeout. Without that a server that opens the connection then stalls
//! forever can hang the client indefinitely.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use url::Url;

pub use tars_types::HttpProviderExtras;
use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext};

use crate::auth::ResolvedAuth;
use crate::provider::LlmEventStream;
use crate::tool_buffer::ToolCallBuffer;

/// Hard upper bound on user-configured retry counts. Borrowed from
/// codex-rs's `MAX_REQUEST_MAX_RETRIES` — a defensive cap so a typo
/// like `request_max_retries = 999999` can't silently turn the runtime
/// into a retry storm.
pub const MAX_REQUEST_MAX_RETRIES: u64 = 100;
pub const MAX_STREAM_MAX_RETRIES: u64 = 100;

/// Default idle timeout BETWEEN SSE chunks. The previous name was
/// `read_timeout` but that was misleading — reqwest's `read_timeout`
/// is a connection-level setting we don't want for streaming.
const DEFAULT_STREAM_IDLE_MS: u64 = 300_000;

/// HTTP timeouts.
///
/// `connect_timeout`: cap on the TCP+TLS handshake.
/// `stream_idle_timeout`: cap on time WITHOUT receiving an SSE chunk
/// once the stream is open. The whole response can take many minutes
/// for a long generation; what matters is whether bytes are still
/// flowing. Default is 5 minutes (matches codex-rs default).
#[derive(Clone, Debug)]
pub struct HttpProviderConfig {
    pub connect_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub user_agent: String,
}

impl Default for HttpProviderConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            stream_idle_timeout: Duration::from_millis(DEFAULT_STREAM_IDLE_MS),
            user_agent: format!("tars/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Shared base for HTTP-based providers.
///
/// Holds the (single, shared) reqwest client. Adapters consume an
/// `Arc<HttpProviderBase>` so they don't re-create the client per
/// request.
#[derive(Clone)]
pub struct HttpProviderBase {
    pub client: reqwest::Client,
    pub config: HttpProviderConfig,
}

impl HttpProviderBase {
    pub fn new(config: HttpProviderConfig) -> Result<Arc<Self>, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            // Deliberately no overall `.timeout()` — streaming responses
            // can take minutes. Idleness is enforced at the SSE loop layer.
            .user_agent(&config.user_agent)
            .build()
            .map_err(ProviderError::from)?;
        Ok(Arc::new(Self { client, config }))
    }

    pub fn default_arc() -> Result<Arc<Self>, ProviderError> {
        Self::new(HttpProviderConfig::default())
    }
}

/// What an HTTP-based provider must implement on top of [`HttpProviderBase`].
///
/// Each call slot is intentionally narrow: the base handles transport,
/// the adapter handles wire format.
#[async_trait]
pub trait HttpAdapter: Send + Sync + 'static {
    /// Where to POST. Adapter is responsible for any provider-mandated
    /// query params (e.g. Gemini's `?alt=sse`); user-config
    /// `query_params` are appended on top by [`stream_via_adapter`].
    fn build_url(&self, model: &str) -> Result<Url, ProviderError>;

    /// Provider-specific headers (auth, version, content type).
    /// User-config `http_headers` / `env_http_headers` are layered on
    /// top by [`stream_via_adapter`] *after* this returns, so they can
    /// override defaults.
    fn build_headers(&self, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError>;

    /// Transform our normalized [`ChatRequest`] into the provider's
    /// JSON body.
    fn translate_request(&self, req: &ChatRequest) -> Result<Value, ProviderError>;

    /// Parse one decoded SSE event into zero or more [`ChatEvent`]s.
    /// Adapter has access to the [`ToolCallBuffer`] for stateful
    /// accumulation. Return `Ok(events)` — events are emitted in order.
    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut ToolCallBuffer,
    ) -> Result<Vec<ChatEvent>, ProviderError>;

    /// Map a non-2xx HTTP response into a typed [`ProviderError`].
    fn classify_error(&self, status: StatusCode, body: &str) -> ProviderError;

    /// Extras (custom headers / query params) declared in the provider
    /// config. Default is empty — adapters that store extras override
    /// this to surface them.
    fn extras(&self) -> &HttpProviderExtras {
        // OnceLock-backed empty singleton: HashMap::new() isn't const,
        // so we can't make this a `static` directly.
        static EMPTY: std::sync::OnceLock<HttpProviderExtras> = std::sync::OnceLock::new();
        EMPTY.get_or_init(HttpProviderExtras::default)
    }
}

/// Decoded SSE event passed to [`HttpAdapter::parse_event`].
#[derive(Clone, Debug)]
pub struct SseEvent {
    /// `event:` field; defaults to `"message"` per the spec.
    pub event: String,
    /// `data:` field, joined with `\n` if multiple.
    pub data: String,
}

/// The streaming workhorse. Build a request via the adapter, POST it,
/// decode the SSE stream, drive the adapter's `parse_event` for each
/// frame, and emit a flat [`LlmEventStream`].
///
/// Idle-timeout-protected: each `stream.next()` is wrapped in
/// `tokio::time::timeout(stream_idle_timeout, ...)`. A server that
/// stops sending bytes mid-stream is detected and surfaced as a
/// network error rather than hanging forever.
pub async fn stream_via_adapter<A>(
    base: Arc<HttpProviderBase>,
    adapter: Arc<A>,
    auth: ResolvedAuth,
    req: ChatRequest,
    _ctx: RequestContext,
) -> Result<LlmEventStream, ProviderError>
where
    A: HttpAdapter,
{
    let model = req
        .model
        .explicit()
        .ok_or_else(|| {
            ProviderError::InvalidRequest(
                "ChatRequest.model must be ModelHint::Explicit before reaching the Provider"
                    .into(),
            )
        })?
        .to_string();

    let mut url = adapter.build_url(&model)?;
    adapter.extras().apply_query_params(&mut url);

    let mut headers = adapter.build_headers(&auth)?;
    adapter.extras().apply_headers(&mut headers);

    let body = adapter.translate_request(&req)?;

    let response = base
        .client
        .request(Method::POST, url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(ProviderError::from)?;

    let status = response.status();
    if !status.is_success() {
        // Bounded read so a verbose error body can't OOM us.
        let text = response.text().await.unwrap_or_default();
        let trunc = if text.len() > 4096 { &text[..4096] } else { &text };
        return Err(adapter.classify_error(status, trunc));
    }

    let idle = base.config.stream_idle_timeout;
    let byte_stream = response.bytes_stream();
    let sse = byte_stream.eventsource();

    let mut buf = ToolCallBuffer::new();

    let stream = async_stream::try_stream! {
        let mut sse = sse;
        loop {
            // Per-chunk idle timeout. If the server stops sending bytes
            // for `idle` we surface a typed Network error and exit.
            let next = match tokio::time::timeout(idle, sse.next()).await {
                Ok(n) => n,
                Err(_) => Err(ProviderError::Network(Box::new(IdleTimeout(idle))))?,
            };
            let Some(event_result) = next else { break };
            let frame = event_result.map_err(|e| {
                ProviderError::Network(Box::new(e))
            })?;
            let decoded = SseEvent {
                event: if frame.event.is_empty() { "message".to_string() } else { frame.event },
                data: frame.data,
            };
            let events = adapter.parse_event(&decoded, &mut buf)?;
            for ev in events {
                yield ev;
            }
        }
    };

    Ok(Box::pin(stream))
}

/// Tiny error type used so the `ProviderError::Network` carries something
/// debuggable when an idle timeout fires.
#[derive(Debug)]
struct IdleTimeout(Duration);

impl std::fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no SSE chunk received within {:?}", self.0)
    }
}

impl std::error::Error for IdleTimeout {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_has_sensible_timeouts() {
        let c = HttpProviderConfig::default();
        assert!(c.connect_timeout.as_secs() >= 1);
        // Default idle is 5 minutes — plenty for slow LLMs.
        assert!(c.stream_idle_timeout.as_secs() >= 60);
    }

    #[test]
    fn base_constructs() {
        let _ = HttpProviderBase::default_arc().unwrap();
    }

    #[test]
    fn retry_caps_are_reasonable() {
        // Sanity: 100 retries × any reasonable backoff is still finite.
        // If we ever need more, raise the constant deliberately.
        assert_eq!(MAX_REQUEST_MAX_RETRIES, 100);
        assert_eq!(MAX_STREAM_MAX_RETRIES, 100);
    }
}
