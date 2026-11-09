//! A provider that remembers the calls it served, so a test can see the tenant an eval entry
//! point made each call for and the text it sent.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tars_llm::ProviderError;
use tars_llm::{CannedResponse, LlmEventStream, LlmProvider, MockProvider};
use tars_types::{Caller, ChatRequest, ProviderId, ProviderProfile, TenantId};

pub(crate) struct CallRecorder {
    inner: Arc<MockProvider>,
    tenants: Mutex<Vec<TenantId>>,
    prompts: Mutex<Vec<String>>,
}

impl CallRecorder {
    pub(crate) fn new(id: &'static str, reply: &str) -> Arc<Self> {
        Self::replying(ProviderId::new(id), CannedResponse::text(reply))
    }

    /// Builds a recorder that answers every call with `response`.
    pub(crate) fn replying(id: ProviderId, response: CannedResponse) -> Arc<Self> {
        Arc::new(Self {
            inner: MockProvider::new(id, response),
            tenants: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
        })
    }

    /// Returns the tenant of every call served, in call order.
    pub(crate) fn tenants(&self) -> Vec<TenantId> {
        self.tenants.lock().unwrap().clone()
    }

    /// Returns the text of every call's messages, one string per call, in call order.
    pub(crate) fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for CallRecorder {
    fn id(&self) -> &ProviderId {
        self.inner.id()
    }

    fn capabilities(&self) -> &ProviderProfile {
        self.inner.capabilities()
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        self.tenants.lock().unwrap().push(ctx.tenant_id.clone());
        let text: String = req
            .messages
            .iter()
            .flat_map(|m| m.content())
            .filter_map(|b| b.as_text())
            .collect();
        self.prompts.lock().unwrap().push(text);
        Arc::clone(&self.inner).stream(req, model, ctx).await
    }
}
