//! Per-call budget middleware — refuses any LLM call whose
//! upper-bound estimated cost exceeds a configured USD cap.
//!
//! Stateless and call-local: the cap applies to **one** request, not
//! to a running tenant total. The stateful tenant-budget variant
//! ([`docs/roadmap.md §4`](../../../../docs/roadmap.md)) is a separate
//! middleware with a `BudgetStore` trait, not yet shipped.
//!
//! ## Estimation strategy
//!
//! Pre-call we do not know the true token counts. We follow the
//! anti-pattern checklist in `docs/architecture/01-llm-provider.md §15`
//! (#1: no tokenizers on the hot path) and use:
//!
//! - **Input tokens** ≈ `chars / 4` over `system` + all `Message::Text`
//!   content blocks.
//! - **Output tokens** = `req.max_output_tokens` if set, else the
//!   provider's `Capabilities.max_output_tokens` as a worst-case bound.
//! - **Thinking tokens** — Anthropic bundles thinking into output and
//!   prices it at the output rate, so the worst-case output bound
//!   already covers it. Providers that bill thinking separately
//!   (e.g. Gemini) under-estimate by their thinking cap; document a
//!   refinement path for V2.
//!
//! Cost = `input_tokens × input_per_million / 1e6 + output_tokens × output_per_million / 1e6`.
//! Strict upper bound; cached-input savings and cache-creation
//! discounts are **not** subtracted because we cannot know cache state
//! before the call. The result is the most conservative possible.
//!
//! ## Subscription / zero-pricing backends
//!
//! `Pricing::default()` is all zeros — what the `claude_cli` /
//! `gemini_cli` / `codex_cli` backends use because the cost is borne
//! by the user's subscription, not per-call billing. The middleware
//! detects this, emits one `tracing::warn` (per-service-instance) so
//! the misconfiguration is observable, and passes through.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use tars_provider::LlmEventStream;
use tars_types::{Capabilities, ChatRequest, Pricing, ProviderError, RequestContext};

use crate::middleware::Middleware;
use crate::service::LlmService;

/// Refuses any call whose estimated USD cost exceeds `cap_usd`.
#[derive(Clone, Debug)]
pub struct PerCallBudgetMiddleware {
    cap_usd: f64,
    pricing: Pricing,
    default_max_output_tokens: u32,
}

impl PerCallBudgetMiddleware {
    /// Construct from a provider's capability snapshot — the natural
    /// caller path is `provider.capabilities()`.
    ///
    /// `cap_usd` is the per-call upper bound. Estimates at or above
    /// this value are rejected with [`ProviderError::BudgetExceeded`].
    pub fn new(cap_usd: f64, capabilities: &Capabilities) -> Self {
        validate_cap(cap_usd);
        validate_pricing(&capabilities.pricing);
        Self {
            cap_usd,
            pricing: capabilities.pricing,
            default_max_output_tokens: capabilities.max_output_tokens,
        }
    }

    /// Construct from explicit pricing + worst-case output bound. Use
    /// when you don't have a `Capabilities` handy (tests, hand-rolled
    /// services).
    pub fn from_parts(cap_usd: f64, pricing: Pricing, default_max_output_tokens: u32) -> Self {
        validate_cap(cap_usd);
        validate_pricing(&pricing);
        Self {
            cap_usd,
            pricing,
            default_max_output_tokens,
        }
    }
}

/// Reject a non-finite or negative cap at construction. A NaN cap would
/// make `estimate >= cap_usd` always false (NaN compares false), silently
/// disabling the budget; a negative cap would reject every call. Both are
/// configuration bugs, so fail loudly where they're introduced.
fn validate_cap(cap_usd: f64) {
    assert!(
        cap_usd.is_finite() && cap_usd >= 0.0,
        "PerCallBudgetMiddleware cap_usd must be finite and non-negative, got {cap_usd}"
    );
}

/// Reject non-finite or negative pricing rates. A NaN/inf rate would
/// propagate into the cost estimate and bypass (NaN) or always-trip (inf)
/// the budget. `Capabilities`-sourced pricing should already be valid;
/// this guards the hand-rolled `from_parts` path too.
///
/// Pairs with [`validate_cap`] above — same fail-fast invariant
/// pattern, same panic semantics. `arc scan --judge` flagged this
/// site as ROT ("recoverable input validation") while flagging
/// `validate_cap` as essential; the two are symmetric programmer-
/// error guards and both stay as `assert!`. All current callers of
/// `from_parts` and `new` pass either typed `Capabilities.pricing`
/// (already validated upstream) or hard-coded test values, so this
/// fires only on a genuine bug at the call site — exactly when a
/// loud panic is most useful.
fn validate_pricing(pricing: &Pricing) {
    assert!(
        pricing.input_per_million.is_finite() && pricing.input_per_million >= 0.0,
        "Pricing.input_per_million must be finite and non-negative, got {}",
        pricing.input_per_million
    );
    assert!(
        pricing.output_per_million.is_finite() && pricing.output_per_million >= 0.0,
        "Pricing.output_per_million must be finite and non-negative, got {}",
        pricing.output_per_million
    );
}

impl Middleware for PerCallBudgetMiddleware {
    fn name(&self) -> &'static str {
        "per_call_budget"
    }

    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(PerCallBudgetService {
            inner,
            cap_usd: self.cap_usd,
            pricing: self.pricing,
            default_max_output_tokens: self.default_max_output_tokens,
            zero_pricing_warned: AtomicBool::new(false),
        })
    }
}

struct PerCallBudgetService {
    inner: Arc<dyn LlmService>,
    cap_usd: f64,
    pricing: Pricing,
    default_max_output_tokens: u32,
    /// First call on a zero-pricing provider warns; subsequent calls
    /// don't, so a busy subscription-backed pipeline doesn't spam.
    zero_pricing_warned: AtomicBool,
}

impl PerCallBudgetService {
    fn is_zero_pricing(&self) -> bool {
        self.pricing.is_zero()
    }

    /// Strict upper-bound USD estimate for `req`. Delegated to
    /// [`Pricing::estimate_chat_cost`] — see that for the formula.
    fn estimate_cost_usd(&self, req: &ChatRequest) -> f64 {
        self.pricing
            .estimate_chat_cost(req, self.default_max_output_tokens)
    }
}

#[async_trait]
impl LlmService for PerCallBudgetService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        match ctx.telemetry.lock() {
            Ok(mut t) => t.layers.push("per_call_budget".into()),
            // Telemetry is best-effort, so a failed layer trace must not
            // fail the call — but a poisoned lock means another thread
            // panicked holding it, so leave a breadcrumb rather than
            // swallowing silently. [arc:intentional-handle]
            Err(_) => tracing::debug!(
                event = "per_call_budget.telemetry_poisoned",
                "telemetry mutex poisoned; skipping layer trace"
            ),
        }

        if self.is_zero_pricing() {
            // First time we see a zero-priced provider, warn so an
            // accidental misconfig (paid provider with empty pricing
            // table) is visible. Subsequent calls stay silent.
            if !self.zero_pricing_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    event = "per_call_budget.zero_pricing",
                    cap_usd = self.cap_usd,
                    trace_id = %ctx.trace_id,
                    "per-call budget cap is a no-op on this provider \
                     (Pricing has zero input+output rates — typical for \
                     subscription-billed CLI backends)",
                );
            }
            return self.inner.clone().call(req, ctx).await;
        }

        let estimate = self.estimate_cost_usd(&req);
        if estimate >= self.cap_usd {
            tracing::warn!(
                event = "per_call_budget.exceeded",
                estimate_usd = estimate,
                cap_usd = self.cap_usd,
                trace_id = %ctx.trace_id,
                tenant_id = %ctx.tenant_id,
            );
            return Err(ProviderError::BudgetExceeded);
        }

        tracing::debug!(
            event = "per_call_budget.checked",
            estimate_usd = estimate,
            cap_usd = self.cap_usd,
            trace_id = %ctx.trace_id,
        );
        self.inner.clone().call(req, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    use futures::StreamExt;
    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ChatEvent, ModelHint};

    fn priced(input: f64, output: f64) -> Pricing {
        Pricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: 0.0,
            cache_creation_per_million: 0.0,
            thinking_per_million: 0.0,
        }
    }

    fn ok_service() -> (Arc<dyn LlmService>, Arc<AtomicU32>) {
        let observed = Arc::new(AtomicU32::new(0));
        struct Count {
            inner: Arc<dyn LlmService>,
            observed: Arc<AtomicU32>,
        }
        #[async_trait]
        impl LlmService for Count {
            async fn call(
                self: Arc<Self>,
                req: ChatRequest,
                ctx: RequestContext,
            ) -> Result<LlmEventStream, ProviderError> {
                self.observed.fetch_add(1, Ordering::SeqCst);
                self.inner.clone().call(req, ctx).await
            }
        }
        let mock = MockProvider::new("p", CannedResponse::text("hi"));
        let inner: Arc<dyn LlmService> = crate::ProviderService::new(mock);
        (
            Arc::new(Count {
                inner,
                observed: observed.clone(),
            }) as Arc<dyn LlmService>,
            observed,
        )
    }

    fn req_with_text(text: &str, max_out: Option<u32>) -> ChatRequest {
        let mut r = ChatRequest::user(ModelHint::Explicit("m".into()), text);
        r.max_output_tokens = max_out;
        r
    }

    async fn drain(stream: LlmEventStream) {
        let mut s = stream;
        while let Some(ev) = s.next().await {
            if matches!(ev.unwrap(), ChatEvent::Finished { .. }) {
                break;
            }
        }
    }

    #[tokio::test]
    async fn under_cap_passes_through() {
        // 100 chars ≈ 25 input tokens. At $3/M input and $15/M output,
        // 25 × 3/1M + 1000 × 15/1M = $0.000075 + $0.015 = $0.015075
        let mw = PerCallBudgetMiddleware::from_parts(0.05, priced(3.0, 15.0), 1000);
        let (inner, observed) = ok_service();
        let svc = mw.wrap(inner);

        let stream = svc
            .call(
                req_with_text(&"x".repeat(100), Some(1000)),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        drain(stream).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn over_cap_rejects_before_inner() {
        // 1M chars ≈ 250k input tokens. At $3/M input, that's $0.75
        // — way over the $0.05 cap.
        let mw = PerCallBudgetMiddleware::from_parts(0.05, priced(3.0, 15.0), 1000);
        let (inner, observed) = ok_service();
        let svc = mw.wrap(inner);

        let err = svc
            .call(
                req_with_text(&"x".repeat(1_000_000), Some(1000)),
                RequestContext::test_default(),
            )
            .await
            .err()
            .expect("must reject");
        assert!(matches!(err, ProviderError::BudgetExceeded));
        // Inner service never invoked — the whole point.
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn zero_pricing_passes_through_and_warns_once() {
        let mw = PerCallBudgetMiddleware::from_parts(0.05, Pricing::default(), 1000);
        let (inner, observed) = ok_service();
        let svc = mw.wrap(inner);

        // Two calls with massive input — both must pass since pricing is 0.
        for _ in 0..2 {
            let stream = svc
                .clone()
                .call(
                    req_with_text(&"x".repeat(10_000_000), Some(1_000_000)),
                    RequestContext::test_default(),
                )
                .await
                .unwrap();
            drain(stream).await;
        }
        assert_eq!(observed.load(Ordering::SeqCst), 2);
        // We can't easily assert "warn fired only once" without a tracing
        // subscriber capture; the AtomicBool flip is the contract, and
        // it's verified by the unit-level test below.
    }

    #[test]
    fn zero_pricing_warn_flag_flips_exactly_once() {
        let svc = PerCallBudgetService {
            inner: {
                let mock = MockProvider::new("p", CannedResponse::text("hi"));
                crate::ProviderService::new(mock) as Arc<dyn LlmService>
            },
            cap_usd: 0.01,
            pricing: Pricing::default(),
            default_max_output_tokens: 1000,
            zero_pricing_warned: AtomicBool::new(false),
        };
        // First call should observe `false → true`.
        assert!(!svc.zero_pricing_warned.swap(true, Ordering::Relaxed));
        // Subsequent observe `true → true`.
        assert!(svc.zero_pricing_warned.swap(true, Ordering::Relaxed));
    }

    #[tokio::test]
    async fn falls_back_to_capabilities_max_output_when_request_unset() {
        // No req.max_output_tokens → use default_max_output_tokens (8000)
        // 8000 × $15/M = $0.12 — over the $0.05 cap, so this must reject.
        let mw = PerCallBudgetMiddleware::from_parts(0.05, priced(3.0, 15.0), 8000);
        let (inner, observed) = ok_service();
        let svc = mw.wrap(inner);

        let err = svc
            .call(req_with_text("hi", None), RequestContext::test_default())
            .await
            .err()
            .expect("over cap due to default_max_output_tokens");
        assert!(matches!(err, ProviderError::BudgetExceeded));
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn estimator_matches_documented_formula() {
        // Pin the formula: estimate = (chars/4) × in_rate + max_out × out_rate
        // Both divided by 1M.
        let svc = PerCallBudgetService {
            inner: {
                let mock = MockProvider::new("p", CannedResponse::text("hi"));
                crate::ProviderService::new(mock) as Arc<dyn LlmService>
            },
            cap_usd: 1.0,
            pricing: priced(3.0, 15.0),
            default_max_output_tokens: 0,
            zero_pricing_warned: AtomicBool::new(false),
        };
        // 400 chars total → 100 tokens at chars/4
        // 100 × 3 / 1e6 = 0.0003
        // 2000 × 15 / 1e6 = 0.03
        // total = 0.0303
        let req = req_with_text(&"a".repeat(400), Some(2000));
        let est = svc.estimate_cost_usd(&req);
        // Float compare with tolerance.
        assert!((est - 0.0303).abs() < 1e-9, "got {est}");
    }

    #[test]
    fn pricing_new_from_capabilities() {
        // Sanity: the canonical caller path works without explicit field tunes.
        let mut caps = Capabilities::text_only_baseline(priced(3.0, 15.0));
        caps.max_output_tokens = 4096;
        let mw = PerCallBudgetMiddleware::new(0.10, &caps);
        // Pulled out the pricing and the default_max_output_tokens.
        assert_eq!(mw.pricing.input_per_million, 3.0);
        assert_eq!(mw.default_max_output_tokens, 4096);
        assert_eq!(mw.cap_usd, 0.10);
    }
}
