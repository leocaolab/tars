//! The [`LlmProvider`] trait.
//!
//! See Doc 01 §3 for design rationale. Key points:
//!
//! - `stream` is the **basic** operation; `complete` is a default-impl
//!   that consumes the stream and aggregates with [`ChatResponseBuilder`].
//! - Trait takes `self: Arc<Self>` so streams can outlive the call site
//!   (the `'static` requirement on `BoxStream`).
//! - Item type is `Result<ChatEvent, ProviderError>` — mid-stream errors
//!   are common and must not panic.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream, StreamExt};

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder, CostUsd,
    ProviderError, ProviderId, RequestContext, Usage,
};

/// Convenience alias for the streaming return type. `'static` because
/// the stream owns everything it needs (no borrowing from `self`).
pub type LlmEventStream =
    Pin<Box<dyn Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static>>;

#[async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    /// Stable identifier (`openai_main`, `local_qwen`, …).
    fn id(&self) -> &ProviderId;

    /// Static capability descriptor — what this Provider can do.
    fn capabilities(&self) -> &Capabilities;

    /// Open a streaming chat. Adapter clones `self` internally.
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError>;

    /// Default: consume `stream` and accumulate. Override only if the
    /// provider has a non-streaming fast-path that's strictly better.
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<ChatResponse, ProviderError> {
        let mut s = self.stream(req, ctx).await?;
        let mut acc = ChatResponseBuilder::new();
        while let Some(event) = s.next().await {
            acc.apply(event?);
        }
        Ok(acc.finish())
    }

    /// Estimate token count for a request. `fast = true` allows
    /// chars/4 estimation (suitable for budget checks); `false`
    /// requires loading the real tokenizer.
    ///
    /// The default implementation only supports `fast = true`. Adapters
    /// that have a real tokenizer must override this and honor `fast = false`.
    async fn count_tokens(&self, req: &ChatRequest, fast: bool) -> Result<u64, ProviderError> {
        if !fast {
            return Err(ProviderError::Internal(
                "count_tokens(fast=false) requires a real tokenizer; this provider only supports fast estimation".into(),
            ));
        }
        let mut chars: usize = req.system.as_deref().map_or(0, str::len);
        for m in &req.messages {
            for block in m.content() {
                if let Some(t) = block.as_text() {
                    chars += t.len();
                }
            }
        }
        // Rough heuristic — 4 chars per token for English mix.
        Ok((chars as u64).div_ceil(4))
    }

    /// Compute USD cost from observed usage. Defaults to `pricing.cost_for`.
    fn cost(&self, usage: &Usage) -> CostUsd {
        self.capabilities().pricing.cost_for(usage)
    }
}

/// Convenience helper: turn any `Stream<Item=Result<ChatEvent, ProviderError>> + Send + 'static`
/// into an [`LlmEventStream`].
pub fn boxed_stream<S>(s: S) -> LlmEventStream
where
    S: Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static,
{
    Box::pin(s)
}

/// Erase the concrete stream type (used in tests / mocks).
pub fn boxed_iter_stream(
    events: impl IntoIterator<Item = Result<ChatEvent, ProviderError>> + Send + 'static,
) -> LlmEventStream
where
    <Vec<Result<ChatEvent, ProviderError>> as IntoIterator>::IntoIter: Send,
{
    let v: Vec<_> = events.into_iter().collect();
    boxed_stream(futures::stream::iter(v))
}

// Type alias for downstream consumers that don't want to write `BoxStream` themselves.
pub type EventBoxStream = BoxStream<'static, Result<ChatEvent, ProviderError>>;
