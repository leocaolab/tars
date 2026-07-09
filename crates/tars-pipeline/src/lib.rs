//! Middleware pipeline framework — Doc 02.
//!
//! An [`LlmService`] is a concrete, reusable callable: **one provider +
//! one model + an ordered list of [`Middleware`] layers**. It is the one
//! public service concept — there is no service trait, no per-layer
//! wrapper service, no `dyn LlmService`.
//!
//! Calling it drives the layers as a **handler chain**: each layer gets
//! the request and a [`Next`] cursor, does its pre-work, calls
//! `next.run(req, ctx)` — zero times to short-circuit (cache hit, budget
//! reject), once normally, many times to retry — then post-processes.
//! The terminal of the chain is `provider.stream(req, model, ctx)`.
//!
//! A middleware is ONE type with ONE method:
//!
//! ```ignore
//! #[async_trait]
//! impl Middleware for MyGuard {
//!     fn name(&self) -> &'static str { "my_guard" }
//!     async fn handle(&self, req: ChatRequest, ctx: RequestContext, next: Next<'_>)
//!         -> Result<LlmEventStream, ProviderError> {
//!         // pre-work …
//!         let out = next.run(req, ctx).await?;
//!         // post-work …
//!         Ok(out)
//!     }
//! }
//! ```
//!
//! **The model lives on the service**, not on the request and not on the
//! context. Layers that need it — cache key, telemetry label, event
//! record — read `next.model()`; the rest never see it.
//!
//! ## Composition order
//!
//! Call order **is** chain order: the first `.layer(...)` is OUTERMOST
//! (runs first inbound, last outbound); the provider is innermost.
//!
//! Canonical order (Doc 02 §2). Unchanged except that **Routing and
//! Fallback are gone** — provider selection is no longer a pipeline
//! concern:
//!
//! ```text
//! Telemetry (outermost)
//!  └─ Auth / IAM
//!      └─ Budget
//!          └─ Cache Lookup
//!              └─ Prompt Guard
//!                  └─ Retry
//!                      └─ Circuit Breaker   (a provider wrapper, not a layer)
//!                          └─ Provider call (innermost)
//! ```
//!
//! (Doc 02 §2 drew the breaker *above* Retry; the code puts it below —
//! see [`CircuitBreaker`] — so an open breaker rejects each attempt
//! before the provider is hit and Retry reacts to that rejection.)
//!
//! What `Pipeline::default_chain` / `Pipeline::chain_over` actually
//! assemble today (each layer conditional on its `PipelineOpts` field),
//! with the rest filling in as their dependencies come online:
//!
//! ```text
//! EventEmitter → Telemetry → Validation → Cache Lookup → Retry → [CircuitBreaker] provider
//! ```
//!
//! The circuit breaker is **not** a middleware: [`CircuitBreaker`] wraps
//! the [`LlmProvider`](tars_provider::LlmProvider) itself, so it is
//! applied below the chain rather than added with `.layer(...)`. Auth /
//! IAM and Prompt Guard are not implemented yet. The budget layers
//! ([`PerCallBudgetMiddleware`], [`TenantBudgetMiddleware`]) exist and are
//! opt-in via `.layer(...)`, not part of the default chain.
//!
//! **Provider selection is not a pipeline concern.** There is no routing,
//! ensemble or fallback layer. A caller who wants them composes several
//! `LlmService`s: ensemble = build N services, call all, merge; fallback
//! = try one, on error try the next.
//!
//! ## Building a service
//!
//! ```ignore
//! use std::sync::Arc;
//! use tars_pipeline::{Pipeline, RetryMiddleware, TelemetryMiddleware};
//!
//! let provider: Arc<dyn LlmProvider> = /* registry.get(&id).unwrap() */;
//! let svc = Pipeline::builder(provider, "claude-sonnet-5")
//!     .layer(TelemetryMiddleware::new())   // outermost
//!     .layer(RetryMiddleware::default())   // closest to the provider
//!     .build();                            // -> LlmService
//!
//! let stream = svc.call(req, ctx).await?;  // `req` carries no model
//! ```

mod middleware;
mod service;

pub use middleware::budget::{BudgetConfigError, PerCallBudgetMiddleware};
pub use middleware::cache::{CacheLookupMiddleware, set_cache_policy};
pub use middleware::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
pub use middleware::event_emitter::EventEmitterMiddleware;
pub use middleware::retry::{RetryConfig, RetryMiddleware};
pub use middleware::telemetry::TelemetryMiddleware;
pub use middleware::tenant_budget::{
    BudgetStore, BudgetStoreError, InMemoryBudgetStore, TenantBudgetMiddleware,
};
pub use middleware::validation::{
    OutputValidator, ValidationMiddleware,
    builtin::{JsonShapeValidator, MaxLengthValidator, NotEmptyValidator, OnExceed, ResponseField},
};
pub use middleware::{EventStores, Middleware, Pipeline, PipelineBuilder, PipelineOpts};
pub use service::{LlmService, Next};

// Re-export the few tars-types items that show up in middleware
// signatures so callers don't need a separate `use tars_types::…`.
pub use tars_provider::LlmEventStream;
pub use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext};
