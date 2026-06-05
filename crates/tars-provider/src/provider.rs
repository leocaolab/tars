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
    ///
    /// **All-or-nothing contract:** this returns either a fully
    /// accumulated [`ChatResponse`] or the first mid-stream error. If the
    /// stream errors after emitting some deltas, the partially
    /// accumulated state is discarded and the `Err` is returned — callers
    /// who need the partial text must drive [`Self::stream`] directly and
    /// aggregate themselves.
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
                    // saturating_add: this count gates budget/cost
                    // checks, so on pathological inputs cap at usize::MAX
                    // rather than wrap silently to a tiny value in release.
                    chars = chars.saturating_add(t.len());
                }
            }
        }
        // Rough heuristic — 4 chars per token for English mix.
        // `usize as u64` is lossless on all supported targets, but use
        // try_from to make that explicit and stay correct if usize ever
        // exceeds 64 bits.
        Ok(u64::try_from(chars).unwrap_or(u64::MAX).div_ceil(4))
    }

    /// Compute USD cost from observed usage. Defaults to `pricing.cost_for`.
    fn cost(&self, usage: &Usage) -> CostUsd {
        self.capabilities().pricing.cost_for(usage)
    }

    /// If this provider supports the vendor's batch API, return a handle
    /// to its [`crate::batch::BatchSubmitter`] surface. Default `None`
    /// — backends that implement `BatchSubmitter` override this to
    /// return `Some(self)`. See `docs/roadmap.md §5` for the rationale.
    ///
    /// Callers do:
    /// ```ignore
    /// let p: Arc<dyn LlmProvider> = registry.get(&id).unwrap();
    /// match p.as_batch_submitter() {
    ///     Some(b) => b.submit(items).await?,
    ///     None => /* this provider has no batch surface — sync only */,
    /// }
    /// ```
    fn as_batch_submitter(self: Arc<Self>) -> Option<Arc<dyn crate::batch::BatchSubmitter>> {
        let _ = self; // suppress unused-self warning
        None
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

// Type alias for downstream consumers that don't want to write `BoxStream` themselves.
pub type EventBoxStream = BoxStream<'static, Result<ChatEvent, ProviderError>>;
