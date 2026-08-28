//! Sync convenience wrappers over the async `LlmService` trait.

use std::sync::LazyLock;

use futures::StreamExt;
use tokio::runtime::{Builder, Runtime};

use crate::LlmService;
use tars_types::{ChatRequest, ChatResponse, ChatResponseBuilder, ProviderError, RequestContext};

/// Process-wide multi-thread tokio runtime. One shared instance so
/// every sync caller in the process amortises the thread-pool cost
/// (≈ 1ms / 1MB per pool). Multi-threaded so a single sync caller's
/// async I/O concurrency inside one round-trip still works.
///
/// Callers who need their own runtime (different thread count,
/// per-tenant isolation, etc.) should construct their own instead of
/// using this — the shared instance is for the common case.
pub fn shared_runtime() -> &'static Runtime {
    static RT: LazyLock<Runtime> = LazyLock::new(|| {
        Builder::new_multi_thread()
            .enable_all()
            .thread_name("tars-shared")
            .build()
            .expect("tars-runtime: failed to build shared runtime")
    });
    &RT
}

/// Drive `svc.call(req, ctx)` to completion synchronously on the
/// [`shared_runtime`], returning the assembled [`ChatResponse`].
pub async fn complete_async(
    svc: LlmService,
    req: ChatRequest,
    ctx: RequestContext,
) -> Result<ChatResponse, ProviderError> {
    let outcome_handle = ctx.validation_outcome.clone();
    let mut stream = svc.call(req, ctx).await?;
    let mut builder = ChatResponseBuilder::new();
    while let Some(ev) = stream.next().await {
        builder.apply(ev?);
    }
    let mut response = builder.finish();
    // The validation-outcome side channel is security-critical: it
    // carries the post-Filter response that must REPLACE the raw
    // stream. A poisoned lock means a panic happened while a writer
    // held it — we can't prove filtering ran, so fail closed rather
    // than silently return an unvalidated response.
    let rec = outcome_handle.lock().map_err(|_| {
        ProviderError::Internal(
            "validation_outcome lock poisoned; cannot confirm output filtering ran".into(),
        )
    })?;
    if let Some(filtered) = rec.filtered_response.as_ref() {
        response = filtered.clone();
    } else {
        response.validation_summary = rec.summary.clone();
    }
    drop(rec);
    Ok(response)
}

/// Block-on wrapper over [`complete_async`] for SYNC call sites.
/// Callers already on a runtime should await [`complete_async`] directly instead — no nested runtime, no block_on.
pub fn complete_sync(
    svc: LlmService,
    req: ChatRequest,
    ctx: RequestContext,
) -> Result<ChatResponse, ProviderError> {
    shared_runtime().block_on(complete_async(svc, req, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChainOpts, LlmService};
    use std::sync::Arc;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::ProviderId;

    #[test]
    fn shared_runtime_is_stable_across_calls() {
        let a = shared_runtime() as *const _;
        let b = shared_runtime() as *const _;
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn complete_sync_drains_stream_into_response() {
        let provider = MockProvider::new("p", CannedResponse::text("hello"));
        let pipeline =
            LlmService::default_chain(provider, "p", ChainOpts::new(ProviderId::new("p")));
        let svc: LlmService = pipeline;

        let req = ChatRequest::user("ping");
        let ctx = RequestContext::test_default();
        let resp = complete_sync(svc, req, ctx).expect("call succeeds");

        assert_eq!(resp.text, "hello");
    }

    #[test]
    fn complete_sync_substitutes_filtered_response() {
        use crate::{MaxLengthValidator, OutputValidator};

        let provider = MockProvider::new("p", CannedResponse::text("hello world"));
        let mut opts = ChainOpts::new(ProviderId::new("p"));
        opts.validators =
            vec![Arc::new(MaxLengthValidator::truncate_above(5)) as Arc<dyn OutputValidator>];
        let pipeline = LlmService::default_chain(provider, "p", opts);
        let svc: LlmService = pipeline;

        let req = ChatRequest::user("ping");
        let ctx = RequestContext::test_default();
        let resp = complete_sync(svc, req, ctx).expect("call succeeds");

        assert_eq!(resp.text, "hello"); // post-Filter
    }
}
