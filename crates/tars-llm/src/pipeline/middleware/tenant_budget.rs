//! Caps a tenant's running USD total: refuses a call whose estimate exceeds what the tenant has
//! left, and debits the real cost when the stream ends, in `Finished` or in an error, from the
//! usage the provider reported by then.
//!
//! The cap is soft. Nothing is reserved before the call, so concurrent calls can all pass the
//! check and drive the balance below zero; a stream dropped before its last event is never
//! debited.
//! Hard accounting belongs in a [`BudgetStore`] with its own consistency model.
//!
//! # Composition
//!
//! ```ignore
//! let store = Arc::new(InMemoryBudgetStore::new());
//! store.set(&TenantId::new("acme"), 10.00).await;    // $10 cap for acme
//!
//! let pipeline = LlmService::builder(provider, "test-model")
//!     .layer(TelemetryMiddleware::new())
//!     .layer(PerCallBudgetMiddleware::new(0.05, &caps)?)   // per-call cap on the estimate
//!     .layer(TenantBudgetMiddleware::new(store, &caps))    // aggregate per-tenant
//!     .layer(RetryMiddleware::default())
//!     .build();
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::LlmError;
use crate::LlmEventStream;
use tars_types::{Caller, ChatRequest, Pricing, ProviderProfile, TenantId};
use tars_utils::display_chain;

use crate::pipeline::middleware::Middleware;
use crate::pipeline::middleware::OutputReserve;
use crate::pipeline::service::{Next, OpenFailure};

// ─── BudgetStore trait + reference impl ────────────────────────────────

/// Holds each tenant's remaining USD budget, under whatever consistency model the implementation
/// chooses.
///
/// `None` from `remaining()` or `debit()` means the tenant has no budget, which the middleware
/// treats as unlimited, so adding the middleware does not cut off existing tenants.
#[async_trait]
pub trait BudgetStore: Send + Sync + 'static {
    /// Returns the tenant's remaining USD; `None` is unlimited and `Some(0.0)` is exhausted.
    async fn remaining(&self, tenant: &TenantId) -> Result<Option<f64>, BudgetStoreError>;

    /// Debits `amount_usd` atomically and returns the new balance, or `None` for a tenant with no
    /// budget. A debit that drives the balance below zero is the soft cap working, not corruption,
    /// and must be accepted.
    async fn debit(
        &self,
        tenant: &TenantId,
        amount_usd: f64,
    ) -> Result<Option<f64>, BudgetStoreError>;
}

#[derive(Debug, Error)]
pub enum BudgetStoreError {
    /// The store's backend failed; the in-memory store never does, a remote one may.
    #[error("budget store backend error")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Keeps budgets in this process's memory, so two processes do not see each other's debits.
#[derive(Default)]
pub struct InMemoryBudgetStore {
    /// A tenant with no entry has no budget.
    balances: Mutex<HashMap<TenantId, f64>>,
}

impl InMemoryBudgetStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the tenant's remaining balance, giving a new tenant a budget.
    ///
    /// A NaN, infinite or negative amount is stored as `0.0` with a warning: a NaN balance would
    /// make every `estimate > remaining` false and uncap the tenant.
    pub async fn set(&self, tenant: &TenantId, amount_usd: f64) {
        let safe = if amount_usd.is_finite() && amount_usd >= 0.0 {
            amount_usd
        } else {
            tracing::warn!(
                event = "tenant_budget.invalid_balance",
                tenant_id = %tenant,
                requested_usd = amount_usd,
                "rejected non-finite/negative tenant balance; clamping to 0.0 (fail-closed)",
            );
            0.0
        };
        self.balances.lock().await.insert(tenant.clone(), safe);
    }

    /// Removes the tenant's budget, making it unlimited.
    pub async fn clear(&self, tenant: &TenantId) {
        self.balances.lock().await.remove(tenant);
    }
}

#[async_trait]
impl BudgetStore for InMemoryBudgetStore {
    async fn remaining(&self, tenant: &TenantId) -> Result<Option<f64>, BudgetStoreError> {
        Ok(self.balances.lock().await.get(tenant).copied())
    }

    async fn debit(
        &self,
        tenant: &TenantId,
        amount_usd: f64,
    ) -> Result<Option<f64>, BudgetStoreError> {
        // A NaN debit would make the balance NaN and uncap the tenant; a negative one would credit
        // it. Both are skipped.
        if !amount_usd.is_finite() || amount_usd < 0.0 {
            tracing::error!(
                event = "tenant_budget.invalid_debit",
                tenant_id = %tenant,
                amount_usd,
                "rejected non-finite/negative debit amount; skipping debit to protect balance",
            );
            return Ok(self.balances.lock().await.get(tenant).copied());
        }
        let mut guard = self.balances.lock().await;
        if let Some(balance) = guard.get_mut(tenant) {
            *balance -= amount_usd;
            Ok(Some(*balance))
        } else {
            Ok(None)
        }
    }
}

// ─── Middleware ────────────────────────────────────────────────────────

/// The decision on one call against a tenant's balance.
#[derive(Debug, PartialEq, Eq)]
enum BalanceCheck {
    Within,
    /// The estimate exceeds the balance. A negative balance, which concurrent calls under the soft
    /// cap leave behind, is exhausted for every estimate.
    Exhausted,
    /// The store returned NaN, which no estimate can be compared with.
    Corrupt,
}

/// Decides a call against the tenant's balance. `estimate` is never NaN or negative: it is
/// computed from rates the constructor checked.
fn check_balance(estimate: f64, remaining: f64) -> BalanceCheck {
    if remaining.is_nan() {
        BalanceCheck::Corrupt
    } else if estimate > remaining {
        BalanceCheck::Exhausted
    } else {
        BalanceCheck::Within
    }
}

/// Refuses a call whose estimate exceeds the tenant's remaining budget, and debits the real cost
/// when the stream ends.
pub struct TenantBudgetMiddleware {
    store: Arc<dyn BudgetStore>,
    pricing: Pricing,
    default_max_output_tokens: u32,
    /// Set by the first call on a zero-pricing provider, so only that call warns.
    zero_pricing_warned: AtomicBool,
}

impl TenantBudgetMiddleware {
    /// Builds the middleware from a provider's pricing, assuming the default [`OutputReserve`] for a
    /// request that reaches it with no `max_output_tokens`.
    pub fn new(store: Arc<dyn BudgetStore>, capabilities: &ProviderProfile) -> Self {
        Self::from_parts(
            store,
            capabilities.pricing,
            OutputReserve::default().output_tokens.get(),
        )
    }

    /// Builds the middleware from explicit pricing and the output tokens to assume when a request
    /// sets none.
    pub fn from_parts(
        store: Arc<dyn BudgetStore>,
        pricing: Pricing,
        default_max_output_tokens: u32,
    ) -> Self {
        Self {
            store,
            pricing,
            default_max_output_tokens,
            zero_pricing_warned: AtomicBool::new(false),
        }
    }

    fn is_zero_pricing(&self) -> bool {
        self.pricing.is_zero()
    }

    fn estimate_cost_usd(&self, req: &ChatRequest) -> f64 {
        self.pricing
            .estimate_chat_cost(req, self.default_max_output_tokens)
    }
}

#[async_trait]
impl Middleware for TenantBudgetMiddleware {
    fn name(&self) -> &'static str {
        "tenant_budget"
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
                    event = "tenant_budget.zero_pricing",
                    tars.trace_id = %ctx.trace_id,
                    "tenant budget cap is a no-op on this provider \
                     (Pricing has zero input+output rates — typical for \
                     subscription-billed CLI backends)",
                );
            }
            return next.run(req, ctx).await;
        }

        let estimate = self.estimate_cost_usd(&req);
        match self.store.remaining(&ctx.tenant_id).await {
            Ok(Some(remaining)) => match check_balance(estimate, remaining) {
                BalanceCheck::Within => tracing::debug!(
                    event = "tenant_budget.checked",
                    tenant_id = %ctx.tenant_id,
                    estimate_usd = estimate,
                    remaining_usd = remaining,
                ),
                BalanceCheck::Exhausted => {
                    tracing::warn!(
                        event = "tenant_budget.exceeded",
                        tenant_id = %ctx.tenant_id,
                        estimate_usd = estimate,
                        remaining_usd = remaining,
                        tars.trace_id = %ctx.trace_id,
                    );
                    return Err(LlmError::BudgetExceeded.into());
                }
                BalanceCheck::Corrupt => {
                    tracing::error!(
                        event = "tenant_budget.invalid_remaining",
                        tenant_id = %ctx.tenant_id,
                        tars.trace_id = %ctx.trace_id,
                    );
                    return Err(LlmError::BudgetBalanceCorrupt {
                        tenant: ctx.tenant_id.clone(),
                    }
                    .into());
                }
            },
            Ok(None) => {
                tracing::debug!(
                    event = "tenant_budget.unconfigured",
                    tenant_id = %ctx.tenant_id,
                );
                return next.run(req, ctx).await;
            }
            Err(e) => {
                // Refused, so a store outage cannot uncap a tenant.
                tracing::error!(
                    event = "tenant_budget.store_error",
                    tenant_id = %ctx.tenant_id,
                    error = %display_chain(&e),
                );
                return Err(LlmError::BudgetStoreFailed(e).into());
            }
        }

        // The debit is taken before the call's last event is passed on, so a consumer that stops
        // reading there is still charged; a call that fails to open, or errors before reporting
        // any usage, spent nothing that can be priced and is not debited.
        let stream = next.run(req, ctx.clone()).await?;
        let store = self.store.clone();
        let pricing = self.pricing;
        let tenant = ctx.tenant_id.clone();
        let trace_id = ctx.trace_id.clone();

        let observed = async_stream::stream! {
            let mut s = stream;
            let mut spent = None;
            while let Some(ev) = s.next().await {
                let ended = match &ev {
                    Ok(event) => {
                        spent = event.spent_after(spent);
                        event.is_terminal()
                    }
                    Err(_) => true,
                };
                if ended && let Some(usage) = spent.take() {
                    debit(store.as_ref(), &tenant, &trace_id, pricing.cost_for(&usage).0).await;
                }
                yield ev;
            }
        };

        Ok(Box::pin(observed))
    }
}

/// Debits a call's cost after its response has been delivered, so a failure here does not fail
/// the call; the log is for the operator.
async fn debit(
    store: &dyn BudgetStore,
    tenant: &TenantId,
    trace_id: &tars_types::TraceId,
    cost_usd: f64,
) {
    match store.debit(tenant, cost_usd).await {
        Ok(Some(remaining)) => tracing::debug!(
            event = "tenant_budget.debited",
            tenant_id = %tenant,
            debit_usd = cost_usd,
            remaining_usd = remaining,
            tars.trace_id = %trace_id,
        ),
        // The tenant's budget was cleared between the check and the debit.
        Ok(None) => tracing::warn!(
            event = "tenant_budget.debit_lost",
            tenant_id = %tenant,
            debit_usd = cost_usd,
            "tenant became unconfigured between pre-check and debit",
        ),
        Err(e) => tracing::error!(
            event = "tenant_budget.debit_failed",
            tenant_id = %tenant,
            debit_usd = cost_usd,
            error = %display_chain(&e),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmService;
    use crate::ProviderError;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicU32;

    use crate::LlmProvider;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ChatEvent, ProviderId, ProviderProfile, StopReason, Usage};

    fn priced(input: f64, output: f64) -> Pricing {
        Pricing {
            input_per_million: tars_types::Rate::new(input).unwrap(),
            output_per_million: tars_types::Rate::new(output).unwrap(),
            ..Pricing::default()
        }
    }

    fn ok_service(usage: Usage) -> (Arc<dyn LlmProvider>, Arc<AtomicU32>) {
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
        // Use Sequence (not Text) so we control the exact Usage emitted —
        // post-debit math otherwise depends on chars/4 which is fragile.
        let canned = CannedResponse::Sequence(vec![
            ChatEvent::started("m"),
            ChatEvent::Delta { text: "ok".into() },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(usage),
            },
        ]);
        let mock = MockProvider::new(ProviderId::new("p"), canned);
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

    fn req(text: &str, max_out: Option<u32>) -> ChatRequest {
        let mut r = ChatRequest::user(text);
        r.max_output_tokens = max_out;
        r
    }

    fn ctx_for(tenant: &str) -> Caller {
        let mut c = Caller::test_default();
        c.tenant_id = tenant.parse::<TenantId>().unwrap();
        c
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
    async fn a_stream_ending_in_an_error_is_debited_the_usage_it_reported() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let tenant = TenantId::new("acme");
        store.set(&tenant, 1.00).await;
        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        // No `Finished`, so the pipeline ends the stream in `Unfinished`.
        let mock = MockProvider::new(
            ProviderId::new("p"),
            CannedResponse::Sequence(vec![
                ChatEvent::started("m"),
                ChatEvent::Delta { text: "ok".into() },
                ChatEvent::UsageProgress {
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 150,
                        ..Default::default()
                    },
                },
                ChatEvent::UsageProgress {
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 200,
                        ..Default::default()
                    },
                },
            ]),
        );
        let svc = LlmService::builder(mock, "test-model").layer(mw).build();

        let mut stream = svc
            .call(req("hi", Some(1000)), ctx_for("acme"))
            .await
            .result
            .unwrap();
        let mut ended_in = None;
        while let Some(ev) = stream.next().await {
            if let Err(e) = ev {
                ended_in = Some(e);
                break;
            }
        }

        assert!(
            matches!(
                ended_in,
                Some(ProviderError::Unfinished(
                    tars_types::Unfinished::NoStopReason
                ))
            ),
            "{ended_in:?}"
        );
        // 100 × $3/M + 200 × $15/M = $0.0033.
        let remaining = store.remaining(&tenant).await.unwrap().unwrap();
        assert!(
            (remaining - (1.00 - 0.0033)).abs() < 1e-9,
            "got {remaining}"
        );
    }

    #[tokio::test]
    async fn unconfigured_tenant_is_unlimited() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        // Use a usage that would cost > $0 to confirm we're not debiting.
        let (inner, observed) = ok_service(Usage {
            input_tokens: 10_000,
            output_tokens: 1_000,
            ..Default::default()
        });
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        // Massive request that would blow any reasonable budget.
        let stream = svc
            .call(
                req(&"x".repeat(10_000_000), Some(100_000)),
                ctx_for("ghost"),
            )
            .await
            .result
            .unwrap();
        drain(stream).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        // Ghost tenant still unconfigured.
        assert_eq!(
            store.remaining(&TenantId::new("ghost")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn pre_check_rejects_when_estimate_exceeds_remaining() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let tenant = TenantId::new("acme");
        store.set(&tenant, 0.01).await; // $0.01 cap

        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        let (inner, observed) = ok_service(Usage::default());
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        // 1M chars input × $3/M ≈ $0.75 — way over $0.01 cap.
        let err = svc
            .call(req(&"x".repeat(1_000_000), Some(1000)), ctx_for("acme"))
            .await
            .result
            .err()
            .expect("must reject");
        assert!(matches!(err, LlmError::BudgetExceeded));
        assert_eq!(observed.load(Ordering::SeqCst), 0);
        // Balance untouched — no pre-debit semantics.
        assert_eq!(store.remaining(&tenant).await.unwrap(), Some(0.01));
    }

    #[tokio::test]
    async fn post_call_debits_real_usage() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let tenant = TenantId::new("acme");
        store.set(&tenant, 1.00).await;

        // pricing: $3/M input, $15/M output
        // usage:   100 input, 200 output
        // cost:    100×3/1M + 200×15/1M = 0.0003 + 0.003 = 0.0033
        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        let (inner, observed) = ok_service(Usage {
            input_tokens: 100,
            output_tokens: 200,
            ..Default::default()
        });
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        let stream = svc
            .call(req("hi", Some(1000)), ctx_for("acme"))
            .await
            .result
            .unwrap();
        drain(stream).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        let remaining = store.remaining(&tenant).await.unwrap().unwrap();
        assert!(
            (remaining - (1.00 - 0.0033)).abs() < 1e-9,
            "got {remaining}"
        );
    }

    #[tokio::test]
    async fn inner_error_path_does_not_debit() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let tenant = TenantId::new("acme");
        store.set(&tenant, 1.00).await;

        // Inner service that returns immediate error.
        struct ImmediateError;
        #[async_trait]
        impl LlmProvider for ImmediateError {
            fn id(&self) -> &ProviderId {
                static ID: std::sync::OnceLock<ProviderId> = std::sync::OnceLock::new();
                ID.get_or_init(|| ProviderId::new("immediate-error"))
            }
            fn capabilities(&self) -> &ProviderProfile {
                static CAPS: std::sync::OnceLock<ProviderProfile> = std::sync::OnceLock::new();
                CAPS.get_or_init(|| ProviderProfile::text_only_baseline(Pricing::default()))
            }
            async fn stream(
                self: Arc<Self>,
                _req: ChatRequest,
                _model: &str,
                _ctx: Caller,
            ) -> Result<LlmEventStream, ProviderError> {
                Err(ProviderError::ModelOverloaded)
            }
        }

        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        let svc = LlmService::builder(
            Arc::new(ImmediateError) as Arc<dyn LlmProvider>,
            "test-model",
        )
        .layer(mw)
        .build();

        let err = svc
            .call(req("hi", Some(100)), ctx_for("acme"))
            .await
            .result
            .err()
            .expect("inner failed");
        assert!(matches!(
            err,
            LlmError::Provider(ProviderError::ModelOverloaded)
        ));
        // Balance untouched — no debit on open failure.
        assert_eq!(store.remaining(&tenant).await.unwrap(), Some(1.00));
    }

    #[tokio::test]
    async fn zero_pricing_passes_through_with_warn_flag() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let tenant = TenantId::new("acme");
        store.set(&tenant, 0.0001).await; // tiny budget that would normally reject

        let mw = TenantBudgetMiddleware::from_parts(store.clone(), Pricing::default(), 1000);
        let (inner, observed) = ok_service(Usage {
            input_tokens: 999_999,
            output_tokens: 999_999,
            ..Default::default()
        });
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        // Should pass despite the huge usage — zero pricing means no check.
        let stream = svc
            .call(req(&"x".repeat(1_000_000), Some(100_000)), ctx_for("acme"))
            .await
            .result
            .unwrap();
        drain(stream).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        // No debit either — balance untouched.
        assert_eq!(store.remaining(&tenant).await.unwrap(), Some(0.0001));
    }

    #[tokio::test]
    async fn fails_closed_on_store_error() {
        struct BrokenStore;
        #[async_trait]
        impl BudgetStore for BrokenStore {
            async fn remaining(&self, _tenant: &TenantId) -> Result<Option<f64>, BudgetStoreError> {
                Err(BudgetStoreError::Backend("redis down".into()))
            }
            async fn debit(
                &self,
                _tenant: &TenantId,
                _amount_usd: f64,
            ) -> Result<Option<f64>, BudgetStoreError> {
                Err(BudgetStoreError::Backend("redis down".into()))
            }
        }
        let mw = TenantBudgetMiddleware::from_parts(
            Arc::new(BrokenStore) as Arc<dyn BudgetStore>,
            priced(3.0, 15.0),
            1000,
        );
        let (inner, observed) = ok_service(Usage::default());
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        let err = svc
            .call(req("hi", Some(100)), ctx_for("acme"))
            .await
            .result
            .err()
            .expect("must fail-closed on store error");
        assert!(matches!(err, LlmError::BudgetStoreFailed(_)));
        // Inner never invoked — fail-closed semantics.
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn in_memory_store_concurrent_debits() {
        // 100 concurrent debits of $0.01 against a $5.00 balance must
        // produce exactly $4.00 remaining (no lost updates).
        let store = Arc::new(InMemoryBudgetStore::new());
        let tenant = TenantId::new("acme");
        store.set(&tenant, 5.00).await;

        let mut handles = Vec::new();
        for _ in 0..100 {
            let store = store.clone();
            let tenant = tenant.clone();
            handles.push(tokio::spawn(async move {
                store.debit(&tenant, 0.01).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let remaining = store.remaining(&tenant).await.unwrap().unwrap();
        assert!((remaining - 4.00).abs() < 1e-9, "got {remaining}");
    }

    #[tokio::test]
    async fn in_memory_store_unconfigured_tenant_debit_returns_none() {
        let store = InMemoryBudgetStore::new();
        let result = store.debit(&TenantId::new("ghost"), 1.0).await.unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn a_balance_below_the_estimate_is_exhausted_and_one_at_it_is_within() {
        assert_eq!(check_balance(0.5, 0.5), BalanceCheck::Within);
        assert_eq!(check_balance(0.5, 0.4999), BalanceCheck::Exhausted);
    }

    #[test]
    fn a_negative_balance_is_exhausted_and_only_nan_is_corrupt() {
        assert_eq!(check_balance(0.0, -0.01), BalanceCheck::Exhausted);
        assert_eq!(check_balance(0.0, f64::NAN), BalanceCheck::Corrupt);
        assert_eq!(check_balance(0.0, f64::INFINITY), BalanceCheck::Within);
    }

    #[tokio::test]
    async fn a_tenant_driven_below_zero_is_refused_as_over_budget() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let tenant = TenantId::new("acme");
        store.set(&tenant, 1.00).await;
        store.debit(&tenant, 1.50).await.unwrap();

        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        let (inner, observed) = ok_service(Usage::default());
        let svc = LlmService::builder(inner, "test-model").layer(mw).build();

        let err = svc
            .call(req("hi", Some(1)), ctx_for("acme"))
            .await
            .result
            .err()
            .expect("an overdrawn tenant must be refused");
        assert!(matches!(err, LlmError::BudgetExceeded));
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }
}
