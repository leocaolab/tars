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
        // True bounded read — stream chunks and stop at the cap so a
        // 1 GB hostile error body can't OOM us. Audit
        // `tars-provider-src-http-base-1` (round 2): the prior
        // implementation called `response.text().await`, which reads
        // the *entire* body before truncating — defeating the whole
        // point of the cap. Now we bail at `ERROR_BODY_CAP_BYTES` of
        // bytes received and carry whatever we've got into
        // `classify_error`. Read errors are surfaced as a marker
        // (round-1 fix); we keep that.
        let body = read_bounded_body(response, ERROR_BODY_CAP_BYTES).await;
        let trunc = truncate_utf8(&body, ERROR_BODY_CAP_BYTES);
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

/// Hard cap on how much of an HTTP error body we'll buffer for
/// `classify_error`. Anything past this is dropped before we even
/// allocate a `String`. Sized for "diagnostic message + small JSON
/// error envelope" — provider error responses are typically <1 KB.
pub(crate) const ERROR_BODY_CAP_BYTES: usize = 8 * 1024;

/// Stream the response body and stop at `cap` bytes. Read errors
/// surface as a marker string so `classify_error` sees what happened
/// instead of an empty input. The returned `String` may end on a
/// non-UTF-8 boundary at the cap; `truncate_utf8` (called by the
/// caller) walks back to a codepoint boundary.
async fn read_bounded_body(response: reqwest::Response, cap: usize) -> String {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(1024));
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let remaining = cap.saturating_sub(buf.len());
                if remaining == 0 {
                    break;
                }
                let take = bytes.len().min(remaining);
                buf.extend_from_slice(&bytes[..take]);
                if buf.len() >= cap {
                    break;
                }
            }
            Err(e) => {
                return format!(
                    "<error reading response body after {} bytes: {e}>",
                    buf.len()
                );
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Truncate `s` to at most `max_bytes` while staying on a UTF-8 boundary.
///
/// `&s[..max_bytes]` is unsafe-by-design: the index must fall on a
/// codepoint boundary or `str::index` panics. Bug report:
/// `tars-provider-src-http-base-2` (audit run 3ab2b7fa). Fix lifts the
/// idiom from std's own `floor_char_boundary` (still unstable as of 1.85).
pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk back from `max_bytes` to the previous char boundary.
    // is_char_boundary(s.len()) is always true, so this terminates.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

    #[test]
    fn truncate_utf8_handles_multibyte_boundary() {
        // Regression: `&s[..n]` panics when `n` lands mid-codepoint.
        // Audit finding `tars-provider-src-http-base-2`.
        // 4 bytes per emoji (each is 4-byte UTF-8) — truncate at 3
        // would split, must round down to 0.
        let s = "🦀🦀🦀";
        assert_eq!(truncate_utf8(s, 3), "");
        assert_eq!(truncate_utf8(s, 4), "🦀");
        assert_eq!(truncate_utf8(s, 7), "🦀");
        assert_eq!(truncate_utf8(s, 8), "🦀🦀");
        assert_eq!(truncate_utf8(s, 100), s);
    }

    #[test]
    fn truncate_utf8_short_input_is_passthrough() {
        assert_eq!(truncate_utf8("hi", 100), "hi");
    }

    #[test]
    fn truncate_utf8_ascii_like_simple_slice() {
        let s: String = "x".repeat(10_000);
        assert_eq!(truncate_utf8(&s, 4096).len(), 4096);
    }

    /// Audit `tars-provider-src-http-base-2`: round-1 of this finding
    /// added a marker string for body-read failures but didn't test
    /// it. Drive a wiremock 500 response with an oversized body and
    /// assert (a) we don't OOM, (b) the cap actually bounds memory.
    #[tokio::test]
    async fn error_body_is_capped_at_cap_bytes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // 1 MB body — more than 100× the cap.
        let big_body: String = "X".repeat(1_000_000);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(500).set_body_string(big_body))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/big", server.uri()))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_server_error());

        let body = read_bounded_body(resp, ERROR_BODY_CAP_BYTES).await;
        assert!(
            body.len() <= ERROR_BODY_CAP_BYTES,
            "body should be capped at {ERROR_BODY_CAP_BYTES} bytes, got {}",
            body.len(),
        );
        assert!(body.starts_with('X'), "should contain the start of the body");
    }
}
