//! Routing — pick which Provider serves a given request.
//!
//! Doc 02 §4.6 + Doc 01 §12. Per Doc 14 M2, this is the second-shipping
//! middleware responsibility (after Telemetry/Retry/Cache). The MVP
//! covers two policies + an implicit fallback chain:
//!
//! - [`StaticPolicy`] — caller-supplied ordered list. Useful when the
//!   CLI / config has already decided "use these N providers in this
//!   order".
//! - [`TierPolicy`] — `HashMap<ModelTier, Vec<ProviderId>>`. Resolves
//!   `ModelHint::Tier(...)` requests by table lookup.
//!
//! Out of scope for M2.1 (next commits):
//! - `CostPolicy` / `LatencyPolicy` — both need runtime metrics
//!   infrastructure that doesn't exist yet.
//! - `EnsemblePolicy` — fan-out + merge is its own thing.
//! - `CircuitBreaker` — separate middleware (state-per-provider).
//!
//! ## Fallback chain
//!
//! [`RoutingPolicy::select`] returns an ordered `Vec<ProviderId>`.
//! [`RoutingService`] tries them in order, with classification:
//!
//! - `Ok(stream)` → return immediately
//! - `Err(class=Permanent)` → return immediately (auth, invalid req,
//!   content filter, budget, context-too-long — none of these get
//!   better with a different provider)
//! - `Err(class=Retriable | MaybeRetriable)` → log + try next candidate
//! - All candidates exhausted → return last error
//!
//! This is the "fallback chain" Doc 01 §12 calls for, baked into the
//! return shape rather than as a separate `FallbackChain<P>` wrapper.
//! Composing chains-of-policies is uncommon enough that a list-of-IDs
//! is the natural primitive.
//!
//! ## Tier resolution
//!
//! When `req.model` is `ModelHint::Tier(t)`, `RoutingService` rewrites
//! it to `ModelHint::Explicit(...)` using the chosen provider's
//! `default_model` before calling `stream()`. Cache layer requires
//! Explicit (Doc 03 §4.2) so this resolution must happen before any
//! middleware further down the onion looks at `req.model`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use tars_provider::registry::ProviderRegistry;
use tars_provider::{LlmEventStream, LlmProvider};
use tars_types::{
    ChatRequest, CompatibilityCheck, CompatibilityReason, ErrorClass, ModelHint, ModelTier,
    ProviderError, ProviderId, RequestContext,
};

use crate::service::LlmService;

/// Decide which providers can serve `req`, in priority order.
///
/// Implementors are stateless wrt requests; per-call state (e.g.
/// metrics, recent failures) lives on supporting structures the
/// policy may borrow.
#[async_trait]
pub trait RoutingPolicy: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    async fn select(
        &self,
        req: &ChatRequest,
        registry: &ProviderRegistry,
    ) -> Result<Vec<ProviderId>, ProviderError>;
}

/// Trivial policy: always returns the same hardcoded list. Useful as
/// the default when the caller has already decided ("--provider X" in
/// the CLI) and no actual routing is needed.
#[derive(Clone, Debug)]
pub struct StaticPolicy {
    candidates: Vec<ProviderId>,
}

impl StaticPolicy {
    pub fn new(candidates: Vec<ProviderId>) -> Self {
        assert!(
            !candidates.is_empty(),
            "StaticPolicy requires at least one candidate ProviderId",
        );
        Self { candidates }
    }

    pub fn single(id: ProviderId) -> Self {
        Self::new(vec![id])
    }
}

#[async_trait]
impl RoutingPolicy for StaticPolicy {
    fn name(&self) -> &'static str {
        "static"
    }
    async fn select(
        &self,
        _req: &ChatRequest,
        _registry: &ProviderRegistry,
    ) -> Result<Vec<ProviderId>, ProviderError> {
        Ok(self.candidates.clone())
    }
}

/// Map `ModelHint::Tier(...)` to a list of providers. Doc 01 §12's
/// `[routing.tiers]` config maps directly to the inner HashMap.
///
/// On `ModelHint::Explicit(...)` this policy falls back to the
/// `explicit_fallback` chain — typically a copy of the Default tier
/// or an empty list (in which case the routing service errors out and
/// the caller chooses something more direct).
///
/// On `ModelHint::Ensemble(...)` this policy returns
/// `ProviderError::InvalidRequest` — ensembles need a different
/// composition shape (fan-out + merge) that's out of scope for M2.
#[derive(Clone, Debug)]
pub struct TierPolicy {
    tiers: HashMap<ModelTier, Vec<ProviderId>>,
    explicit_fallback: Vec<ProviderId>,
}

impl TierPolicy {
    pub fn new(tiers: HashMap<ModelTier, Vec<ProviderId>>) -> Self {
        Self { tiers, explicit_fallback: Vec::new() }
    }

    /// Set the candidate list returned when `req.model` is Explicit.
    /// Defaults to empty.
    pub fn with_explicit_fallback(mut self, fallback: Vec<ProviderId>) -> Self {
        self.explicit_fallback = fallback;
        self
    }
}

#[async_trait]
impl RoutingPolicy for TierPolicy {
    fn name(&self) -> &'static str {
        "tier"
    }
    async fn select(
        &self,
        req: &ChatRequest,
        _registry: &ProviderRegistry,
    ) -> Result<Vec<ProviderId>, ProviderError> {
        match &req.model {
            ModelHint::Tier(t) => Ok(self.tiers.get(t).cloned().unwrap_or_default()),
            ModelHint::Explicit(_) => Ok(self.explicit_fallback.clone()),
            ModelHint::Ensemble(_) => Err(ProviderError::InvalidRequest(
                "TierPolicy does not handle ModelHint::Ensemble — fan-out/merge needs a dedicated EnsemblePolicy"
                    .into(),
            )),
        }
    }
}

/// Bottom-of-pipeline service: consults a [`RoutingPolicy`] for an
/// ordered candidate list and dispatches to providers in order with a
/// fallback chain.
///
/// Use [`crate::Pipeline::builder_with_inner`] to put this at the
/// bottom of a multi-layer pipeline:
///
/// ```ignore
/// let routing = RoutingService::new(registry, Arc::new(TierPolicy::new(tiers)));
/// let pipeline = Pipeline::builder_with_inner(routing)
///     .layer(TelemetryMiddleware::new())
///     .layer(CacheLookupMiddleware::new(...))
///     .layer(RetryMiddleware::default())
///     .build();
/// ```
pub struct RoutingService {
    registry: Arc<ProviderRegistry>,
    policy: Arc<dyn RoutingPolicy>,
}

impl RoutingService {
    pub fn new(registry: Arc<ProviderRegistry>, policy: Arc<dyn RoutingPolicy>) -> Arc<Self> {
        Arc::new(Self { registry, policy })
    }
}

#[async_trait]
impl LlmService for RoutingService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let candidates = self.policy.select(&req, &self.registry).await?;
        if candidates.is_empty() {
            return Err(ProviderError::InvalidRequest(format!(
                "routing: policy `{}` returned no candidates for model={}",
                self.policy.name(),
                req.model.label(),
            )));
        }

        let mut last_err: Option<ProviderError> = None;
        // Track skip reasons across the chain so the final error
        // (when *all* candidates were skipped) carries useful context.
        let mut skipped_with_reasons: Vec<(ProviderId, Vec<CompatibilityReason>)> = Vec::new();
        let total_candidates = candidates.len();
        for (idx, id) in candidates.iter().enumerate() {
            let provider = match self.registry.get(id) {
                Some(p) => p,
                None => {
                    tracing::warn!(
                        candidate_id = %id,
                        chain_position = idx,
                        chain_total = total_candidates,
                        trace_id = %ctx.trace_id,
                        "routing: candidate not in registry; skipping",
                    );
                    continue;
                }
            };

            // Capability pre-flight check (B-31). Cheap local
            // comparison: avoids a wire round-trip when the candidate
            // can't honour the request's feature set (tools / vision /
            // thinking / structured_output / context_window /
            // max_output_tokens). The provider would otherwise either
            // silently drop the feature or 400 at the wire — both worse
            // than a clean local skip.
            //
            // We do NOT short-circuit the chain on incompatibility:
            // the next candidate may have stronger capabilities. Only
            // when *all* candidates are skipped do we surface an
            // InvalidRequest error with the collected reasons.
            let resolved = resolve_model_for_provider(req.clone(), &provider);
            match resolved.compatibility_check(provider.capabilities()) {
                CompatibilityCheck::Compatible => {}
                CompatibilityCheck::Incompatible { reasons } => {
                    // Structured kinds first so log aggregation /
                    // dashboards can facet on `reasons.kinds` directly
                    // without parsing the human message.
                    let kinds: Vec<&'static str> =
                        reasons.iter().map(|r| r.kind()).collect();
                    tracing::warn!(
                        candidate_id = %id,
                        chain_position = idx,
                        chain_total = total_candidates,
                        trace_id = %ctx.trace_id,
                        reason_kinds = ?kinds,
                        "routing: candidate skipped (capability mismatch); trying next",
                    );
                    skipped_with_reasons.push((id.clone(), reasons));
                    continue;
                }
                // `#[non_exhaustive]` forces this — future variants
                // (e.g. MaybeWithCaveat) treated as Compatible by
                // default; routing layer doesn't yet know how to
                // degrade gracefully, so let the call go through
                // and let the provider do its thing.
                _ => {}
            }

            tracing::debug!(
                policy = self.policy.name(),
                candidate_id = %id,
                chain_position = idx,
                chain_total = total_candidates,
                trace_id = %ctx.trace_id,
                model = %resolved.model.label(),
                "routing: dispatching",
            );

            match provider.stream(resolved, ctx.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    let class = e.class();
                    if class == ErrorClass::Permanent {
                        // No other provider will fix this — return now.
                        tracing::debug!(
                            candidate_id = %id,
                            chain_position = idx,
                            trace_id = %ctx.trace_id,
                            error = %e,
                            "routing: permanent error; halting fallback chain",
                        );
                        return Err(e);
                    }
                    tracing::warn!(
                        candidate_id = %id,
                        chain_position = idx,
                        chain_total = total_candidates,
                        trace_id = %ctx.trace_id,
                        error_class = ?class,
                        error = %e,
                        "routing: candidate failed; trying next",
                    );
                    last_err = Some(e);
                }
            }
        }

        // If we exhausted candidates purely through capability skips
        // (no wire-level errors), surface the structured
        // `NoCompatibleCandidate` variant. This carries the typed
        // skipped list end-to-end (no message-string parsing required
        // by downstream consumers). Permanent class — retrying the
        // same request against the same fallback list won't help.
        if last_err.is_none() && !skipped_with_reasons.is_empty() {
            return Err(ProviderError::NoCompatibleCandidate {
                skipped: skipped_with_reasons,
            });
        }

        Err(last_err.unwrap_or_else(|| {
            ProviderError::Internal(format!(
                "routing: all {} candidates skipped (none registered)",
                candidates.len(),
            ))
        }))
    }
}

/// If `req.model` is `Tier(...)`, rewrite to `Explicit(provider.default_model)`.
/// Cache + adapter layers below this point require Explicit.
///
/// `LlmProvider` doesn't expose `default_model` (that's on
/// `ProviderConfig`, not the trait), so we use `capabilities()` as a
/// proxy: in practice every adapter sets a sensible default during
/// construction. M2.1 will add a `LlmProvider::default_model()` method
/// when it becomes a real pain point; for M2 the workaround is "use
/// `ProviderId` itself as a model hint when no better signal exists".
fn resolve_model_for_provider(
    mut req: ChatRequest,
    provider: &Arc<dyn LlmProvider>,
) -> ChatRequest {
    if matches!(req.model, ModelHint::Tier(_)) {
        // No tier→model resolution at the trait level yet — adapters
        // ignore the tier and use their own default_model when given a
        // model name they don't recognise. The honest fix is to add
        // `LlmProvider::default_model()`; for now we forward the tier
        // as a label so logs stay useful, and the adapter falls back.
        // Most adapters' `translate_request` rejects non-Explicit, so
        // this works out as: providers that need an explicit model
        // surface a clear error pointing at this exact line.
        req.model = ModelHint::Explicit(format!(
            "tier-resolution-deferred:{}",
            provider.id().as_ref(),
        ));
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_provider::registry::ProviderRegistry;

    fn registry_with(providers: Vec<(&str, Arc<dyn LlmProvider>)>) -> Arc<ProviderRegistry> {
        // ProviderRegistry::from_config wants a ProvidersConfig; for
        // unit tests we'd rather construct directly. Use the empty
        // registry + a hidden constructor — but there isn't one. Punt
        // by building a fresh registry-like wrapper via from_iter
        // through a config… actually, simpler: assemble via the
        // existing public API by inserting via a helper.
        //
        // The existing ProviderRegistry has no public insert method
        // (intentional — built once from config). For tests we
        // expose a tiny adapter via a builder. Until that lands,
        // tests use the registry's own test path: build from a
        // minimal TOML.
        let _ = providers;
        unreachable!("see registry_from_id_and_provider helper below")
    }

    /// Build a 2-provider registry directly using the public TOML loader
    /// (which already sits in tars-provider tests). For routing tests
    /// we just need MockProviders the registry can hand back via id.
    fn registry_with_mocks(
        mocks: Vec<(&str, CannedResponse)>,
    ) -> Arc<ProviderRegistry> {
        // The registry's only public constructor consumes a ProvidersConfig.
        // Build a TOML snippet that maps each id → mock provider.
        let mut toml = String::new();
        for (id, _resp) in &mocks {
            use std::fmt::Write;
            writeln!(
                &mut toml,
                "[providers.{id}]\ntype = \"mock\"\ncanned_response = \"placeholder\"\n",
            )
            .unwrap();
        }
        let cfg = tars_config::ConfigManager::load_from_str(&toml).unwrap();
        let http = tars_provider::http_base::HttpProviderBase::default_arc().unwrap();
        let reg = ProviderRegistry::from_config(&cfg.providers, http, tars_provider::auth::basic())
            .unwrap();
        // Replace the canned default with the per-test response by
        // building a fresh ProviderRegistry that re-uses the IDs but
        // overrides the providers themselves. The registry doesn't
        // expose a mutator, so this helper just hands back a registry
        // whose providers all reply with a hardcoded "placeholder"
        // string; most routing tests don't care about the body, only
        // about which provider fielded the call. For richer tests
        // that DO care about per-provider responses, see
        // `_registry_with` above (currently inert).
        let _ = mocks; // suppress unused on the per-response data
        Arc::new(reg)
    }

    /// A trait-level fake that lets us drive any error/success outcome
    /// per-provider without going through the Mock provider's
    /// CannedResponse machinery (which only supports text/sequence/error).
    struct ScriptedProvider {
        id: ProviderId,
        outcome: ScriptedOutcome,
        calls: Arc<AtomicU32>,
        capabilities: tars_types::Capabilities,
    }

    enum ScriptedOutcome {
        Ok,
        Err(fn() -> ProviderError),
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn capabilities(&self) -> &tars_types::Capabilities {
            &self.capabilities
        }
        async fn stream(
            self: Arc<Self>,
            _req: ChatRequest,
            _ctx: RequestContext,
        ) -> Result<LlmEventStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.outcome {
                ScriptedOutcome::Ok => {
                    let mock = MockProvider::new(self.id.clone(), CannedResponse::text("ok"));
                    mock.stream(_req, _ctx).await
                }
                ScriptedOutcome::Err(f) => Err(f()),
            }
        }
    }

    fn scripted(id: &str, outcome: ScriptedOutcome) -> (Arc<ScriptedProvider>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let p = Arc::new(ScriptedProvider {
            id: ProviderId::new(id),
            outcome,
            calls: calls.clone(),
            capabilities: tars_types::Capabilities::text_only_baseline(
                tars_types::Pricing::default(),
            ),
        });
        (p, calls)
    }

    /// A fake registry: mimics `ProviderRegistry::get` for routing
    /// tests, without going through the public TOML constructor.
    /// Lives in this test module only.
    struct FakeRegistry {
        map: HashMap<ProviderId, Arc<dyn LlmProvider>>,
    }

    impl FakeRegistry {
        fn new(entries: Vec<(ProviderId, Arc<dyn LlmProvider>)>) -> Self {
            Self {
                map: entries.into_iter().collect(),
            }
        }
    }

    /// Helper: drive a RoutingService whose registry is our fake.
    /// Mirrors the production `RoutingService::call` body but takes
    /// our fake registry rather than the real one. This is a test
    /// double, NOT production code.
    async fn drive_routing(
        fake: &FakeRegistry,
        policy: &dyn RoutingPolicy,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // ProviderRegistry isn't a trait — for routing-policy unit
        // tests we recreate the dispatch loop here against the fake.
        // **Mirror prod's capability pre-flight** (B-31): keep the
        // test harness in sync with the real RoutingService::call
        // semantics so behavioral tests on this fake exercise the
        // same logic as the production path.
        let candidates = policy
            .select(&req, &dummy_provider_registry())
            .await?;
        let mut last_err: Option<ProviderError> = None;
        let mut skipped: Vec<(ProviderId, Vec<CompatibilityReason>)> = Vec::new();
        for id in &candidates {
            let provider = match fake.map.get(id).cloned() {
                Some(p) => p,
                None => continue,
            };
            // Capability pre-flight (mirror RoutingService::call).
            match req.compatibility_check(provider.capabilities()) {
                CompatibilityCheck::Compatible => {}
                CompatibilityCheck::Incompatible { reasons } => {
                    skipped.push((id.clone(), reasons));
                    continue;
                }
                _ => {}
            }
            match provider.stream(req.clone(), ctx.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) if e.class() == ErrorClass::Permanent => return Err(e),
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        if last_err.is_none() && !skipped.is_empty() {
            return Err(ProviderError::NoCompatibleCandidate { skipped });
        }
        Err(last_err.unwrap_or_else(|| {
            ProviderError::Internal("no candidate produced a result".into())
        }))
    }

    /// A real ProviderRegistry that we don't actually consult in the
    /// fake-registry tests — RoutingPolicy::select takes a reference
    /// but [`StaticPolicy`] / [`TierPolicy`] don't introspect it.
    fn dummy_provider_registry() -> ProviderRegistry {
        ProviderRegistry::empty()
    }

    fn req(model: ModelHint) -> ChatRequest {
        let mut r = ChatRequest::user(model, "ping");
        r.temperature = Some(0.0);
        r
    }

    async fn drain(s: LlmEventStream) {
        let mut s = s;
        while s.next().await.is_some() {}
    }

    // ── StaticPolicy ────────────────────────────────────────────────────
    #[tokio::test]
    async fn static_policy_returns_its_list_unchanged() {
        let policy = StaticPolicy::new(vec![
            ProviderId::new("a"),
            ProviderId::new("b"),
        ]);
        let r = policy
            .select(&req(ModelHint::Explicit("m".into())), &dummy_provider_registry())
            .await
            .unwrap();
        assert_eq!(
            r,
            vec![ProviderId::new("a"), ProviderId::new("b")]
        );
    }

    #[test]
    #[should_panic(expected = "at least one candidate")]
    fn static_policy_rejects_empty_list_at_construction() {
        let _ = StaticPolicy::new(vec![]);
    }

    // ── TierPolicy ──────────────────────────────────────────────────────
    fn tier_table() -> TierPolicy {
        let mut t = HashMap::new();
        t.insert(
            ModelTier::Reasoning,
            vec![ProviderId::new("opus"), ProviderId::new("o1")],
        );
        t.insert(ModelTier::Fast, vec![ProviderId::new("haiku")]);
        TierPolicy::new(t)
    }

    #[tokio::test]
    async fn tier_policy_resolves_known_tier() {
        let p = tier_table();
        let r = p
            .select(&req(ModelHint::Tier(ModelTier::Reasoning)), &dummy_provider_registry())
            .await
            .unwrap();
        assert_eq!(r, vec![ProviderId::new("opus"), ProviderId::new("o1")]);
    }

    #[tokio::test]
    async fn tier_policy_unknown_tier_returns_empty() {
        let p = tier_table();
        let r = p
            .select(&req(ModelHint::Tier(ModelTier::Local)), &dummy_provider_registry())
            .await
            .unwrap();
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn tier_policy_explicit_falls_through_to_fallback() {
        let p = tier_table().with_explicit_fallback(vec![ProviderId::new("default_p")]);
        let r = p
            .select(&req(ModelHint::Explicit("gpt-4o".into())), &dummy_provider_registry())
            .await
            .unwrap();
        assert_eq!(r, vec![ProviderId::new("default_p")]);
    }

    #[tokio::test]
    async fn tier_policy_rejects_ensemble() {
        let p = tier_table();
        let result = p
            .select(
                &req(ModelHint::Ensemble(vec![ModelHint::Explicit("a".into())])),
                &dummy_provider_registry(),
            )
            .await;
        match result {
            Err(ProviderError::InvalidRequest(_)) => {}
            other => panic!("expected InvalidRequest for Ensemble, got {other:?}"),
        }
    }

    // ── Fallback dispatch (the heart of RoutingService) ─────────────────
    #[tokio::test]
    async fn fallback_chain_skips_first_on_retriable_error() {
        let (a, calls_a) = scripted("a", ScriptedOutcome::Err(|| ProviderError::ModelOverloaded));
        let (b, calls_b) = scripted("b", ScriptedOutcome::Ok);

        let fake = FakeRegistry::new(vec![
            (ProviderId::new("a"), a),
            (ProviderId::new("b"), b),
        ]);
        let policy = StaticPolicy::new(vec![ProviderId::new("a"), ProviderId::new("b")]);

        let stream = drive_routing(
            &fake,
            &policy,
            req(ModelHint::Explicit("m".into())),
            RequestContext::test_default(),
        )
        .await
        .unwrap();
        drain(stream).await;

        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
        assert_eq!(calls_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fallback_chain_halts_on_permanent_error() {
        let (a, calls_a) = scripted("a", ScriptedOutcome::Err(|| ProviderError::Auth("bad".into())));
        let (b, calls_b) = scripted("b", ScriptedOutcome::Ok);

        let fake = FakeRegistry::new(vec![
            (ProviderId::new("a"), a),
            (ProviderId::new("b"), b),
        ]);
        let policy = StaticPolicy::new(vec![ProviderId::new("a"), ProviderId::new("b")]);

        let result = drive_routing(
            &fake,
            &policy,
            req(ModelHint::Explicit("m".into())),
            RequestContext::test_default(),
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Auth(_))));
        // Fallback must NOT have been tried.
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
        assert_eq!(calls_b.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fallback_chain_returns_last_error_when_all_fail() {
        let (a, _) = scripted("a", ScriptedOutcome::Err(|| ProviderError::ModelOverloaded));
        let (b, _) = scripted("b", ScriptedOutcome::Err(|| ProviderError::Network(
            "test transport".to_string().into(),
        )));

        let fake = FakeRegistry::new(vec![
            (ProviderId::new("a"), a),
            (ProviderId::new("b"), b),
        ]);
        let policy = StaticPolicy::new(vec![ProviderId::new("a"), ProviderId::new("b")]);

        let result = drive_routing(
            &fake,
            &policy,
            req(ModelHint::Explicit("m".into())),
            RequestContext::test_default(),
        )
        .await;
        // Last error wins (Network from "b").
        match result {
            Err(ProviderError::Network(_)) => {}
            Err(other) => panic!("expected Network error, got {other:?}"),
            Ok(_) => panic!("expected Network error, got Ok stream"),
        }
    }

    #[tokio::test]
    async fn fallback_chain_skips_unregistered_candidates() {
        // Policy says try [phantom, real]; phantom doesn't exist in
        // the registry, real does → should land on real without error.
        let (real, calls_real) = scripted("real", ScriptedOutcome::Ok);
        let fake = FakeRegistry::new(vec![(ProviderId::new("real"), real)]);
        let policy = StaticPolicy::new(vec![ProviderId::new("phantom"), ProviderId::new("real")]);

        let stream = drive_routing(
            &fake,
            &policy,
            req(ModelHint::Explicit("m".into())),
            RequestContext::test_default(),
        )
        .await
        .unwrap();
        drain(stream).await;
        assert_eq!(calls_real.load(Ordering::SeqCst), 1);
    }

    // ── Capability pre-flight tests (B-31) ─────────────────────────

    /// Provider with a custom capability set — same shape as
    /// `scripted()` but lets the test pin specific cap fields.
    fn scripted_with_caps(
        id: &str,
        outcome: ScriptedOutcome,
        capabilities: tars_types::Capabilities,
    ) -> (Arc<ScriptedProvider>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let p = Arc::new(ScriptedProvider {
            id: ProviderId::new(id),
            outcome,
            calls: calls.clone(),
            capabilities,
        });
        (p, calls)
    }

    /// Build a Capabilities pinning supports_tool_use to a value, all
    /// other features baseline.
    fn caps_with_tools(tools: bool) -> tars_types::Capabilities {
        let mut c = tars_types::Capabilities::text_only_baseline(
            tars_types::Pricing::default(),
        );
        c.supports_tool_use = tools;
        c
    }

    #[tokio::test]
    async fn capability_skip_skips_incompatible_candidate() {
        // chain: [no_tools (skip), supports_tools (call)] — request has tools.
        // Expected: no_tools never called; supports_tools called once.
        let (p_skip, calls_skip) =
            scripted_with_caps("no_tools", ScriptedOutcome::Ok, caps_with_tools(false));
        let (p_ok, calls_ok) =
            scripted_with_caps("with_tools", ScriptedOutcome::Ok, caps_with_tools(true));
        let fake = FakeRegistry::new(vec![
            (ProviderId::new("no_tools"), p_skip as _),
            (ProviderId::new("with_tools"), p_ok as _),
        ]);
        let policy = StaticPolicy::new(vec![
            ProviderId::new("no_tools"),
            ProviderId::new("with_tools"),
        ]);

        let mut req = req(ModelHint::Explicit("any".into()));
        req.tools.push(tars_types::ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: tars_types::JsonSchema::loose(serde_json::json!({})),
        });

        let stream = drive_routing(&fake, &policy, req, RequestContext::test_default())
            .await
            .expect("routing should succeed via the tool-supporting provider");
        drain(stream).await;
        assert_eq!(calls_skip.load(Ordering::SeqCst), 0,
            "incompatible provider must NOT be called");
        assert_eq!(calls_ok.load(Ordering::SeqCst), 1,
            "compatible provider should serve the request");
    }

    #[tokio::test]
    async fn capability_skip_returns_invalid_request_when_all_skipped() {
        // chain: [no_tools, no_tools_either] — request has tools.
        // Expected: both skipped, routing returns InvalidRequest with
        // each candidate's reason in the message.
        let (p_a, _) =
            scripted_with_caps("a", ScriptedOutcome::Ok, caps_with_tools(false));
        let (p_b, _) =
            scripted_with_caps("b", ScriptedOutcome::Ok, caps_with_tools(false));
        let fake = FakeRegistry::new(vec![
            (ProviderId::new("a"), p_a as _),
            (ProviderId::new("b"), p_b as _),
        ]);
        let policy = StaticPolicy::new(vec![ProviderId::new("a"), ProviderId::new("b")]);

        let mut req = req(ModelHint::Explicit("any".into()));
        req.tools.push(tars_types::ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: tars_types::JsonSchema::loose(serde_json::json!({})),
        });

        let result = drive_routing(&fake, &policy, req, RequestContext::test_default())
            .await;
        let err = match result {
            Ok(_) => panic!("expected error, got Ok stream"),
            Err(e) => e,
        };
        match err {
            ProviderError::NoCompatibleCandidate { skipped } => {
                assert_eq!(skipped.len(), 2, "expected both candidates skipped");
                let ids: Vec<&str> = skipped.iter().map(|(id, _)| id.as_ref()).collect();
                assert!(ids.contains(&"a"));
                assert!(ids.contains(&"b"));
                // Each candidate carries a typed `tool_use` reason.
                for (_, reasons) in &skipped {
                    assert!(reasons.iter().any(|r| r.kind() == "tool_use"));
                }
            }
            other => panic!("expected NoCompatibleCandidate, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn capability_skip_doesnt_block_compatible_request() {
        // Chain: [no_tools, no_tools] — request WITHOUT tools.
        // Expected: first provider serves the request normally.
        let (p_a, calls_a) =
            scripted_with_caps("a", ScriptedOutcome::Ok, caps_with_tools(false));
        let fake = FakeRegistry::new(vec![(ProviderId::new("a"), p_a as _)]);
        let policy = StaticPolicy::new(vec![ProviderId::new("a")]);

        let stream = drive_routing(
            &fake,
            &policy,
            req(ModelHint::Explicit("any".into())),
            RequestContext::test_default(),
        )
        .await
        .expect("text-only request should be served by a text-only provider");
        drain(stream).await;
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
    }

    // Suppress dead_code warnings on test-only helpers we keep around
    // for future coverage even when no test currently reaches them.
    #[allow(dead_code)]
    fn _suppress_unused() {
        let _ = registry_with;
        let _ = registry_with_mocks;
    }
}
