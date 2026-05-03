//! HTTP transport abstraction.
//!
//! Borrowed pattern from `codex-rs/codex-client/src/transport.rs`:
//! HTTP execution sits behind a `HttpTransport` trait so tests can
//! substitute a fake without spinning up a real wiremock server. The
//! production impl is [`ReqwestTransport`].
//!
//! The trait deliberately stays narrow — `execute` for one-shot, `stream`
//! for byte-streaming. Higher-level concerns (SSE decoding, retry,
//! per-chunk idle timeout) live above in [`crate::http_base`] so a
//! single transport impl works for every provider.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::sync::Arc;

use tars_types::ProviderError;
use url::Url;

/// One byte stream from a streaming response. `'static` so it can
/// outlive the call that opened it.
pub type ByteStream = BoxStream<'static, Result<Bytes, ProviderError>>;

/// Description of an outgoing HTTP request.
#[derive(Clone, Debug)]
pub struct OutboundRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Value>,
}

/// Aggregated response (for non-streaming endpoints we don't currently
/// use, but exposed for completeness so adapters can `execute` against
/// non-SSE endpoints — e.g. cachedContent CRUD on Gemini).
#[derive(Debug)]
pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Streaming response: status + headers known immediately, body streamed.
pub struct StreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: ByteStream,
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn execute(&self, req: OutboundRequest) -> Result<HttpResponse, ProviderError>;
    async fn stream(&self, req: OutboundRequest) -> Result<StreamResponse, ProviderError>;
}

/// Production transport — backed by [`reqwest::Client`].
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    fn build_request(&self, req: OutboundRequest) -> reqwest::RequestBuilder {
        let mut b = self.client.request(req.method, req.url).headers(req.headers);
        if let Some(body) = req.body {
            b = b.json(&body);
        }
        b
    }

    fn map_send_error(e: reqwest::Error) -> ProviderError {
        ProviderError::Network(Box::new(e))
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, req: OutboundRequest) -> Result<HttpResponse, ProviderError> {
        let resp = self
            .build_request(req)
            .send()
            .await
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(Self::map_send_error)?;
        Ok(HttpResponse { status, headers, body })
    }

    async fn stream(&self, req: OutboundRequest) -> Result<StreamResponse, ProviderError> {
        let resp = self
            .build_request(req)
            .send()
            .await
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes: ByteStream = Box::pin(
            resp.bytes_stream()
                .map(|r| r.map_err(|e| ProviderError::Network(Box::new(e)))),
        );
        Ok(StreamResponse { status, headers, bytes })
    }
}

/// Convenience: wrap `reqwest::Client` as `Arc<dyn HttpTransport>`.
pub fn arc_reqwest_transport(client: reqwest::Client) -> Arc<dyn HttpTransport> {
    Arc::new(ReqwestTransport::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingTransport {
        recorded: std::sync::Mutex<Option<OutboundRequest>>,
    }

    #[async_trait]
    impl HttpTransport for RecordingTransport {
        async fn execute(&self, req: OutboundRequest) -> Result<HttpResponse, ProviderError> {
            *self.recorded.lock().unwrap() = Some(req);
            Ok(HttpResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from_static(b"{}"),
            })
        }

        async fn stream(&self, req: OutboundRequest) -> Result<StreamResponse, ProviderError> {
            *self.recorded.lock().unwrap() = Some(req);
            Ok(StreamResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                bytes: Box::pin(futures::stream::empty()),
            })
        }
    }

    #[tokio::test]
    async fn fake_transport_records_outbound_request() {
        let t = Arc::new(RecordingTransport::default());
        let url = Url::parse("https://example.com/x").unwrap();
        let req = OutboundRequest {
            method: Method::POST,
            url: url.clone(),
            headers: HeaderMap::new(),
            body: Some(serde_json::json!({"hello": "world"})),
        };
        let _ = t.execute(req).await.unwrap();
        let captured = t.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(captured.url, url);
        assert_eq!(captured.method, Method::POST);
    }
}
