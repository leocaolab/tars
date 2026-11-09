//! Runs an LLM call through an ordered chain of [`Middleware`] down to one provider and model.
//!
//! An [`LlmService`] is one provider, one model and an ordered list of layers. Each layer gets the
//! request and a [`Next`] cursor and calls `next.run(req, ctx)` zero times to short-circuit (a
//! budget refusal), once normally, or several times to retry. The end of the chain is
//! `provider.stream(req, model, ctx)`.
//!
//! ```ignore
//! #[async_trait]
//! impl Middleware for MyGuard {
//!     fn name(&self) -> &'static str { "my_guard" }
//!     async fn handle(&self, req: ChatRequest, ctx: Caller, next: Next<'_>)
//!         -> Result<LlmEventStream, OpenFailure> {
//!         // pre-work …
//!         let out = next.run(req, ctx).await?;
//!         // post-work …
//!         Ok(out)
//!     }
//! }
//! ```
//!
//! The model belongs to the service, not to the request or the context; a layer that needs it
//! reads `next.model()`.
//!
//! ## Composition order
//!
//! The first `.layer(...)` is outermost: it runs first on the way in and last on the way out. The
//! provider call is innermost. `LlmService::default_chain` and `LlmService::chain_over` assemble
//! the chain below; Telemetry and Retry are always there, and EventEmitter and Validation each
//! only when its `ChainOptions` field asks:
//!
//! ```text
//! EventEmitter → Telemetry → Validation → Retry → [CircuitBreaker] provider
//! ```
//!
//! [`CircuitBreaker`] wraps the [`LlmProvider`] rather than joining
//! the chain, so it sits below Retry: an open breaker rejects an attempt before the provider is
//! hit, and Retry returns that rejection to the caller without retrying it. The budget layers ([`PerCallBudgetMiddleware`],
//! [`TenantBudgetMiddleware`]) are not in the default chain; a caller adds them with
//! `.layer(...)`.
//!
//! Provider selection is not a pipeline concern: there is no routing, ensemble or fallback
//! layer. A caller who wants one composes several `LlmService`s.
//!
//! ## Building a service
//!
//! ```ignore
//! use std::sync::Arc;
//! use tars_llm::{LlmService, RetryMiddleware, TelemetryMiddleware};
//!
//! let provider: Arc<dyn LlmProvider> = /* registry.get(&id).unwrap() */;
//! let svc = LlmService::builder(provider, "claude-sonnet-5")
//!     .layer(TelemetryMiddleware::new())   // outermost
//!     .layer(RetryMiddleware::default())   // closest to the provider
//!     .build();                            // -> LlmService
//!
//! let call = svc.call(req, ctx).await;      // `req` carries no model
//! let ids = call.call_ids;                  // the `llm_call` rows it was logged under
//! let stream = call.result?;
//! ```
//!
//! [`CircuitBreaker`]: crate::CircuitBreaker
//! [`LlmProvider`]: crate::LlmProvider
//! [`LlmService`]: crate::LlmService
//! [`Middleware`]: crate::Middleware
//! [`Next`]: crate::Next
//! [`PerCallBudgetMiddleware`]: crate::PerCallBudgetMiddleware
//! [`TenantBudgetMiddleware`]: crate::TenantBudgetMiddleware

pub(crate) mod circuit_breaker;
pub(crate) mod complete;
pub(crate) mod error;
pub(crate) mod middleware;
pub(crate) mod service;
