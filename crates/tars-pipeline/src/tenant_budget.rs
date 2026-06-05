//! Tenant budget middleware — stateful per-tenant USD cap.
//!
//! The stateless [`crate::PerCallBudgetMiddleware`] caps each call
//! independently. This one caps an aggregate per-tenant running total:
//! pre-call checks `store.remaining(tenant) >= estimate`, post-call
//! debits the **real** USD cost from the stream's terminal `Finished`
//! event.
//!
//! See [`docs/roadmap.md §4`](../../../../docs/roadmap.md) for the
//! design and the soft-cap tradeoff (no pre-debit → small race where
//! concurrent calls can both pass pre-check). That tradeoff is
//! deliberate: tars is an agent runtime, not a financial ledger.
//! Hard accounting belongs in caller-owned `BudgetStore` impls that
//! choose their own consistency model (Redis WATCH, Postgres row
//! locks, etc.).
//!
//! # Composition
//!
//! ```ignore
//! let store = Arc::new(InMemoryBudgetStore::new());
//! store.set(&TenantId::new("acme"), 10.00).await?;   // $10 cap for acme
//!
//! let pipeline = Pipeline::builder(provider)
//!     .layer(TelemetryMiddleware::new())
//!     .layer(CacheLookupMiddleware::new(cache))
//!     .layer(PerCallBudgetMiddleware::new(0.05, &caps))   // per-call hard cap
//!     .layer(TenantBudgetMiddleware::new(store, &caps))   // aggregate per-tenant
//!     .layer(RetryMiddleware::default())
//!     .build();
//! ```
//!
//! The two budget layers are independent: PerCall handles the
//! "no single call may cost more than X" constraint; TenantBudget
//! handles the "tenant Y has $Z left this month" constraint.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use thiserror::Error;
use tokio::sync::Mutex;

use tars_provider::LlmEventStream;
use tars_types::{
    Capabilities, ChatEvent, ChatRequest, Pricing, ProviderError, RequestContext, TenantId,
};

use crate::middleware::Middleware;
use crate::service::LlmService;

// ─── BudgetStore trait + reference impl ────────────────────────────────

/// Where per-tenant USD budgets live. Caller implements this against
/// whatever consistency story they want (Postgres, Redis, in-memory).
///
/// `None` from `remaining()` and `debit()` means **tenant has no
/// configured budget** — the middleware interprets this as unlimited
/// and skips the cap. This makes "drop in a budget middleware without
/// breaking existing tenants" the default path.
#[async_trait]
pub trait BudgetStore: Send + Sync + 'static {
    /// Remaining USD budget for the tenant. `None` = unconfigured /
    /// unlimited; `Some(0.0)` = configured but exhausted.
    async fn remaining(&self, tenant: &TenantId) -> Result<Option<f64>, BudgetStoreError>;

    /// Atomically debit `amount_usd` from the tenant's balance.
    /// Returns the new remaining balance (or `None` if unconfigured).
    ///
    /// Implementations should be tolerant of `amount_usd` driving the
    /// balance below zero — pre-check race conditions are documented
    /// soft-cap behavior, not a backend integrity violation.
    async fn debit(
        &self,
        tenant: &TenantId,
        amount_usd: f64,
    ) -> Result<Option<f64>, BudgetStoreError>;
}

#[derive(Debug, Error)]
pub enum BudgetStoreError {
    #[error("budget store backend error: {0}")]
    Backend(String),
}

/// In-memory `BudgetStore` — for tests, single-process deployments,
/// and as the canonical reference impl new backends can compare
/// against.
///
/// **Not** for multi-process deployments: process A and process B
/// each have their own `InMemoryBudgetStore` and won't see each
/// other's debits. Plug a Redis/Postgres-backed store in for those.
#[derive(Default)]
pub struct InMemoryBudgetStore {
    /// `None` value = unconfigured (sentinel kept for explicit removal).
    /// Absent key also = unconfigured. We store explicit values only.
    balances: Mutex<HashMap<TenantId, f64>>,
}

impl InMemoryBudgetStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the tenant's remaining balance to `amount_usd`. Calling with
    /// a brand-new tenant configures it for the first time.
    ///
    /// A non-finite (`NaN`/`±inf`) or negative balance is clamped to a
    /// fail-closed `0.0`: a `NaN` balance would make every `estimate >
    /// remaining` pre-check `false` and silently uncap the tenant, so we
    /// refuse to store it.
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

    /// Forget a tenant's balance (reverts to unconfigured / unlimited).
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
        // Defend the in-memory balance against a non-finite/negative
        // debit. `set()` already validates stored balances; without the
        // same guard here a NaN/inf `amount_usd` would poison the balance
        // (`x -= NaN` → NaN, after which every `estimate > remaining`
        // pre-check goes false and silently uncaps the tenant) and a
        // negative amount would *credit* the tenant. Driving the balance
        // legitimately below zero (soft-cap race) is still allowed.
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
            // Unconfigured tenant — no debit, no error. Keeps the
            // "drop in a budget MW without breaking existing tenants"
            // invariant.
            Ok(None)
        }
    }
}

// ─── Middleware ────────────────────────────────────────────────────────

/// Tenant-aggregate USD budget cap. Pre-call checks `remaining >=
/// upper-bound estimate`; post-call debits the real cost from the
/// stream's `Finished` event.
#[derive(Clone)]
pub struct TenantBudgetMiddleware {
    store: Arc<dyn BudgetStore>,
    pricing: Pricing,
    default_max_output_tokens: u32,
}

impl TenantBudgetMiddleware {
    /// Construct with provider capability snapshot — canonical caller path.
    pub fn new(store: Arc<dyn BudgetStore>, capabilities: &Capabilities) -> Self {
        Self {
            store,
            pricing: capabilities.pricing,
            default_max_output_tokens: capabilities.max_output_tokens,
        }
    }

    /// Construct from explicit pricing — for tests / hand-rolled flows.
    pub fn from_parts(
        store: Arc<dyn BudgetStore>,
        pricing: Pricing,
        default_max_output_tokens: u32,
    ) -> Self {
        Self {
            store,
            pricing,
            default_max_output_tokens,
        }
    }
}

impl Middleware for TenantBudgetMiddleware {
    fn name(&self) -> &'static str {
        "tenant_budget"
    }

    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(TenantBudgetService {
            inner,
            store: self.store.clone(),
            pricing: self.pricing,
            default_max_output_tokens: self.default_max_output_tokens,
            zero_pricing_warned: AtomicBool::new(false),
        })
    }
}

struct TenantBudgetService {
    inner: Arc<dyn LlmService>,
    store: Arc<dyn BudgetStore>,
    pricing: Pricing,
    default_max_output_tokens: u32,
    zero_pricing_warned: AtomicBool,
}

impl TenantBudgetService {
    fn is_zero_pricing(&self) -> bool {
        self.pricing.is_zero()
    }

    /// Best-effort pre-call USD estimate. Delegated to
    /// [`Pricing::estimate_chat_cost`] — same formula as the per-call
    /// middleware. The earlier "kept duplicated for likely
    /// divergence" comment didn't pan out; if V2 wants a cache-discount
    /// estimate, it'll land on `Pricing` itself so both budget
    /// middlewares pick it up.
    fn estimate_cost_usd(&self, req: &ChatRequest) -> f64 {
        self.pricing
            .estimate_chat_cost(req, self.default_max_output_tokens)
    }
}

#[async_trait]
impl LlmService for TenantBudgetService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        match ctx.telemetry.lock() {
            Ok(mut t) => t.layers.push("tenant_budget".into()),
            // Telemetry is best-effort, but a poisoned lock means a prior
            // task panicked while holding it — surface it for debugging
            // rather than dropping the trace entry silently.
            Err(e) => tracing::debug!(
                error = %e,
                "tenant_budget: telemetry mutex poisoned recording layer trace",
            ),
        }

        if self.is_zero_pricing() {
            if !self.zero_pricing_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    event = "tenant_budget.zero_pricing",
                    trace_id = %ctx.trace_id,
                    "tenant budget cap is a no-op on this provider \
                     (Pricing has zero input+output rates — typical for \
                     subscription-billed CLI backends)",
                );
            }
            return self.inner.clone().call(req, ctx).await;
        }

        // Pre-check.
        let estimate = self.estimate_cost_usd(&req);
        // A non-finite/negative estimate (e.g. a bad Pricing rate that
        // slipped past construction, or overflow) would make
        // `estimate > remaining` evaluate to `false` for NaN and
        // silently uncap the tenant. Fail-closed instead.
        if !estimate.is_finite() || estimate < 0.0 {
            tracing::error!(
                event = "tenant_budget.invalid_estimate",
                tenant_id = %ctx.tenant_id,
                estimate_usd = estimate,
                trace_id = %ctx.trace_id,
            );
            return Err(ProviderError::Internal(
                "tenant budget produced an invalid (non-finite/negative) cost estimate".into(),
            ));
        }
        match self.store.remaining(&ctx.tenant_id).await {
            Ok(Some(remaining)) => {
                // The trait can't statically forbid a buggy/malicious
                // store from returning NaN/inf/negative. Such a value
                // would make `estimate > remaining` always `false` and
                // silently uncap the tenant, so fail-closed instead.
                if !remaining.is_finite() || remaining < 0.0 {
                    tracing::error!(
                        event = "tenant_budget.invalid_remaining",
                        tenant_id = %ctx.tenant_id,
                        remaining_usd = remaining,
                        trace_id = %ctx.trace_id,
                    );
                    return Err(ProviderError::Internal(
                        "tenant budget store returned an invalid (non-finite/negative) balance"
                            .into(),
                    ));
                }
                if estimate > remaining {
                    tracing::warn!(
                        event = "tenant_budget.exceeded",
                        tenant_id = %ctx.tenant_id,
                        estimate_usd = estimate,
                        remaining_usd = remaining,
                        trace_id = %ctx.trace_id,
                    );
                    return Err(ProviderError::BudgetExceeded);
                }
                tracing::debug!(
                    event = "tenant_budget.checked",
                    tenant_id = %ctx.tenant_id,
                    estimate_usd = estimate,
                    remaining_usd = remaining,
                );
            }
            Ok(None) => {
                // Unconfigured tenant — treat as unlimited. Don't even
                // bother debiting on post-call (it'd be a no-op).
                tracing::debug!(
                    event = "tenant_budget.unconfigured",
                    tenant_id = %ctx.tenant_id,
                );
                return self.inner.clone().call(req, ctx).await;
            }
            Err(e) => {
                // Store backend down. Fail-closed (refuse the call) so
                // a Redis outage can't silently uncap a tenant.
                tracing::error!(
                    event = "tenant_budget.store_error",
                    tenant_id = %ctx.tenant_id,
                    error = %e,
                );
                return Err(ProviderError::Internal(format!(
                    "tenant budget store error: {e}"
                )));
            }
        }

        // Run inner. Wrap the returned stream so we can debit on the
        // terminal `Finished` event (real usage). Mid-stream errors and
        // immediate-open errors don't debit — no real usage was reported.
        let stream = self.inner.clone().call(req, ctx.clone()).await?;
        let store = self.store.clone();
        let pricing = self.pricing;
        let tenant = ctx.tenant_id.clone();
        let trace_id = ctx.trace_id.clone();

        let observed = async_stream::stream! {
            let mut s = stream;
            while let Some(ev) = s.next().await {
                if let Ok(ChatEvent::Finished { usage, .. }) = &ev {
                    let real_cost = pricing.cost_for(usage);
                    // Guard against a bad `Pricing`/`Usage` yielding a
                    // NaN/inf/negative cost: debiting it would poison the
                    // balance (`x -= NaN` → NaN; `x -= -y` → balance grows).
                    // Skip the debit and log loudly instead of corrupting state.
                    if !real_cost.0.is_finite() || real_cost.0 < 0.0 {
                        tracing::error!(
                            event = "tenant_budget.invalid_cost",
                            tenant_id = %tenant,
                            cost_usd = real_cost.0,
                            trace_id = %trace_id,
                            "computed cost is non-finite/negative; skipping debit",
                        );
                        yield ev;
                        continue;
                    }
                    match store.debit(&tenant, real_cost.0).await {
                        Ok(Some(remaining)) => {
                            tracing::debug!(
                                event = "tenant_budget.debited",
                                tenant_id = %tenant,
                                debit_usd = real_cost.0,
                                remaining_usd = remaining,
                                trace_id = %trace_id,
                            );
                        }
                        Ok(None) => {
                            // Pre-check found a balance; debit found none.
                            // Means the tenant was just reset/cleared
                            // between calls — log and move on.
                            tracing::warn!(
                                event = "tenant_budget.debit_lost",
                                tenant_id = %tenant,
                                debit_usd = real_cost.0,
                                "tenant became unconfigured between pre-check and debit",
                            );
                        }
                        Err(e) => {
                            // Backend down post-call — log loudly but
                            // don't fail the call (the user already
                            // got their response). Operator action.
                            tracing::error!(
                                event = "tenant_budget.debit_failed",
                                tenant_id = %tenant,
                                debit_usd = real_cost.0,
                                error = %e,
                            );
                        }
                    }
                }
                yield ev;
            }
        };

        Ok(Box::pin(observed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    use tars_provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::{ModelHint, StopReason, Usage};

    fn priced(input: f64, output: f64) -> Pricing {
        Pricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: 0.0,
            cache_creation_per_million: 0.0,
            thinking_per_million: 0.0,
        }
    }

    fn ok_service(usage: Usage) -> (Arc<dyn LlmService>, Arc<AtomicU32>) {
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
        // Use Sequence (not Text) so we control the exact Usage emitted —
        // post-debit math otherwise depends on chars/4 which is fragile.
        let canned = CannedResponse::Sequence(vec![
            ChatEvent::started("m"),
            ChatEvent::Delta { text: "ok".into() },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage,
            },
        ]);
        let mock = MockProvider::new("p", canned);
        let inner: Arc<dyn LlmService> = crate::ProviderService::new(mock);
        (
            Arc::new(Count {
                inner,
                observed: observed.clone(),
            }) as Arc<dyn LlmService>,
            observed,
        )
    }

    fn req(text: &str, max_out: Option<u32>) -> ChatRequest {
        let mut r = ChatRequest::user(ModelHint::Explicit("m".into()), text);
        r.max_output_tokens = max_out;
        r
    }

    fn ctx_for(tenant: &str) -> RequestContext {
        let mut c = RequestContext::test_default();
        c.tenant_id = TenantId::new(tenant);
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
    async fn unconfigured_tenant_is_unlimited() {
        let store = Arc::new(InMemoryBudgetStore::new());
        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        // Use a usage that would cost > $0 to confirm we're not debiting.
        let (inner, observed) = ok_service(Usage {
            input_tokens: 10_000,
            output_tokens: 1_000,
            ..Default::default()
        });
        let svc = mw.wrap(inner);

        // Massive request that would blow any reasonable budget.
        let stream = svc
            .call(
                req(&"x".repeat(10_000_000), Some(100_000)),
                ctx_for("ghost"),
            )
            .await
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
        let svc = mw.wrap(inner);

        // 1M chars input × $3/M ≈ $0.75 — way over $0.01 cap.
        let err = svc
            .call(req(&"x".repeat(1_000_000), Some(1000)), ctx_for("acme"))
            .await
            .err()
            .expect("must reject");
        assert!(matches!(err, ProviderError::BudgetExceeded));
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
        let svc = mw.wrap(inner);

        let stream = svc
            .call(req("hi", Some(1000)), ctx_for("acme"))
            .await
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
        impl LlmService for ImmediateError {
            async fn call(
                self: Arc<Self>,
                _req: ChatRequest,
                _ctx: RequestContext,
            ) -> Result<LlmEventStream, ProviderError> {
                Err(ProviderError::ModelOverloaded)
            }
        }

        let mw = TenantBudgetMiddleware::from_parts(store.clone(), priced(3.0, 15.0), 1000);
        let svc = mw.wrap(Arc::new(ImmediateError) as Arc<dyn LlmService>);

        let err = svc
            .call(req("hi", Some(100)), ctx_for("acme"))
            .await
            .err()
            .expect("inner failed");
        assert!(matches!(err, ProviderError::ModelOverloaded));
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
        let svc = mw.wrap(inner);

        // Should pass despite the huge usage — zero pricing means no check.
        let stream = svc
            .call(req(&"x".repeat(1_000_000), Some(100_000)), ctx_for("acme"))
            .await
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
        let svc = mw.wrap(inner);

        let err = svc
            .call(req("hi", Some(100)), ctx_for("acme"))
            .await
            .err()
            .expect("must fail-closed on store error");
        assert!(matches!(err, ProviderError::Internal(_)));
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
}
