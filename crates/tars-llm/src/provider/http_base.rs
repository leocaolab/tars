//! Sends a chat request over HTTP and decodes the SSE reply, for every backend that speaks HTTP;
//! a backend supplies only its wire format, as an [`HttpAdapter`]. It does not retry.

use std::env::VarError;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{
    CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, InvalidHeaderName, InvalidHeaderValue,
};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::ProviderError;
use crate::provider::error::{NetworkError, stated_class};
pub use tars_types::HttpOverrides;
use tars_types::{Caller, ChatEvent, ChatRequest, ErrorClass, ProviderId};

use crate::provider::LlmEventStream;
use crate::provider::auth::{EnvSource, ProcessEnv, ResolvedAuth};
use crate::provider::tool_buffer::{StreamDecodeState, StreamEnd};
use crate::provider::trace_headers::{TraceHeaderWriter, add_trace_headers};
use crate::provider::wire::{Attempt, Transport, WireOutcome, WireRecorder, failed};

const DEFAULT_STREAM_IDLE_MS: u64 = 300_000;

/// The HTTP client's settings. The two timeouts read from the `[llm.http]` config section in
/// milliseconds (`connect_timeout_ms`, `stream_idle_timeout_ms`); the user agent and the trace
/// headers are the binary's, not an operator's, and no file sets them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpClientConfig {
    /// Bounds the TCP and TLS handshake.
    #[serde(rename = "connect_timeout_ms", with = "crate::duration_serde::millis")]
    pub connect_timeout: Duration,
    /// Bounds the wait between two SSE chunks once the stream is open (default 5 minutes); a
    /// long generation may take far longer as a whole. It is a timer around each read, not
    /// reqwest's `read_timeout`, so a stall ends as `Network(Idle)` whether or not the
    /// request has a deadline, which reqwest would report as the deadline's `TimedOut`.
    #[serde(
        rename = "stream_idle_timeout_ms",
        with = "crate::duration_serde::millis"
    )]
    pub stream_idle_timeout: Duration,
    #[serde(skip)]
    pub user_agent: String,
    /// Writes the trace-context headers every request carries, read at send time inside the
    /// call's span; `None` adds none.
    #[serde(skip)]
    pub trace_headers: Option<TraceHeaderWriter>,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            stream_idle_timeout: Duration::from_millis(DEFAULT_STREAM_IDLE_MS),
            user_agent: format!("tars/{}", env!("CARGO_PKG_VERSION")),
            trace_headers: None,
        }
    }
}

/// The reqwest client the HTTP backends share, with its settings and the environment their
/// `env_http_headers` are looked up in.
#[derive(Clone)]
pub struct HttpClient {
    pub client: reqwest::Client,
    pub config: HttpClientConfig,
    pub env: Arc<dyn EnvSource>,
}

impl HttpClient {
    /// Builds the shared client.
    ///
    /// # Errors
    ///
    /// Returns reqwest's error when the client cannot be built (no TLS backend initializes).
    pub fn new(
        config: HttpClientConfig,
        env: Arc<dyn EnvSource>,
    ) -> Result<Arc<Self>, reqwest::Error> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            // No overall `.timeout()`: a streaming response can take minutes. A caller that
            // wants a total bound sets `ctx.deadline`, applied per request.
            .user_agent(&config.user_agent)
            .build()?;
        Ok(Arc::new(Self {
            client,
            config,
            env,
        }))
    }

    /// Builds the shared client with default settings over the process environment.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn default_arc() -> Result<Arc<Self>, reqwest::Error> {
        Self::new(HttpClientConfig::default(), Arc::new(ProcessEnv))
    }
}

/// The wire format of one HTTP backend; [`stream_via_adapter`] does the transport.
#[async_trait]
pub trait HttpAdapter: Send + Sync + 'static {
    /// Returns the URL to POST to, with any query parameter the provider requires (Gemini's
    /// `?alt=sse`); the configured `query_params` are appended after.
    fn build_url(&self, model: &str) -> Result<Url, ProviderError>;

    /// Returns the provider's headers (auth, version, content type). The configured
    /// `http_headers` and `env_http_headers` are applied after, so they override these.
    fn build_headers(&self, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError>;

    /// Returns the provider's JSON body for `req` against the concrete model `model`.
    fn translate_request(&self, req: &ChatRequest, model: &str) -> Result<Value, ProviderError>;

    /// Returns the chat events of one SSE event, in order, accumulating tool-call arguments in
    /// `buf` across events.
    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut StreamDecodeState,
    ) -> Result<Vec<ChatEvent>, ProviderError>;

    /// Returns the error of a non-2xx response; `headers` give `Retry-After` for
    /// `RateLimited::retry_after`, and `body` is capped at 8 KiB.
    fn classify_error(
        &self,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> ProviderError;

    /// Returns the extra headers and query parameters the provider config declares; none by
    /// default.
    fn extras(&self) -> &HttpOverrides {
        static EMPTY: std::sync::OnceLock<HttpOverrides> = std::sync::OnceLock::new();
        EMPTY.get_or_init(HttpOverrides::default)
    }
}

#[derive(Clone, Debug)]
pub struct SseEvent {
    /// The `event:` field, `"message"` when absent, as the SSE spec says.
    pub event: String,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

/// POSTs `req` through `adapter` and returns its events as they arrive.
///
/// A server that sends nothing for `stream_idle_timeout` ends the stream with a `Network` error;
/// a request past `ctx.deadline` fails with `TimedOut`, before or during the body. A body that
/// ends before the stream's end ends the stream with `StreamCut`. With a
/// `wire` recorder and a logged call (`ctx.call`), the attempt becomes one `llm_wire` row when it
/// ends: the body as sent, the status, and the response bytes as received.
#[allow(clippy::too_many_arguments)]
pub async fn stream_via_adapter<A>(
    base: Arc<HttpClient>,
    adapter: Arc<A>,
    auth: ResolvedAuth,
    req: ChatRequest,
    model: &str,
    ctx: Caller,
    provider: &ProviderId,
    wire: Option<&WireRecorder>,
) -> Result<LlmEventStream, ProviderError>
where
    A: HttpAdapter,
{
    let mut url = adapter.build_url(model)?;
    adapter.extras().apply_query_params(&mut url);

    let mut headers = adapter.build_headers(&auth)?;
    insert_extra_headers(adapter.extras(), base.env.as_ref(), &mut headers)
        .map_err(ProviderError::ExtraHeader)?;
    if let Some(write) = base.config.trace_headers {
        add_trace_headers(write, &mut headers);
    }
    // JSON unless the adapter or the extras set another content type.
    headers
        .entry(CONTENT_TYPE)
        .or_insert(HeaderValue::from_static("application/json"));

    let body = adapter.translate_request(&req, model)?;
    // Serialized once, so the bytes sent are the bytes the wire row records.
    let body = bytes::Bytes::from(serde_json::to_vec(&body).map_err(ProviderError::Encode)?);
    let mut attempt = wire.and_then(|w| {
        w.begin(
            &ctx,
            provider,
            model,
            Transport::HttpSse { status: None },
            body.clone(),
        )
    });

    let mut rb = base
        .client
        .request(Method::POST, url.clone())
        .headers(headers)
        .body(body);
    // Without a deadline, `connect_timeout` and `stream_idle_timeout` are the only bounds.
    let deadline_budget = ctx.remaining();
    if let Some(left) = deadline_budget {
        rb = rb.timeout(left);
    }

    let response = match rb.send().await {
        Ok(response) => response,
        Err(e) => {
            let fired = e.is_timeout().then(|| {
                fired_timeout(deadline_budget, base.config.connect_timeout, e.is_connect())
            });
            let e = match fired {
                Some(FiredTimeout::Deadline(budget)) => ProviderError::TimedOut {
                    budget,
                    detail: format!("HTTP request to {url} timed out (model={model}): {e}"),
                },
                _ => ProviderError::from(e),
            };
            if let Some(attempt) = attempt.take() {
                attempt.end(failed(&e));
            }
            return Err(e);
        }
    };

    let status = response.status();
    if let Some(attempt) = attempt.as_mut() {
        attempt.status(status.as_u16());
    }
    if !status.is_success() {
        // Cloned before the body read consumes the response.
        let headers = response.headers().clone();
        // Stops reading at the cap, so a huge error body cannot exhaust memory.
        let read = read_bounded(response, ERROR_BODY_CAP_BYTES).await;
        let text = read.text();
        let e =
            adapter.classify_error(status, &headers, truncate_utf8(&text, ERROR_BODY_CAP_BYTES));
        if let Some(mut attempt) = attempt.take() {
            attempt.received().push(&read.bytes);
            if read.truncated {
                attempt.truncated();
            }
            attempt.end(failed(&e));
        }
        return Err(e);
    }

    let idle = base.config.stream_idle_timeout;
    // Each chunk goes to the wire row before the SSE parser sees it.
    let tap = attempt.as_ref().map(Attempt::received);
    let byte_stream = response.bytes_stream().inspect(move |chunk| {
        if let (Some(tap), Ok(bytes)) = (&tap, chunk) {
            tap.push(bytes);
        }
    });
    let sse = byte_stream.eventsource();

    let mut buf = StreamDecodeState::new();

    // reqwest's per-request timeout also aborts the body after the headers arrived.
    let body_budget = deadline_budget;
    let model_owned = model.to_string();

    let stream = async_stream::stream! {
        // Dropped with the stream: an attempt the caller stops reading before
        // the provider's end is recorded as cancelled.
        let mut attempt = attempt;
        let mut sse = sse;
        loop {
            match next_events(&mut sse, idle, body_budget, &model_owned, adapter.as_ref(), &mut buf)
                .await
            {
                Ok(Some(events)) => {
                    // Marked before the events go out: a caller that stops
                    // reading at `Finished` dropped a completed attempt.
                    if let Some(attempt) = attempt.as_mut()
                        && events.iter().any(|ev| matches!(ev, ChatEvent::Finished { .. }))
                    {
                        attempt.ended_by_provider();
                    }
                    for ev in events {
                        yield Ok(ev);
                    }
                }
                Ok(None) => {
                    if let Some(attempt) = attempt.take() {
                        attempt.end(WireOutcome::Completed);
                    }
                    // A body that ended before the stream's end was cut; one that ended after it
                    // ends any call still open here.
                    match buf.end_open_calls(StreamEnd::BodyEnd) {
                        Ok(events) => {
                            for ev in events {
                                yield Ok(ev);
                            }
                        }
                        Err(e) => yield Err(e),
                    }
                    break;
                }
                Err(e) => {
                    if let Some(attempt) = attempt.take() {
                        attempt.end(failed(&e));
                    }
                    yield Err(e);
                    break;
                }
            }
        }
    };

    Ok(Box::pin(stream))
}

/// Which of the two reqwest timeouts on a request fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FiredTimeout {
    /// The connect timeout: a `Network` fault, retriable apart from the caller's deadline.
    Connect,
    /// The per-request timeout set from the caller's deadline, which left this budget.
    Deadline(Duration),
}

/// Returns which timeout fired for a timed-out request: during the handshake it is the connect
/// timeout unless the deadline was the shorter of the two; after it, only the deadline is armed.
fn fired_timeout(
    deadline_budget: Option<Duration>,
    connect_timeout: Duration,
    during_connect: bool,
) -> FiredTimeout {
    match deadline_budget {
        Some(budget) if !during_connect || budget <= connect_timeout => {
            FiredTimeout::Deadline(budget)
        }
        _ => FiredTimeout::Connect,
    }
}

/// Returns the events of the next SSE frame, `None` once the body has ended, and a `Network`
/// error when nothing arrives for `idle`.
async fn next_events<A, S>(
    sse: &mut S,
    idle: Duration,
    body_budget: Option<Duration>,
    model: &str,
    adapter: &A,
    buf: &mut StreamDecodeState,
) -> Result<Option<Vec<ChatEvent>>, ProviderError>
where
    A: HttpAdapter,
    S: futures::Stream<
            Item = Result<
                eventsource_stream::Event,
                eventsource_stream::EventStreamError<reqwest::Error>,
            >,
        > + Unpin,
{
    let next = tokio::time::timeout(idle, sse.next())
        .await
        .map_err(|_| ProviderError::Network(NetworkError::Idle(idle)))?;
    let Some(event_result) = next else {
        return Ok(None);
    };
    let frame = event_result.map_err(|e| {
        if let (Some(budget), eventsource_stream::EventStreamError::Transport(re)) =
            (body_budget, &e)
            && re.is_timeout()
        {
            return ProviderError::TimedOut {
                budget,
                detail: format!("HTTP SSE body read timed out (model={model}): {re}"),
            };
        }
        ProviderError::Network(NetworkError::Stream(e))
    })?;
    let decoded = SseEvent {
        event: if frame.event.is_empty() {
            "message".to_string()
        } else {
            frame.event
        },
        data: frame.data,
    };
    adapter.parse_event(&decoded, buf).map(Some)
}

/// Bounds how much of an error body is read; provider error bodies are usually under 1 KiB.
pub(crate) const ERROR_BODY_CAP_BYTES: usize = 8 * 1024;

/// An error response's body as read, up to a cap: the raw bytes, whether
/// bytes were left unread past the cap, and the error that cut the read
/// short, if one did.
pub(crate) struct BoundedBody {
    pub(crate) bytes: bytes::Bytes,
    pub(crate) truncated: bool,
    pub(crate) read_error: Option<reqwest::Error>,
}

impl BoundedBody {
    /// Returns the body as lossy UTF-8, led by the read error when one cut it short.
    pub(crate) fn text(&self) -> String {
        let body = String::from_utf8_lossy(&self.bytes);
        match &self.read_error {
            None => body.into_owned(),
            Some(e) => format!(
                "<error reading response body after {} bytes: {e}> {body}",
                self.bytes.len()
            ),
        }
    }
}

/// Reads the response body up to `cap` bytes. A read error ends the read and keeps the bytes
/// before it.
pub(crate) async fn read_bounded(response: reqwest::Response, cap: usize) -> BoundedBody {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(1024));
    let mut truncated = false;
    let mut read_error = None;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let remaining = cap.saturating_sub(buf.len());
                let take = bytes.len().min(remaining);
                buf.extend_from_slice(&bytes[..take]);
                if take < bytes.len() {
                    truncated = true;
                    break;
                }
            }
            Err(e) => {
                read_error = Some(e);
                break;
            }
        }
    }
    BoundedBody {
        bytes: buf.into(),
        truncated,
        read_error,
    }
}

/// Returns [`read_bounded`]'s body as text.
pub(crate) async fn read_bounded_body(response: reqwest::Response, cap: usize) -> String {
    read_bounded(response, cap).await.text()
}

/// Returns `url` parsed.
///
/// # Errors
///
/// Fails with `InvalidUrl`, carrying the text, when it does not parse.
pub(crate) fn parse_url(url: String) -> Result<Url, ProviderError> {
    Url::parse(&url).map_err(|source| ProviderError::InvalidUrl { url, source })
}

/// Cuts `s` to at most `max_bytes`, backing off to a char boundary. It exists because
/// `str::floor_char_boundary` is newer than the workspace's minimum Rust, 1.85.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Cuts `s` as [`truncate_utf8`] does and appends `…` when it cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let trimmed = truncate_utf8(s, max);
    if trimmed.len() == s.len() {
        s.to_string()
    } else {
        format!("{trimmed}…")
    }
}

/// Returns how long a 429 asks the caller to wait, for `ProviderError::RateLimited::retry_after`.
/// It tries, in order, `retry-after-ms` (Anthropic), `retry-after` in whole seconds, and
/// `retry-after` as an HTTP date (RFC 7231). A date already past gives `Duration::ZERO`, so
/// "retry now" stays distinct from no header; `None` means no header parsed.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(v) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok())
        && let Ok(ms) = v.trim().parse::<u64>()
    {
        return Some(Duration::from_millis(ms));
    }
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let target = httpdate::parse_http_date(raw).ok()?;
    let now = std::time::SystemTime::now();
    Some(target.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Inserts the headers a provider's config adds to each of its requests into `target`, replacing
/// headers of the same name: the static `http_headers`, then each `env_http_headers` header whose
/// var `env` holds. `env` must be the environment the provider's credential is read from, so a
/// `.env` file feeds both.
///
/// # Errors
///
/// Fails on the first configured header that does not make a valid one: a static one only when
/// the provider was not built by the registry, which refuses those, and an env one when its var
/// is set to something that is not a header value.
pub(crate) fn insert_extra_headers(
    extras: &HttpOverrides,
    env: &dyn EnvSource,
    target: &mut HeaderMap,
) -> Result<(), ExtraHeaderError> {
    for (name, value) in &extras.http_headers {
        let (name, value) = static_header(name, value)?;
        target.insert(name, value);
    }
    for (header, var) in &extras.env_http_headers {
        if let Some((name, value)) = env_header(header, var, env.var(var))? {
            target.insert(name, value);
        }
    }
    Ok(())
}

/// Returns the static `http_headers` of a provider's config as a header map.
///
/// # Errors
///
/// Fails on the first one that does not make a valid header.
pub(crate) fn static_headers(extras: &HttpOverrides) -> Result<HeaderMap, ExtraHeaderError> {
    extras
        .http_headers
        .iter()
        .map(|(name, value)| static_header(name, value))
        .collect()
}

/// Inserts the extra headers as [`insert_extra_headers`] does, skipping each one that does not
/// make a valid header with a `tracing::warn!`. Only the `tars models` listing uses it; a provider
/// call fails instead.
pub fn apply_extra_headers(extras: &HttpOverrides, env: &dyn EnvSource, target: &mut HeaderMap) {
    let statics = extras
        .http_headers
        .iter()
        .map(|(name, value)| static_header(name, value).map(Some));
    let from_env = extras
        .env_http_headers
        .iter()
        .map(|(header, var)| env_header(header, var, env.var(var)).map_err(ExtraHeaderError::from));
    for header in statics.chain(from_env) {
        match header {
            Ok(Some((name, value))) => {
                target.insert(name, value);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    error = %tars_utils::display_chain(&e),
                    "http_extras: skipping header",
                );
            }
        }
    }
}

/// A header a provider's config adds that does not make a valid one. It never holds the value,
/// which may be a credential.
#[derive(Debug, thiserror::Error)]
pub enum ExtraHeaderError {
    #[error("configured header `{header}` is not a valid header name")]
    Name {
        header: String,
        #[source]
        source: InvalidHeaderName,
    },
    #[error("the configured value of header `{header}` is not a valid header value")]
    Value {
        header: String,
        #[source]
        source: InvalidHeaderValue,
    },
    #[error(transparent)]
    Env(#[from] EnvHeaderError),
}

/// Why an `env_http_headers` entry whose var is set does not make a header. It never holds the
/// var's value, which may be a credential.
#[derive(Debug, thiserror::Error)]
pub enum EnvHeaderError {
    #[error("`{header}` is not a valid header name")]
    Name {
        header: String,
        #[source]
        source: InvalidHeaderName,
    },
    #[error("env var `{var}` for header `{header}` is not UTF-8")]
    NotUnicode { header: String, var: String },
    #[error("env var `{var}` does not hold a valid value for header `{header}`")]
    Value {
        header: String,
        var: String,
        #[source]
        source: InvalidHeaderValue,
    },
}

fn static_header(header: &str, value: &str) -> Result<(HeaderName, HeaderValue), ExtraHeaderError> {
    let name = HeaderName::try_from(header).map_err(|source| ExtraHeaderError::Name {
        header: header.to_owned(),
        source,
    })?;
    let value = HeaderValue::try_from(value).map_err(|source| ExtraHeaderError::Value {
        header: header.to_owned(),
        source,
    })?;
    Ok((name, value))
}

/// Returns the header `header` gets from env var `var`, whose lookup gave `lookup`: none when the
/// var is unset or blank, so `OPENAI_ORGANIZATION=` turns the header off.
fn env_header(
    header: &str,
    var: &str,
    lookup: Result<String, VarError>,
) -> Result<Option<(HeaderName, HeaderValue)>, EnvHeaderError> {
    let value = match lookup {
        Ok(value) if value.trim().is_empty() => return Ok(None),
        Ok(value) => value,
        Err(VarError::NotPresent) => return Ok(None),
        Err(VarError::NotUnicode(_)) => {
            return Err(EnvHeaderError::NotUnicode {
                header: header.to_owned(),
                var: var.to_owned(),
            });
        }
    };
    let name = HeaderName::try_from(header).map_err(|source| EnvHeaderError::Name {
        header: header.to_owned(),
        source,
    })?;
    let value = HeaderValue::try_from(value).map_err(|source| EnvHeaderError::Value {
        header: header.to_owned(),
        var: var.to_owned(),
        source,
    })?;
    Ok(Some((name, value)))
}

/// Returns the class a response's `x-should-retry` header states (Anthropic and OpenAI send it),
/// for [`ProviderError::stated`].
pub(crate) fn stated_by_headers(headers: &reqwest::header::HeaderMap) -> Option<ErrorClass> {
    // [arc:intentional-handle] reason: a header value that is not text states nothing, as an
    // absent header does; the response's own status still classifies the error.
    let value = headers.get("x-should-retry").and_then(|v| v.to_str().ok());
    stated_class(value.map(str::trim))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_has_sensible_timeouts() {
        let c = HttpClientConfig::default();
        assert!(c.connect_timeout.as_secs() >= 1);
        assert!(c.stream_idle_timeout.as_secs() >= 60);
    }

    #[test]
    fn base_constructs() {
        let _ = HttpClient::default_arc().unwrap();
    }

    #[test]
    fn an_unset_or_blank_var_sends_no_header() {
        assert!(matches!(
            env_header("X-Org", "ORG", Err(VarError::NotPresent)),
            Ok(None)
        ));
        assert!(matches!(
            env_header("X-Org", "ORG", Ok(" \t".into())),
            Ok(None)
        ));
    }

    #[test]
    fn a_set_var_sends_its_value_as_the_header() {
        let (name, value) = env_header("X-Org", "ORG", Ok("org-1".into()))
            .unwrap()
            .unwrap();
        assert_eq!(name, "x-org");
        assert_eq!(value, "org-1");
    }

    #[test]
    fn a_set_var_that_makes_no_valid_header_is_an_error_without_its_value() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = std::ffi::OsString::from_vec(vec![0xff]);
            assert!(matches!(
                env_header("X-Org", "ORG", Err(VarError::NotUnicode(raw))),
                Err(EnvHeaderError::NotUnicode { .. })
            ));
        }
        assert!(matches!(
            env_header("bad name", "ORG", Ok("org-1".into())),
            Err(EnvHeaderError::Name { .. })
        ));
        let err = env_header("X-Org", "ORG", Ok("sk-secret\n".into())).unwrap_err();
        assert!(matches!(err, EnvHeaderError::Value { .. }));
        assert!(!tars_utils::display_chain(&err).contains("sk-secret"));
    }

    struct FileAndShell;

    impl EnvSource for FileAndShell {
        fn var(&self, name: &str) -> Result<String, VarError> {
            match name {
                "IN_ENV" => Ok("org-1".into()),
                _ => Err(VarError::NotPresent),
            }
        }
    }

    #[test]
    fn applies_static_headers() {
        let extras = HttpOverrides {
            http_headers: [("X-Custom".into(), "v1".into())].into_iter().collect(),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_extra_headers(&extras, &FileAndShell, &mut headers);
        assert_eq!(headers.get("X-Custom").unwrap(), "v1");
    }

    #[test]
    fn env_headers_are_looked_up_in_the_given_environment_not_the_process() {
        let extras = HttpOverrides {
            env_http_headers: [
                ("X-Org".into(), "IN_ENV".into()),
                ("X-Project".into(), "NOWHERE".into()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_extra_headers(&extras, &FileAndShell, &mut headers);
        assert_eq!(headers.get("X-Org").unwrap(), "org-1");
        assert!(headers.get("X-Project").is_none());
    }

    #[test]
    fn a_handshake_timeout_is_the_connect_timeout_unless_the_deadline_was_shorter() {
        let connect = Duration::from_secs(5);
        let six = Duration::from_secs(6);
        let four = Duration::from_secs(4);
        assert_eq!(
            fired_timeout(Some(six), connect, true),
            FiredTimeout::Connect
        );
        assert_eq!(
            fired_timeout(Some(four), connect, true),
            FiredTimeout::Deadline(four)
        );
        assert_eq!(fired_timeout(None, connect, true), FiredTimeout::Connect);
        assert_eq!(
            fired_timeout(Some(six), connect, false),
            FiredTimeout::Deadline(six)
        );
    }

    #[test]
    fn truncate_utf8_handles_multibyte_boundary() {
        // Each crab is 4 bytes, so a cut at 3 backs off to 0.
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

    #[tokio::test]
    async fn error_body_is_capped_at_cap_bytes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

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
        assert!(
            body.starts_with('X'),
            "should contain the start of the body"
        );
    }

    // ── parse_retry_after ──────────────────────────────────────────────

    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn parse_retry_after_returns_none_for_empty_headers() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_retry_after_prefers_retry_after_ms_when_present() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("250"));
        h.insert(RETRY_AFTER, HeaderValue::from_static("60"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_millis(250)));
    }

    #[test]
    fn parse_retry_after_handles_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("42"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(42)));
    }

    #[test]
    fn parse_retry_after_handles_http_date_in_future() {
        let mut h = HeaderMap::new();
        let future = std::time::SystemTime::now() + Duration::from_secs(60);
        let date_str = httpdate::fmt_http_date(future);
        h.insert(RETRY_AFTER, HeaderValue::from_str(&date_str).unwrap());
        let parsed = parse_retry_after(&h).expect("HTTP date should parse");
        // An HTTP date has whole seconds, and a second boundary may pass meanwhile.
        assert!(
            parsed.as_secs() >= 58 && parsed.as_secs() <= 62,
            "expected ~60s, got {parsed:?}",
        );
    }

    #[test]
    fn parse_retry_after_handles_http_date_already_passed() {
        let mut h = HeaderMap::new();
        let past = std::time::SystemTime::now() - Duration::from_secs(120);
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(past)).unwrap(),
        );
        assert_eq!(parse_retry_after(&h), Some(Duration::ZERO));
    }

    #[test]
    fn parse_retry_after_returns_none_for_garbage_value() {
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("not-a-date-or-number"),
        );
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_returns_none_for_garbage_ms_value() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("not-a-number"));
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn an_http_section_sets_the_timeouts_and_leaves_the_agent_and_headers_alone() {
        let cfg: HttpClientConfig =
            toml::from_str("connect_timeout_ms = 2000\n").expect("a partial [llm.http] reads");
        let default = HttpClientConfig::default();
        assert_eq!(cfg.connect_timeout, Duration::from_secs(2));
        assert_eq!(cfg.stream_idle_timeout, default.stream_idle_timeout);
        assert_eq!(cfg.user_agent, default.user_agent);
        assert!(toml::from_str::<HttpClientConfig>("user_agent = \"x\"\n").is_err());
    }

    #[test]
    fn the_default_http_config_writes_only_the_timeouts() {
        let written: toml::Table =
            toml::Table::try_from(HttpClientConfig::default()).expect("the default writes");
        let keys: Vec<&str> = written.keys().map(String::as_str).collect();
        assert_eq!(keys, ["connect_timeout_ms", "stream_idle_timeout_ms"]);
    }
}
