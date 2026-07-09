//! Output-validation middleware. See [Doc 15](../../docs/architecture/15-output-validation.md).
//!
//! Position in the onion (W4 final): **outside Cache, outside Retry,
//! inside Telemetry**. Onion order:
//! `Telemetry → Validation → Cache → Retry → Provider`.
//!
//! `ValidationFailed` is **always `ErrorClass::Permanent`** — no retry.
//! The W1 "retriable Reject" path was cut in W4: it would have required
//! either putting Validation back inside Retry (re-introducing the
//! cache×filter corruption W4 fixes) or duplicating retry logic inside
//! ValidationMiddleware itself, both for a use case (model resample on
//! validation failure) that real consumers (2026-05-08 dogfood feedback)
//! don't have. Callers who do need a model retry on validation failure
//! catch `ValidationFailed` at their own layer.
//!
//! ## Lifecycle within `call()`
//!
//! 1. Drain the inner stream into a complete `ChatResponse`.
//!    Validators by definition need the whole response (rule_id
//!    whitelist needs all findings, JSON shape needs the full text).
//!    Streaming UX is preserved at the *outer* layer (caller still
//!    iterates a stream); inside this middleware, the response is
//!    materialised once.
//! 2. Run validators in registration order.
//!    - `Pass` → keep response, record `OutcomeSummary::Pass`.
//!    - `Filter { response, dropped }` → response replaces current;
//!      subsequent validators see the filtered version.
//!    - `Reject { reason }` → short-circuit. Return
//!      `Err(ValidationFailed { validator, reason })` (always Permanent).
//!      No subsequent validators run; no summary is attached because
//!      there's no Response object.
//!    - `Annotate { metrics }` → keep response; record metrics.
//! 3. Stamp `validation_summary` onto the (potentially Filtered)
//!    response. Re-emit as a stream so the caller-visible contract
//!    is unchanged.
//!
//! ## Cache × Validator interaction
//!
//! W4 (2026-05-08) moved Validation OUTSIDE Cache, fixing two W1 bugs:
//!
//! 1. Cache used to store post-Filter events (because ValidationMiddleware
//!    re-emit happened *inside* Cache); now Cache sees raw Provider
//!    events and stores raw.
//! 2. Cache hits used to skip validators entirely (because Cache
//!    short-circuits before reaching its inner layer); now Validation
//!    runs on every call — hit or miss — because it sits *outside* Cache.
//!
//! Result: validators are pure (same input → same output, per
//! [`OutputValidator`] trait contract) and rerun on every call. Cache
//! holds raw across the lifetime of an entry — changing the validator
//! chain on a Pipeline doesn't invalidate cache, multi-caller cache
//! sharing across distinct validator chains is safe.
//!
//! Failed-validator runs DO have a side-effect on cache: the raw
//! Provider response was already streamed through Cache before
//! Validation could reject, so it's stored. Repeated cache hits
//! deterministically fail the same way (validator is pure). Callers
//! who want force-fresh on validation fail use an explicit
//! `skip_cache=True` kwarg (future).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use tars_provider::LlmEventStream;
use tars_types::{
    ChatRequest, ChatResponse, ChatResponseBuilder, OutcomeSummary, ProviderError, RequestContext,
    ValidationOutcome, ValidationSummary,
};

use crate::middleware::Middleware;
use crate::service::Next;

pub mod builtin;

/// Trait implemented by output validators. See module docs / Doc 15
/// for the full lifecycle and design rationale.
///
/// **Pure-function contract**: implementations MUST be deterministic
/// (same `(req, resp)` → same `ValidationOutcome`) and side-effect-free
/// (no IO, no global state mutation). The Cache×Validator interaction
/// rule depends on this property — if it breaks, multi-caller cache
/// sharing produces incorrect behaviour. Validators that need IO go
/// to the evaluator framework (Doc 16) where async + non-determinism
/// are first-class.
///
/// **Panic safety**: implementations should not panic on
/// adversarial input. The middleware does NOT catch panics; a
/// panicking validator brings down the request thread. (For evaluators
/// the OnlineEvaluatorRunner does catch_unwind because evaluators run
/// out-of-band; validators run on the request hot path where catching
/// would mask bugs.)
pub trait OutputValidator: Send + Sync {
    /// Stable name. Used as the key in
    /// [`ValidationSummary::outcomes`] and as the `validator` field
    /// of [`ProviderError::ValidationFailed`]. Should be unique
    /// within a Pipeline's validator list — duplicates collapse in
    /// the BTreeMap (last-write-wins).
    fn name(&self) -> &str;

    /// Run the validator against a (req, resp) pair. The request is
    /// supplied so validators that need original-prompt context
    /// (e.g. SnippetGroundingValidator wants source bytes) can use
    /// it; most validators ignore it.
    fn validate(&self, req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome;
}

/// Middleware that runs a chain of [`OutputValidator`]s after the
/// provider's stream finishes. See module docs.
pub struct ValidationMiddleware {
    validators: Arc<[Arc<dyn OutputValidator>]>,
}

impl ValidationMiddleware {
    /// Build from a `Vec<Arc<dyn OutputValidator>>`. Callers porting
    /// from the previous `Vec<Box<...>>` shape rewrite `Box::new(X)`
    /// to `Arc::new(X)`; single-validator vec literals need an
    /// explicit `as Arc<dyn OutputValidator>` cast on one element
    /// (subsequent elements coerce automatically).
    pub fn new(validators: Vec<Arc<dyn OutputValidator>>) -> Self {
        Self {
            validators: validators.into(),
        }
    }
}

#[async_trait]
impl Middleware for ValidationMiddleware {
    fn name(&self) -> &'static str {
        "validation"
    }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        // Telemetry: layer trace. Best-effort, but don't silently drop
        // it on a poisoned mutex — recover the guard (the poisoned data
        // is still readable/appendable) and log so the poisoning is
        // visible. [arc:intentional-handle] reason: telemetry is
        // observability-only; a poisoned lock must never abort a user's
        // LLM request, and recovering preserves the layer trace.
        match ctx.telemetry.lock() {
            Ok(mut t) => t.layers.push("validation".into()),
            Err(poisoned) => {
                tracing::warn!(
                    "validation: telemetry mutex poisoned; recording layer trace on recovered guard"
                );
                poisoned.into_inner().layers.push("validation".into());
            }
        }

        // Empty validator chain — pass through, no drain. Avoids the
        // streaming-UX cost when ValidationMiddleware was added but no
        // validators were registered.
        if self.validators.is_empty() {
            return next.run(req, ctx).await;
        }

        let started = Instant::now();

        // Capture the validation_outcome handle BEFORE moving ctx into
        // the inner call; we'll write to it after the stream drains.
        let outcome_handle = ctx.validation_outcome.clone();

        // Drain the inner stream into a complete ChatResponse. Cache
        // hit short-circuits land here too — the inner stream is a
        // replay from CacheLookupMiddleware (which sits OUTSIDE retry
        // and therefore outside us). Validators run on raw cached
        // payload exactly the same as on a fresh response.
        let inner_stream = next.run(req.clone(), ctx).await?;
        let mut builder = ChatResponseBuilder::new();
        let mut events_held = Vec::new();
        let mut s = inner_stream;
        while let Some(ev) = s.next().await {
            let ev = ev?;
            events_held.push(ev.clone());
            builder.apply(ev);
        }
        let mut response = builder.finish();

        // Run validators in registration order.
        let mut summary = ValidationSummary::default();
        let mut filtered_any = false;
        for v in self.validators.iter() {
            let name = v.name().to_string();
            summary.validators_run.push(name.clone());
            match v.validate(&req, &response) {
                ValidationOutcome::Pass => {
                    summary.outcomes.insert(name, OutcomeSummary::Pass);
                }
                ValidationOutcome::Filter {
                    response: filtered,
                    dropped,
                } => {
                    summary.outcomes.insert(
                        name,
                        OutcomeSummary::Filter {
                            dropped: dropped.clone(),
                        },
                    );
                    response = filtered;
                    filtered_any = true;
                }
                ValidationOutcome::Reject { reason } => {
                    // Don't write summary on Reject — there's no
                    // Response to attach it to. The error itself
                    // carries everything caller needs. ValidationFailed
                    // is always Permanent (W4): no retry.
                    return Err(ProviderError::ValidationFailed {
                        validator: name,
                        reason,
                    });
                }
                ValidationOutcome::Annotate { metrics } => {
                    summary
                        .outcomes
                        .insert(name, OutcomeSummary::Annotate { metrics });
                }
                // `#[non_exhaustive]` wildcard. Future variants
                // (e.g. Defer) treated as Pass for now.
                _ => {
                    summary.outcomes.insert(name, OutcomeSummary::Pass);
                }
            }
        }
        summary.total_wall_ms = started.elapsed().as_millis() as u64;

        // Attach summary to response. Stash on the side-channel so
        // the OUTER caller (typically tars-py's `run_complete`) can
        // pull the typed summary + the filtered Response *after*
        // it drains the stream. Caller substitutes the filtered
        // response over the streamed one.
        response.validation_summary = summary;
        // Publish the summary + blessed response on the side-channel.
        // A poisoned mutex must not silently strip this metadata — the
        // outer caller (tars-py's `run_complete`) relies on it — so
        // recover the guard and log rather than dropping the write.
        {
            let mut rec = match outcome_handle.lock() {
                Ok(rec) => rec,
                Err(poisoned) => {
                    tracing::warn!(
                        "validation: outcome side-channel mutex poisoned; publishing summary on recovered guard"
                    );
                    poisoned.into_inner()
                }
            };
            rec.summary = response.validation_summary.clone();
            // Always publish the response so the caller can pick it
            // up — even when no Filter ran, the outer caller may
            // prefer the ValidationMiddleware-blessed version
            // (carries summary) over re-deriving from events.
            rec.filtered_response = Some(response.clone());
        }

        // Re-emit. If a Filter validator changed the response,
        // re-stream the filtered version so downstream observers
        // see the final shape. If nothing was Filtered, replay the
        // captured events verbatim (preserves token-by-token timing
        // semantics for any further wrapping middleware).
        if filtered_any {
            // Preserve the ORIGINAL cache-hit metadata when re-emitting
            // the filtered response. The builder captured it from the
            // inner stream's `Started` event into `response.cache_hit`;
            // using `CacheHitInfo::default()` here would erase whether
            // this response came from cache, diverging from the
            // verbatim-replay (non-filtered) path below and corrupting
            // downstream cache accounting / observability.
            let cache_hit = response.cache_hit.clone();
            let stream = futures::stream::iter(response.into_events(cache_hit).into_iter().map(Ok));
            Ok(Box::pin(stream))
        } else {
            let stream = futures::stream::iter(events_held.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }
}

#[cfg(test)]
mod tests;
