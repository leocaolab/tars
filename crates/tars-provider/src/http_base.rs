//! Shared HTTP infrastructure for HTTP-based Providers.
//!
//! Doc 01 §6.1 says all HTTP backends share one reqwest client + a
//! common retry/timeout/SSE-decoding base, and each Provider is just an
//! [`HttpAdapter`] (build_url / build_headers / translate_request /
//! parse_event / classify_error). This module provides that base.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use url::Url;

use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext};

use crate::auth::ResolvedAuth;
use crate::provider::LlmEventStream;
use crate::tool_buffer::ToolCallBuffer;

/// HTTP timeouts. We split connect/read so e.g. a slow first byte from
/// LLM (5-10s typical) doesn't get confused with a dead TCP socket.
#[derive(Clone, Debug)]
pub struct HttpProviderConfig {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub user_agent: String,
}

impl Default for HttpProviderConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            // LLM streaming responses can take many seconds between
            // chunks; this is the *idle* read timeout, not total.
            read_timeout: Duration::from_secs(120),
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
            // No `.timeout()` — we want streaming, not request total.
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
    /// Where to POST.
    fn build_url(&self, model: &str) -> Result<Url, ProviderError>;

    /// Provider-specific headers (auth, version, content type).
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
/// This is the function each HTTP provider's `LlmProvider::stream`
/// will call.
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
        .ok_or_else(|| ProviderError::InvalidRequest(
            "ChatRequest.model must be ModelHint::Explicit before reaching the Provider"
                .into(),
        ))?
        .to_string();

    let url = adapter.build_url(&model)?;
    let headers = adapter.build_headers(&auth)?;
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

    // Pull the bytes stream; eventsource-stream handles SSE framing.
    let byte_stream = response.bytes_stream();
    let sse = byte_stream.eventsource();

    let mut buf = ToolCallBuffer::new();

    let stream = async_stream::try_stream! {
        let mut sse = sse;
        while let Some(event_result) = sse.next().await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_has_sensible_timeouts() {
        let c = HttpProviderConfig::default();
        assert!(c.connect_timeout.as_secs() >= 1);
        assert!(c.read_timeout.as_secs() >= 30);
    }

    #[test]
    fn base_constructs() {
        let _ = HttpProviderBase::default_arc().unwrap();
    }
}
