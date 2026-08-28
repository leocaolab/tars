//! AWS Bedrock provider — the thin `LlmProvider` adapter over the
//! `tars-bedrock` leaf crate.
//!
//! Feature-gated behind `tars-provider/bedrock`; the AWS SDK subtree only
//! enters a build that asks for Bedrock.

use std::sync::Arc;

use async_trait::async_trait;

use tars_bedrock::BedrockClient;
use tars_types::{
    ChatRequest, ChatResponse, ProviderError, ProviderId, ProviderProfile, RequestContext,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// Builder for [`BedrockProvider`]. No `HttpProviderBase` / `AuthResolver`
/// — Bedrock owns its own transport (the AWS SDK) and auth (the credential
/// chain), so it ignores the shared reqwest/SSE base.
#[derive(Clone, Debug)]
pub struct BedrockProviderBuilder {
    id: ProviderId,
    region: String,
    model: String,
    profile: Option<String>,
    capabilities: Option<ProviderProfile>,
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
    /// ambient role wins.
    pub fn profile(mut self, p: Option<String>) -> Self {
        self.profile = p;
        self
    }

    pub fn capabilities(mut self, c: ProviderProfile) -> Self {
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
    capabilities: ProviderProfile,
    client: BedrockClient,
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }

    /// Non-streaming fast path: unary `converse()` via the leaf client,
    /// strictly cheaper than a stream for the aggregate case.
    #[tracing::instrument(
        name = "bedrock.complete",
        skip_all,
        fields(provider = %self.id, model = %model),
        err(Display),
    )]
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        _ctx: RequestContext,
    ) -> Result<ChatResponse, ProviderError> {
        self.client.complete_response(&req, model).await
    }

    /// Token-by-token `ConverseStream`.
    #[tracing::instrument(
        name = "bedrock.stream",
        skip_all,
        fields(provider = %self.id, model = %model),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        self.client.stream_response(&req, model).await
    }
}
