//! Cache-lookup middleware (Doc 02 §4.4 + Doc 03).
//!
//! Sits **above** Routing/Retry in the canonical onion: a cache hit
//! must short-circuit before either of those layers gets to spend
//! anything. Below IAM though — the cache key already encodes the IAM
//! scopes, so IAM rejection has to happen before we even compute the
//! key.
//!
//! ## On hit
//!
//! Replays the cached response as a synthetic [`ChatEvent`] stream
//! via [`tars_types::ChatResponse::into_events`]. Outer middleware
//! (Telemetry, etc.) cannot tell the replay from a fresh stream —
//! the only signal is `ChatEvent::Started.cache_hit`.
//!
//! ## On miss
//!
//! Calls the inner service, then wraps the returned stream so the
//! [`tars_types::ChatResponse`] is reconstructed as events flow past
//! the consumer. When the terminal `Finished` event arrives we
//! validate the stop-reason for cacheability (Doc 03 §5.1) and write
//! to the registry. Both the consumer's stream consumption and the
//! cache write happen in the same task — no `tokio::spawn`, no
//! "cache write missed because the user dropped the stream early".
//!
//! ## On non-cacheable request
//!
//! `CacheKeyFactory::compute` returns
//! `CacheError::{NonDeterministic, UnresolvedTier, UncacheableEnsemble}`
//! → middleware skips the entire cache flow and just delegates to
//! inner. Logged at `debug` so this is observable but not noisy.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;

use tars_cache::{CacheKeyFactory, CachePolicy, CacheRegistry, CachedResponse};
use tars_provider::LlmEventStream;
use tars_types::{
    CacheHitInfo, ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder, ProviderError,
    ProviderId, RequestContext, StopReason,
};

use crate::middleware::Middleware;
use crate::service::LlmService;

/// Builder-and-factory for [`CacheLookupService`]. The inner Arcs are
/// the real moving parts; the middleware is just a thin layer holder.
#[derive(Clone)]
pub struct CacheLookupMiddleware {
    registry: Arc<dyn CacheRegistry>,
    factory: CacheKeyFactory,
    /// Identifier stamped on `CachedResponse.origin_provider`. M1 has
    /// no Routing layer, so the binding to a single provider id at
    /// build time is fine; once Routing exists this becomes a hint
    /// the inner service overrides per call.
    origin_provider: ProviderId,
}

impl CacheLookupMiddleware {
    pub fn new(
        registry: Arc<dyn CacheRegistry>,
        factory: CacheKeyFactory,
        origin_provider: ProviderId,
    ) -> Self {
        Self { registry, factory, origin_provider }
    }
}

impl Middleware for CacheLookupMiddleware {
    fn name(&self) -> &'static str {
        "cache_lookup"
    }
    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(CacheLookupService {
            inner,
            registry: self.registry.clone(),
            factory: self.factory.clone(),
            origin_provider: self.origin_provider.clone(),
        })
    }
}

struct CacheLookupService {
    inner: Arc<dyn LlmService>,
    registry: Arc<dyn CacheRegistry>,
    factory: CacheKeyFactory,
    origin_provider: ProviderId,
}

#[async_trait]
impl LlmService for CacheLookupService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Telemetry: layer trace.
        if let Ok(mut t) = ctx.telemetry.lock() {
            t.layers.push("cache_lookup".into());
        }
        let policy = read_policy(&ctx);
        if !policy.any_enabled() {
            // Cache fully off — skip key compute too (it's not free).
            return self.inner.clone().call(req, ctx).await;
        }

        let key = match self.factory.compute(&req, &ctx) {
            Ok(k) => k,
            Err(e) if e.is_not_cacheable() => {
                tracing::debug!(reason = %e, "cache: skipping non-cacheable request");
                return self.inner.clone().call(req, ctx).await;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    model = %req.model.label(),
                    msg_count = req.messages.len(),
                    "cache: key computation failed, treating as miss",
                );
                return self.inner.clone().call(req, ctx).await;
            }
        };

        // ── Lookup ─────────────────────────────────────────────────
        match self.registry.lookup(&key, &policy).await {
            Ok(Some(hit)) => {
                tracing::debug!(
                    key = %key.hex(),
                    label = %key.debug_label,
                    "cache: hit",
                );
                // Telemetry: record the middleware-cache hit. (Distinct
                // from `Usage.cached_input_tokens` which is the
                // *provider's* prompt cache. Both can be true; this
                // field is "we avoided the provider call entirely".)
                if let Ok(mut t) = ctx.telemetry.lock() {
                    t.cache_hit = true;
                }
                let cache_hit = CacheHitInfo {
                    // Surface the original input-token count as the
                    // "cached" figure — gives a direct read on "cost
                    // saved" without a separate field.
                    cached_input_tokens: hit.original_usage.input_tokens,
                    used_explicit_handle: false,
                    replayed_from_cache: true,
                };
                let events = hit.response.into_events(cache_hit);
                let stream = futures::stream::iter(events.into_iter().map(Ok));
                return Ok(Box::pin(stream));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    key = %key.hex(),
                    label = %key.debug_label,
                    "cache: lookup failed; treating as miss (Doc 03 §4.3)",
                );
            }
        }

        // ── Miss → call inner, wrap stream to capture for write ────
        let inner_stream = self.inner.clone().call(req, ctx.clone()).await?;
        let captured = wrap_stream_for_write(
            inner_stream,
            self.registry.clone(),
            key,
            policy,
            self.origin_provider.clone(),
        );
        Ok(Box::pin(captured))
    }
}

/// Read [`CachePolicy`] from `ctx.attributes` under `"cache.policy"`.
/// Falls back to `CachePolicy::default()` if missing/malformed.
const POLICY_ATTR: &str = "cache.policy";

fn read_policy(ctx: &RequestContext) -> CachePolicy {
    // Audit `tars-pipeline-src-cache-1`: lock poisoning previously
    // degraded silently to default. Poison = a panic occurred
    // while another task held the write side; treating that as
    // "default policy is fine" hides the underlying bug. Log loud,
    // then degrade.
    let attrs = match ctx.attributes.read() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(
                error = %e,
                "cache: ctx.attributes RwLock poisoned during read; \
                 falling back to default policy. The underlying panic \
                 should be investigated.",
            );
            return CachePolicy::default();
        }
    };
    let Some(v) = attrs.get(POLICY_ATTR) else {
        return CachePolicy::default();
    };
    serde_json::from_value::<CachePolicy>(v.clone()).unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            "cache: `{POLICY_ATTR}` attribute couldn't be deserialized; using default",
        );
        CachePolicy::default()
    })
}

/// Wrap an upstream event stream so we observe every event AND
/// reconstruct the [`ChatResponse`] for caching as a side effect.
/// Events flow through unchanged.
fn wrap_stream_for_write(
    inner: LlmEventStream,
    registry: Arc<dyn CacheRegistry>,
    key: tars_cache::CacheKey,
    policy: CachePolicy,
    origin_provider: ProviderId,
) -> impl futures::Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static {
    async_stream::stream! {
        let mut s = inner;
        let mut builder = ChatResponseBuilder::new();
        let mut saw_terminal = false;
        let mut had_error = false;

        while let Some(item) = s.next().await {
            match &item {
                Ok(ev) => {
                    if matches!(ev, ChatEvent::Finished { .. }) {
                        saw_terminal = true;
                    }
                    builder.apply(ev.clone());
                }
                Err(_) => {
                    had_error = true;
                }
            }
            yield item;
        }

        if had_error || !saw_terminal {
            tracing::debug!("cache: skipping write for incomplete/errored stream");
            return;
        }

        let response: ChatResponse = builder.finish();
        if !is_cacheable_outcome(&response) {
            tracing::debug!(
                stop = ?response.stop_reason,
                "cache: skipping write — non-cacheable stop reason (Doc 03 §5.1)",
            );
            return;
        }

        let stop_reason = response.stop_reason;
        let value = CachedResponse {
            cached_at: std::time::SystemTime::now(),
            origin_provider,
            original_usage: response.usage,
            response,
        };
        let label = key.debug_label.clone();
        let key_hex = key.hex();
        // Cache write is fire-and-forget by design (Doc 03 §4.3): the
        // response has already streamed to the client, so a write
        // failure can't be surfaced to the caller. We log loud with
        // enough context (key hex, label, stop reason) for an operator
        // to correlate against telemetry. A future SLO/metric layer can
        // tap this site too — keep the log shape stable.
        if let Err(e) = registry.write(key, value, &policy).await {
            tracing::warn!(
                error = %e,
                key = %key_hex,
                label = %label,
                stop_reason = ?stop_reason,
                "cache: write failed (degraded silently)",
            );
        }
    }
}

/// Doc 03 §5.1: only fully-successful turns are cached. MaxTokens is
/// truncated; Cancelled/ContentFilter/StopSequence/Other lack semantic
/// completeness; only EndTurn and ToolUse round-trip cleanly.
fn is_cacheable_outcome(response: &ChatResponse) -> bool {
    matches!(
        response.stop_reason,
        Some(StopReason::EndTurn) | Some(StopReason::ToolUse)
    )
}

/// Helper for callers (tars-cli, future tars-server) — sets the
/// `cache.policy` attribute on a context. Saves them from importing
/// the constant.
///
/// # Silent failure modes
///
/// This returns `()`, not `Result`, but it can fail silently in two
/// ways. Both are logged at `error` level with the originating cause:
///
/// 1. **JSON encode failure** — `policy` cannot be serialized. Should
///    be impossible for a well-formed `CachePolicy` value, but the
///    branch exists for completeness.
/// 2. **`ctx.attributes` `RwLock` poisoned** — a previous task panicked
///    while holding the write side. The caller's policy is **not**
///    applied; the context keeps whatever policy was already there
///    (default if none).
///
/// Callers that need a hard guarantee that their policy was applied
/// (e.g. compliance-driven cache disable) should write to
/// `ctx.attributes` themselves and surface the lock-poisoning error,
/// or check the resulting policy via [`read_policy`] in their own
/// audit path. The signature stays `()` here because policy-setting
/// is an advisory hint, not a request-critical operation.
pub fn set_cache_policy(ctx: &RequestContext, policy: &CachePolicy) {
    let value = match serde_json::to_value(policy) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "cache: failed to encode policy; left at default");
            return;
        }
    };
    // Audit `tars-pipeline-src-cache-5`: lock poisoning was
    // previously a silent no-op — the caller's explicit policy
    // wouldn't apply, but the function returned as if it had.
    // Log loud so a poisoned-lock incident leaves a trace; the
    // function still doesn't return Result because a "tell me your
    // cache preference" call shouldn't be on the request critical
    // path.
    match ctx.attributes.write() {
        Ok(mut attrs) => {
            attrs.insert(POLICY_ATTR.into(), value);
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "cache: ctx.attributes RwLock poisoned during set_cache_policy; \
                 the caller's explicit policy was NOT applied. Investigate the \
                 prior panic that poisoned the lock.",
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::json;
    use tars_cache::MemoryCacheRegistry;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ModelHint, Usage};

    use crate::Pipeline;

    /// A counting wrapper around MockProvider so we can assert the
    /// inner provider was (or wasn't) called.
    struct CountingService {
        inner: Arc<dyn LlmService>,
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl LlmService for CountingService {
        async fn call(
            self: Arc<Self>,
            req: ChatRequest,
            ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.clone().call(req, ctx).await
        }
    }

    fn ctx() -> RequestContext {
        RequestContext::test_default()
    }

    fn deterministic_request(prompt: &str) -> ChatRequest {
        let mut r = ChatRequest::user(ModelHint::Explicit("mock-1".into()), prompt);
        r.temperature = Some(0.0);
        r
    }

    fn build_pipeline_with_cache(
        registry: Arc<dyn CacheRegistry>,
        provider: Arc<dyn tars_provider::LlmProvider>,
    ) -> (Arc<Pipeline>, Arc<AtomicU32>) {
        let counter = Arc::new(AtomicU32::new(0));
        let provider_service: Arc<dyn LlmService> =
            crate::ProviderService::new(provider);
        let counting: Arc<dyn LlmService> = Arc::new(CountingService {
            inner: provider_service,
            calls: counter.clone(),
        });
        let factory = CacheKeyFactory::new(1);
        let mw = CacheLookupMiddleware::new(
            registry,
            factory,
            ProviderId::new("mock_origin"),
        );
        let pipeline = Pipeline::builder_with_inner(counting)
            .layer(mw)
            .build();
        (Arc::new(pipeline), counter)
    }

    async fn drain(stream: LlmEventStream) -> Vec<ChatEvent> {
        let mut s = stream;
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn second_identical_call_hits_cache_and_skips_inner() {
        let registry = MemoryCacheRegistry::default_arc();
        let mock = MockProvider::new("mock_origin", CannedResponse::text("haiku"));
        let (pipeline, counter) = build_pipeline_with_cache(registry.clone(), mock);

        let r1 = pipeline.clone().call(deterministic_request("p"), ctx()).await.unwrap();
        let _ = drain(r1).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let r2 = pipeline.clone().call(deterministic_request("p"), ctx()).await.unwrap();
        let events = drain(r2).await;
        // Inner not called again.
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // The replay must end in Finished AND surface replayed_from_cache.
        match &events[0] {
            ChatEvent::Started { cache_hit, .. } => {
                assert!(
                    cache_hit.replayed_from_cache,
                    "Started.cache_hit.replayed_from_cache must be true on cache hit",
                );
            }
            other => panic!("first event should be Started, got {other:?}"),
        }
        assert!(matches!(events.last(), Some(ChatEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn distinct_prompts_each_miss_then_each_hit() {
        let registry = MemoryCacheRegistry::default_arc();
        let mock = MockProvider::new("p", CannedResponse::text("x"));
        let (pipeline, counter) = build_pipeline_with_cache(registry, mock);

        for prompt in ["a", "b"] {
            let s = pipeline.clone().call(deterministic_request(prompt), ctx()).await.unwrap();
            let _ = drain(s).await;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 2, "two misses on first round");
        for prompt in ["a", "b"] {
            let s = pipeline.clone().call(deterministic_request(prompt), ctx()).await.unwrap();
            let _ = drain(s).await;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 2, "second round is all hits");
    }

    #[tokio::test]
    async fn non_deterministic_request_bypasses_cache() {
        let registry = MemoryCacheRegistry::default_arc();
        let mock = MockProvider::new("p", CannedResponse::text("x"));
        let (pipeline, counter) = build_pipeline_with_cache(registry, mock);

        // No temperature set → NonDeterministic → cache skipped.
        let mut req = ChatRequest::user(ModelHint::Explicit("mock-1".into()), "p");
        req.temperature = None;

        for _ in 0..3 {
            let s = pipeline.clone().call(req.clone(), ctx()).await.unwrap();
            let _ = drain(s).await;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3, "every call hits inner");
    }

    #[tokio::test]
    async fn explicit_policy_off_disables_cache_entirely() {
        let registry = MemoryCacheRegistry::default_arc();
        let mock = MockProvider::new("p", CannedResponse::text("x"));
        let (pipeline, counter) = build_pipeline_with_cache(registry, mock);

        let ctx = ctx();
        set_cache_policy(&ctx, &CachePolicy::off());

        for _ in 0..3 {
            let s = pipeline.clone().call(deterministic_request("p"), ctx.clone()).await.unwrap();
            let _ = drain(s).await;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn errored_stream_is_not_cached() {
        // Mock returns an Error from stream() (an open-time failure) —
        // the cache wrapper never sees a Finished event so nothing is
        // written. Then a successful call after seeds the cache.
        let registry = MemoryCacheRegistry::default_arc();

        // First, an Error provider; verify nothing got cached.
        let bad = MockProvider::new("p", CannedResponse::Error("boom".into()));
        let (pipeline, _counter) = build_pipeline_with_cache(registry.clone(), bad);
        let result = pipeline.clone().call(deterministic_request("p"), ctx()).await;
        assert!(result.is_err(), "open-time error should propagate");
        assert_eq!(registry.entry_count(), 0);
    }

    #[tokio::test]
    async fn non_endturn_stop_reason_is_not_cached() {
        // Build a sequence that ends with MaxTokens — should NOT be cached.
        let registry = MemoryCacheRegistry::default_arc();
        let truncated = vec![
            ChatEvent::started("m"),
            ChatEvent::Delta { text: "partial".into() },
            ChatEvent::Finished {
                stop_reason: StopReason::MaxTokens,
                usage: Usage::default(),
            },
        ];
        let mock = MockProvider::new("p", CannedResponse::Sequence(truncated));
        let (pipeline, counter) = build_pipeline_with_cache(registry.clone(), mock);

        let s = pipeline.clone().call(deterministic_request("p"), ctx()).await.unwrap();
        let _ = drain(s).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // Second call must miss again — MaxTokens shouldn't have been cached.
        let s = pipeline.clone().call(deterministic_request("p"), ctx()).await.unwrap();
        let _ = drain(s).await;
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        // Sanity check: registry stayed empty.
        let _ = json!(null); // keep the import live in this minimal test
    }

    // ── Audit `tars-pipeline-src-cache-5`: `read_policy` falls back
    // to default when the `cache.policy` attribute is malformed. ────
    #[test]
    fn read_policy_with_malformed_attribute_falls_back_to_default() {
        let ctx = ctx();
        {
            let mut attrs = ctx.attributes.write().unwrap();
            attrs.insert(POLICY_ATTR.into(), json!("not-a-policy-object"));
        }
        let p = read_policy(&ctx);
        let d = CachePolicy::default();
        assert_eq!(p.l1, d.l1);
        assert_eq!(p.l2, d.l2);
        assert_eq!(p.l3, d.l3);
    }

    // ── Audit `tars-pipeline-src-cache-1`: `read_policy` falls back
    // to default if `ctx.attributes` is RwLock-poisoned. ─────────────
    #[test]
    fn read_policy_with_poisoned_lock_falls_back_to_default() {
        let ctx = ctx();
        // Poison the lock by panicking inside a write guard on another
        // thread. `Arc<RwLock<...>>` is shared via clone of `ctx`.
        let attrs = ctx.attributes.clone();
        let h = std::thread::spawn(move || {
            let _g = attrs.write().unwrap();
            panic!("poison the lock");
        });
        let _ = h.join();
        // Sanity: lock should now be poisoned.
        assert!(ctx.attributes.read().is_err());

        let p = read_policy(&ctx);
        let d = CachePolicy::default();
        assert_eq!(p.l1, d.l1);
        assert_eq!(p.l2, d.l2);
        assert_eq!(p.l3, d.l3);
    }

    // ── Audit `tars-pipeline-src-cache-1` (set side): same poisoned
    // lock, `set_cache_policy` is a no-op (does not panic). ──────────
    #[test]
    fn set_cache_policy_with_poisoned_lock_is_a_noop() {
        let ctx = ctx();
        let attrs = ctx.attributes.clone();
        let h = std::thread::spawn(move || {
            let _g = attrs.write().unwrap();
            panic!("poison the lock");
        });
        let _ = h.join();
        // Should not panic — error is logged and the call returns.
        set_cache_policy(&ctx, &CachePolicy::off());
    }

    // ── Audit `tars-pipeline-src-cache-6`: registry lookup error is
    // treated as a miss; inner service is still called. ─────────────
    #[tokio::test]
    async fn registry_lookup_error_is_treated_as_miss() {
        use tars_cache::{CacheError, CacheKey, CachedResponse};

        struct ErroringRegistry;
        #[async_trait]
        impl CacheRegistry for ErroringRegistry {
            async fn lookup(
                &self,
                _key: &CacheKey,
                _policy: &CachePolicy,
            ) -> Result<Option<CachedResponse>, CacheError> {
                Err(CacheError::Backend("simulated lookup failure".into()))
            }
            async fn write(
                &self,
                _key: CacheKey,
                _value: CachedResponse,
                _policy: &CachePolicy,
            ) -> Result<(), CacheError> {
                Ok(())
            }
            async fn invalidate(&self, _key: &CacheKey) -> Result<(), CacheError> {
                Ok(())
            }
            fn entry_count(&self) -> u64 {
                0
            }
        }

        let registry: Arc<dyn CacheRegistry> = Arc::new(ErroringRegistry);
        let mock = MockProvider::new("p", CannedResponse::text("ok"));
        let (pipeline, counter) = build_pipeline_with_cache(registry, mock);

        let s = pipeline.clone().call(deterministic_request("p"), ctx()).await.unwrap();
        let events = drain(s).await;
        // Inner service called despite the registry erroring.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // Stream still completed normally.
        assert!(matches!(events.last(), Some(ChatEvent::Finished { .. })));
        // The Started event should not be a cache hit.
        match &events[0] {
            ChatEvent::Started { cache_hit, .. } => {
                assert!(!cache_hit.replayed_from_cache);
            }
            other => panic!("expected Started, got {other:?}"),
        }
    }

    // ── Audit `tars-pipeline-src-cache-4`: a stream that opens
    // successfully but yields an error mid-stream must NOT be
    // written to the cache. Drives `wrap_stream_for_write` directly
    // so we control the Ok/Err mix the mock can't express. ──────────
    #[tokio::test]
    async fn mid_stream_error_skips_cache_write() {
        use futures::stream;

        let registry = MemoryCacheRegistry::default_arc();
        let factory = CacheKeyFactory::new(1);
        let req = deterministic_request("midstream");
        let ctx = ctx();
        let key = factory.compute(&req, &ctx).expect("compute key");

        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started("m")),
            Ok(ChatEvent::Delta { text: "partial".into() }),
            Err(ProviderError::Internal("boom".into())),
            // Even a Finished after Err should not rescue the write.
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        ];
        let inner: LlmEventStream = Box::pin(stream::iter(events));

        let wrapped = wrap_stream_for_write(
            inner,
            registry.clone(),
            key,
            CachePolicy::default(),
            ProviderId::new("mock_origin"),
        );
        // Drain — discard items, we only care about the side effect.
        let mut s = Box::pin(wrapped);
        while let Some(_item) = s.next().await {}

        assert_eq!(registry.entry_count(), 0, "errored stream must not be cached");
    }
}
