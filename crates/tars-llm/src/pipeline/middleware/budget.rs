//! Refuses a call, before it is made, whose estimated USD cost exceeds a per-call cap.
//!
//! The cap applies to one request, not to a running total; [`TenantBudgetMiddleware`] keeps the
//! running total. The estimate is [`Pricing::estimate_chat_cost`]: bytes/4 input tokens plus the
//! request's `max_output_tokens` (which `LlmService` fills from the model's output limit), else
//! the output bound the middleware was built with. It is not
//! an upper bound: it undercounts multi-byte scripts and images, and providers that bill thinking
//! separately (Gemini) are undercounted by their thinking cap.
//!
//! A provider whose `Pricing` is all zeros (the subscription-billed CLI backends) is passed
//! through, with one warning per middleware instance so a paid provider with an empty price
//! table is noticed.
//!
//! [`TenantBudgetMiddleware`]: crate::TenantBudgetMiddleware

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use crate::LlmError;
use crate::LlmEventStream;
use tars_types::{Caller, ChatRequest, Pricing, ProviderProfile};

use crate::pipeline::middleware::{Middleware, OutputReserve};
use crate::pipeline::service::{Next, OpenFailure};

/// Refuses any call whose estimated USD cost exceeds `cap_usd`.
#[derive(Debug)]
pub struct PerCallBudgetMiddleware {
    cap_usd: f64,
    pricing: Pricing,
    default_max_output_tokens: u32,
    /// Set by the first call on a zero-pricing provider, so only that call warns.
    zero_pricing_warned: AtomicBool,
}

/// The reason a budget middleware refuses its configuration: a cap that is NaN, infinite or
/// negative, any of which would break budgeting silently.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BudgetConfigError {
    #[error("PerCallBudgetMiddleware cap_usd must be finite and non-negative, got {value}")]
    InvalidCap { value: f64 },
}

impl PerCallBudgetMiddleware {
    /// Builds the middleware from a provider's pricing, assuming the default [`OutputReserve`] for a
    /// request that reaches it with no `max_output_tokens`.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetConfigError`] when `cap_usd` is NaN, infinite or negative.
    pub fn new(cap_usd: f64, capabilities: &ProviderProfile) -> Result<Self, BudgetConfigError> {
        Self::from_parts(
            cap_usd,
            capabilities.pricing,
            OutputReserve::default().output_tokens.get(),
        )
    }

    /// Builds the middleware from explicit pricing and the output tokens to assume when a request
    /// sets none.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetConfigError`] when `cap_usd` is NaN, infinite or negative.
    pub fn from_parts(
        cap_usd: f64,
        pricing: Pricing,
        default_max_output_tokens: u32,
    ) -> Result<Self, BudgetConfigError> {
        validate_cap(cap_usd)?;
        Ok(Self {
            cap_usd,
            pricing,
            default_max_output_tokens,
            zero_pricing_warned: AtomicBool::new(false),
        })
    }

    fn is_zero_pricing(&self) -> bool {
        self.pricing.is_zero()
    }

    fn estimate_cost_usd(&self, req: &ChatRequest) -> f64 {
        self.pricing
            .estimate_chat_cost(req, self.default_max_output_tokens)
    }
}

/// Rejects a NaN cap, which would make `estimate > cap_usd` always false and disable the budget,
/// and a negative one, which would refuse every call.
fn validate_cap(cap_usd: f64) -> Result<(), BudgetConfigError> {
    if cap_usd.is_finite() && cap_usd >= 0.0 {
        Ok(())
    } else {
        Err(BudgetConfigError::InvalidCap { value: cap_usd })
    }
}

#[async_trait]
impl Middleware for PerCallBudgetMiddleware {
    fn name(&self) -> &'static str {
        "per_call_budget"
    }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: Caller,
        next: Next<'_>,
    ) -> Result<LlmEventStream, OpenFailure> {
        if self.is_zero_pricing() {
            if !self.zero_pricing_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    event = "per_call_budget.zero_pricing",
                    cap_usd = self.cap_usd,
                    tars.trace_id = %ctx.trace_id,
                    "per-call budget cap is a no-op on this provider \
                     (Pricing has zero input+output rates — typical for \
                     subscription-billed CLI backends)",
                );
            }
            return next.run(req, ctx).await;
        }

        let estimate = self.estimate_cost_usd(&req);
        // An estimate exactly at the cap passes, as in the tenant budget's `estimate > remaining`.
        if estimate > self.cap_usd {
            tracing::warn!(
                event = "per_call_budget.exceeded",
                estimate_usd = estimate,
                cap_usd = self.cap_usd,
                tars.trace_id = %ctx.trace_id,
                tenant_id = %ctx.tenant_id,
            );
            return Err(LlmError::BudgetExceeded.into());
        }

        tracing::debug!(
            event = "per_call_budget.checked",
            estimate_usd = estimate,
            cap_usd = self.cap_usd,
            tars.trace_id = %ctx.trace_id,
        );
        next.run(req, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmService;
    use crate::ProviderError;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use crate::LlmProvider;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use futures::StreamExt;
    use tars_types::{ChatEvent, ProviderId};

    fn priced(input: f64, output: f64) -> Pricing {
        Pricing {
            input_per_million: tars_types::Rate::new(input).unwrap(),
            output_per_million: tars_types::Rate::new(output).unwrap(),
            ..Pricing::default()
        }
    }

    /// Returns a provider that counts its calls, so a test can tell whether the budget layer let
    /// a call through.
    fn ok_service() -> (Arc<dyn LlmProvider>, Arc<AtomicU32>) {
        let observed = Arc::new(AtomicU32::new(0));
        struct Count {
            id: ProviderId,
            caps: ProviderProfile,
            inner: Arc<dyn LlmProvider>,
            observed: Arc<AtomicU32>,
        }
        #[async_trait]
        impl LlmProvider for Count {
            fn id(&self) -> &ProviderId {
                &self.id
            }
            fn capabilities(&self) -> &ProviderProfile {
                &self.caps
            }
            async fn stream(
                self: Arc<Self>,
                req: ChatRequest,
                model: &str,
                ctx: Caller,
            ) -> Result<LlmEventStream, ProviderError> {
                self.observed.fetch_add(1, Ordering::SeqCst);
                self.inner.clone().stream(req, model, ctx).await
            }
        }
        let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
        (
            Arc::new(Count {
                id: ProviderId::new("count"),
                // No window limits: the tests send prompts far past any real window to reach
                // the budget, and the pipeline would refuse them before it.
                caps: ProviderProfile {
                    max_context_tokens: None,
                    ..ProviderProfile::text_only_baseline(Pricing::default())
                },
                inner: mock,
                observed: observed.clone(),
            }) as Arc<dyn LlmProvider>,
            observed,
        )
    }

    fn req_with_text(text: &str, max_out: Option<u32>) -> ChatRequest {
        let mut r = ChatRequest::user(text);
        r.max_output_tokens = max_out;
        r
    }

    async fn drain(stream: LlmEventStream) {
        let mut s = stream;
        while let Some(ev) = s.next().await {
            let ev = ev.expect("drain: stream yielded a provider error");
            if matches!(ev, ChatEvent::Finished { .. }) {
                break;
            }
        }
    }

    #[tokio::test]
    async fn under_cap_passes_through() {
        // 25 × 3/1M + 1000 × 15/1M = $0.015075, under the $0.05 cap.
        let mw = PerCallBudgetMiddleware::from_parts(0.05, priced(3.0, 15.0), 1000).unwrap();
        let (inner, observed) = ok_service();
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        let stream = svc
            .call(
                req_with_text(&"x".repeat(100), Some(1000)),
                Caller::test_default(),
            )
            .await
            .result
            .unwrap();
        drain(stream).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn over_cap_rejects_before_inner() {
        // 1M chars ≈ 250k input tokens; at $3/M that is $0.75, over the $0.05 cap.
        let mw = PerCallBudgetMiddleware::from_parts(0.05, priced(3.0, 15.0), 1000).unwrap();
        let (inner, observed) = ok_service();
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        let err = svc
            .call(
                req_with_text(&"x".repeat(1_000_000), Some(1000)),
                Caller::test_default(),
            )
            .await
            .result
            .err()
            .expect("must reject");
        assert!(matches!(err, LlmError::BudgetExceeded));
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn zero_pricing_passes_through_and_warns_once() {
        let mw = PerCallBudgetMiddleware::from_parts(0.05, Pricing::default(), 1000).unwrap();
        let (inner, observed) = ok_service();
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        // Two calls with massive input — both must pass since pricing is 0.
        for _ in 0..2 {
            let stream = svc
                .clone()
                .call(
                    req_with_text(&"x".repeat(10_000_000), Some(1_000_000)),
                    Caller::test_default(),
                )
                .await
                .result
                .unwrap();
            drain(stream).await;
        }
        assert_eq!(observed.load(Ordering::SeqCst), 2);
        // That the warning fires once is not asserted: that would need a tracing subscriber.
    }

    #[test]
    fn zero_pricing_warn_flag_flips_exactly_once() {
        let svc = PerCallBudgetMiddleware::from_parts(0.01, Pricing::default(), 1000).unwrap();
        assert!(!svc.zero_pricing_warned.swap(true, Ordering::Relaxed));
        assert!(svc.zero_pricing_warned.swap(true, Ordering::Relaxed));
    }

    #[tokio::test]
    async fn falls_back_to_capabilities_max_output_when_request_unset() {
        // 8000 × $15/M = $0.12, over the $0.05 cap.
        let mw = PerCallBudgetMiddleware::from_parts(0.05, priced(3.0, 15.0), 8000).unwrap();
        let (inner, observed) = ok_service();
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        let err = svc
            .call(req_with_text("hi", None), Caller::test_default())
            .await
            .result
            .err()
            .expect("over cap due to default_max_output_tokens");
        assert!(matches!(err, LlmError::BudgetExceeded));
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn estimator_matches_documented_formula() {
        let svc = PerCallBudgetMiddleware::from_parts(1.0, priced(3.0, 15.0), 0).unwrap();
        // 400 chars → 100 tokens; 100 × 3 / 1e6 + 2000 × 15 / 1e6 = 0.0303.
        let req = req_with_text(&"a".repeat(400), Some(2000));
        let est = svc.estimate_cost_usd(&req);
        assert!((est - 0.0303).abs() < 1e-9, "got {est}");
    }

    #[test]
    fn pricing_new_from_capabilities() {
        let caps = ProviderProfile::text_only_baseline(priced(3.0, 15.0));
        let mw = PerCallBudgetMiddleware::new(0.10, &caps).unwrap();
        assert_eq!(mw.pricing.input_per_million.usd_per_million(), 3.0);
        assert_eq!(mw.default_max_output_tokens, 4096);
        assert_eq!(mw.cap_usd, 0.10);
    }

    #[test]
    fn from_parts_rejects_nan_cap() {
        let err =
            PerCallBudgetMiddleware::from_parts(f64::NAN, priced(3.0, 15.0), 1000).unwrap_err();
        assert!(matches!(err, BudgetConfigError::InvalidCap { value } if value.is_nan()));
    }

    #[test]
    fn from_parts_rejects_negative_cap() {
        let err = PerCallBudgetMiddleware::from_parts(-1.0, priced(3.0, 15.0), 1000).unwrap_err();
        assert!(matches!(err, BudgetConfigError::InvalidCap { value } if value < 0.0));
    }

    #[test]
    fn from_parts_happy_path_constructs() {
        let mw = PerCallBudgetMiddleware::from_parts(0.05, priced(3.0, 15.0), 1000)
            .expect("valid config");
        assert_eq!(mw.cap_usd, 0.05);
        assert_eq!(mw.default_max_output_tokens, 1000);
    }
}
