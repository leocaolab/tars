//! Carries the caller's trace context to the servers tars calls over HTTP. tars-llm does not
//! know how a trace context is written (W3C `traceparent`, another format); the program that
//! installed the trace exporter passes the function that writes it in the HTTP client's settings
//! ([`HttpClientConfig::trace_headers`]). Without one no header is added.
//!
//! [`HttpClientConfig::trace_headers`]: crate::HttpClientConfig::trace_headers

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Returns the headers that carry the trace context current at the call, as name and value.
pub type TraceHeaderWriter = fn() -> Vec<(String, String)>;

/// Adds the headers `write` returns to `headers`; a header the request already sets (an
/// `extras` header the user configured) is kept. A pair that is not a valid header is skipped
/// with a warning: tracing must not fail the call.
pub(crate) fn add_trace_headers(write: TraceHeaderWriter, headers: &mut HeaderMap) {
    for (name, value) in write() {
        match (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            (Ok(name), Ok(value)) => {
                headers.entry(name).or_insert(value);
            }
            _ => tracing::warn!(
                event = "llm.trace_header.invalid",
                header = %name,
                value = %value,
                "the trace-header writer returned a header that cannot be sent; skipped"
            ),
        }
    }
}
