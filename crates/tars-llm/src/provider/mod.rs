//! Defines [`LlmProvider`]: `stream` is the basic operation, and `complete` by default collects
//! the stream into one response. `stream`, `complete` and `as_batch_submitter` take
//! `self: Arc<Self>` so what they return can outlive the call that opened it. Mid-stream failures
//! are items of the stream, not panics.

#[macro_use]
mod builder_macros;

pub mod auth;
pub mod backends;
pub mod batch;
#[cfg(feature = "cli")]
pub mod child_reaper;
pub mod endpoint;
pub mod error;
pub mod http_base;
pub mod registry;
pub mod schema_adapt;
pub mod tool_buffer;
pub mod trace_headers;
pub mod wire;

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream, StreamExt};

use crate::ProviderError;
use crate::provider::error::{Feature, RequestRefusal};
use tars_types::{
    Caller, ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder, CostUsd, OutputLimit,
    ProviderId, ProviderProfile, Usage,
};

/// A stream of chat events that owns what it needs and borrows nothing from the provider.
pub type LlmEventStream =
    Pin<Box<dyn Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static>>;

#[async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    fn id(&self) -> &ProviderId;

    fn capabilities(&self) -> &ProviderProfile;

    /// Returns the most output tokens this provider produces for the concrete model `model`, and
    /// where that number came from. The pipeline's `LlmService` fills a request that sets no
    /// `max_output_tokens` with it, because the wire protocol's own default is far below the
    /// model's ceiling on several providers.
    ///
    /// The default is [`OutputLimit::Unknown`]: a provider built outside config knows no model
    /// table.
    fn output_limit(&self, model: &str) -> OutputLimit {
        let _ = model;
        OutputLimit::Unknown
    }

    /// Opens a streaming call of `req` against `model`, a concrete provider-side name such as
    /// `"gpt-4o-2024-08-06"`. The model is an argument because a [`ChatRequest`] holds content
    /// only; the caller has already resolved any model hint.
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError>;

    /// Makes one call and returns the whole response; by default it collects [`Self::stream`].
    ///
    /// It returns all or nothing: a stream that fails after some deltas returns the error and
    /// drops the partial text. A caller that needs the partial text drives [`Self::stream`]
    /// itself.
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<ChatResponse, ProviderError> {
        let mut s = self.stream(req, model, ctx).await?;
        let mut acc = ChatResponseBuilder::new();
        while let Some(event) = s.next().await {
            acc.apply(event?);
        }
        Ok(acc.finish())
    }

    /// Estimates the request's token count. With `fast` it may divide the text's byte length by 4,
    /// which is enough for budget checks; without it, it must use the real tokenizer.
    ///
    /// The default supports only `fast` and refuses the request as unsupported otherwise; a
    /// backend with a tokenizer overrides it.
    async fn count_tokens(&self, req: &ChatRequest, fast: bool) -> Result<u64, ProviderError> {
        if !fast {
            return Err(ProviderError::InvalidRequest(RequestRefusal::Unsupported(
                Feature::ExactTokenCount,
            )));
        }
        let mut chars: usize = req.system.as_deref().map_or(0, str::len);
        for m in &req.messages {
            for block in m.content() {
                if let Some(t) = block.as_text() {
                    // Saturates: the count gates budget checks, and a wrap would make it tiny.
                    chars = chars.saturating_add(t.len());
                }
            }
        }
        // About 4 bytes per token for English text.
        Ok(u64::try_from(chars).unwrap_or(u64::MAX).div_ceil(4))
    }

    fn cost(&self, usage: &Usage) -> CostUsd {
        self.capabilities().pricing.cost_for(usage)
    }

    /// Returns this provider as a [`crate::provider::batch::BatchSubmitter`] when it supports the vendor's
    /// batch API, `None` by default.
    fn as_batch_submitter(
        self: Arc<Self>,
    ) -> Option<Arc<dyn crate::provider::batch::BatchSubmitter>> {
        let _ = self;
        None
    }
}

pub fn boxed_stream<S>(s: S) -> LlmEventStream
where
    S: Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static,
{
    Box::pin(s)
}

pub type EventBoxStream = BoxStream<'static, Result<ChatEvent, ProviderError>>;
