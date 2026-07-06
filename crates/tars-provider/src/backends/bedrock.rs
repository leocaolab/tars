//! AWS Bedrock provider — the thin `LlmProvider` adapter over the
//! `tars-bedrock` leaf crate (Doc 31 §6 C3).
//!
//! All the Bedrock-specific work — `ChatRequest` ↔ Converse mapping, the
//! `serde_json::Value` ↔ `Document` shim, the lazy keyless SigV4 client,
//! and SDK-error classification — lives in `tars-bedrock`, which depends
//! only on `tars-types`. This module contributes the one thing that must
//! live with the trait's owner: the `impl LlmProvider`. Hosting it here
//! (rather than in `tars-bedrock`) is what keeps the crate graph acyclic —
//! `tars-bedrock` cannot both provide the trait impl *and* be depended on
//! by the crate that defines the trait.
//!
//! Feature-gated behind `tars-provider/bedrock`; the AWS SDK subtree only
//! enters a build that asks for Bedrock.

use std::sync::Arc;

use async_trait::async_trait;

use tars_bedrock::BedrockClient;
use tars_types::{
    Capabilities, ChatRequest, ChatResponse, ProviderError, ProviderId, RequestContext,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// Builder for [`BedrockProvider`]. No `HttpProviderBase` / `AuthResolver`
/// — Bedrock owns its own transport (the AWS SDK) and auth (the credential
/// chain), so it ignores the shared reqwest/SSE base (Doc 31 §7).
#[derive(Clone, Debug)]
pub struct BedrockProviderBuilder {
    id: ProviderId,
    region: String,
    model: String,
    profile: Option<String>,
    capabilities: Option<Capabilities>,
}

impl BedrockProviderBuilder {
    pub fn new(id: impl Into<ProviderId>, region: String, model: String) -> Self {
        Self {
            id: id.into(),
            region,
            model,
            profile: None,
            capabilities: None,
        }
    }

    /// Name a local AWS profile (laptop case). Omit on AWS, where the
    /// ambient role wins (Doc 31 CUJ-4).
    pub fn profile(mut self, p: Option<String>) -> Self {
        self.profile = p;
        self
    }

    pub fn capabilities(mut self, c: Capabilities) -> Self {
        self.capabilities = Some(c);
        self
    }

    pub fn build(self) -> Arc<BedrockProvider> {
        let capabilities = self
            .capabilities
            .unwrap_or_else(tars_bedrock::default_capabilities);
        Arc::new(BedrockProvider {
            id: self.id,
            capabilities,
            client: BedrockClient::new(self.region, self.model, self.profile),
        })
    }
}

pub struct BedrockProvider {
    id: ProviderId,
    capabilities: Capabilities,
    client: BedrockClient,
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Non-streaming fast path (Doc 31 §6 C3): unary `converse()` via the
    /// leaf client, strictly cheaper than a stream for the aggregate case.
    #[tracing::instrument(
        name = "bedrock.complete",
        skip_all,
        fields(provider = %self.id, model = %req.model.label()),
        err(Display),
    )]
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<ChatResponse, ProviderError> {
        self.client.complete_response(&req).await
    }

    /// M1 streaming (Doc 31 §6 C2): real token-by-token `ConverseStream`.
    /// The leaf client opens the stream and translates each
    /// `ConverseStreamOutput` event into a canonical `ChatEvent`
    /// incrementally; the returned stream is already `'static + Send`, so
    /// it maps straight onto [`LlmEventStream`].
    #[tracing::instrument(
        name = "bedrock.stream",
        skip_all,
        fields(provider = %self.id, model = %req.model.label()),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        self.client.stream_response(&req).await
    }
}
