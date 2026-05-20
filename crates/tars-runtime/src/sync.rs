//! Sync convenience wrappers over the async `LlmService` trait.
//!
//! Two helpers, both motivated by the same observation: every sync
//! caller of `tars-pipeline` (CLI tools, FFI bindings, downstream
//! consumers like arc) was reinventing the same plumbing —
//!
//! 1. A `LazyLock<tokio::runtime::Runtime>` to bridge sync → async;
//! 2. `let mut stream = svc.call(...).await?; while let Some(ev) =
//!    stream.next().await { builder.apply(ev?); }` to assemble a
//!    [`ChatResponse`] from the event stream;
//! 3. The `ValidationOutcome` side-channel substitution so callers
//!    see the post-Filter response (if any) and the validation
//!    summary in both Filter and non-Filter paths.
//!
//! Centralising the trio here removes ~80 lines of duplicated
//! plumbing from each consumer and ensures the side-channel handling
//! stays correct as `ValidationMiddleware` evolves.

use std::sync::Arc;
use std::sync::LazyLock;

use futures::StreamExt;
use tokio::runtime::{Builder, Runtime};

use tars_pipeline::LlmService;
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
///
/// Applies the validation-outcome side-channel substitution:
///
/// - If a Filter validator ran, the post-Filter response replaces
///   the raw streamed one. (The stream itself re-emits the filtered
///   text, but the side channel is the authoritative source for the
///   substitution to handle the empty-validator-chain passthrough
///   case correctly.)
/// - The `validation_summary` is copied from the side channel onto
///   the response even when no Filter ran — the streamed events
///   don't carry it directly.
///
/// Returns the same `ProviderError` shape as a direct async call,
/// including `ValidationFailed` (always `ErrorClass::Permanent`).
pub fn complete_sync(
    svc: Arc<dyn LlmService>,
    req: ChatRequest,
    ctx: RequestContext,
) -> Result<ChatResponse, ProviderError> {
    shared_runtime().block_on(async move {
        let outcome_handle = ctx.validation_outcome.clone();
        let mut stream = svc.call(req, ctx).await?;
        let mut builder = ChatResponseBuilder::new();
        while let Some(ev) = stream.next().await {
            builder.apply(ev?);
        }
        let mut response = builder.finish();
        if let Ok(rec) = outcome_handle.lock() {
            if let Some(filtered) = rec.filtered_response.as_ref() {
                response = filtered.clone();
            } else {
                response.validation_summary = rec.summary.clone();
            }
        }
        Ok(response)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_pipeline::{Pipeline, PipelineOpts};
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ModelHint, ProviderId};

    #[test]
    fn shared_runtime_is_stable_across_calls() {
        let a = shared_runtime() as *const _;
        let b = shared_runtime() as *const _;
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn complete_sync_drains_stream_into_response() {
        let provider = MockProvider::new("p", CannedResponse::text("hello"));
        let pipeline = Pipeline::default_chain(provider, PipelineOpts::new(ProviderId::new("p")));
        let svc: Arc<dyn LlmService> = Arc::new(pipeline);

        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "ping");
        let ctx = RequestContext::test_default();
        let resp = complete_sync(svc, req, ctx).expect("call succeeds");

        assert_eq!(resp.text, "hello");
    }

    #[test]
    fn complete_sync_substitutes_filtered_response() {
        use tars_pipeline::{MaxLengthValidator, OutputValidator};

        let provider = MockProvider::new("p", CannedResponse::text("hello world"));
        let mut opts = PipelineOpts::new(ProviderId::new("p"));
        opts.validators =
            vec![Arc::new(MaxLengthValidator::truncate_above(5)) as Arc<dyn OutputValidator>];
        let pipeline = Pipeline::default_chain(provider, opts);
        let svc: Arc<dyn LlmService> = Arc::new(pipeline);

        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "ping");
        let ctx = RequestContext::test_default();
        let resp = complete_sync(svc, req, ctx).expect("call succeeds");

        assert_eq!(resp.text, "hello"); // post-Filter
    }
}
