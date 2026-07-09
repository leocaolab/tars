//! Middleware pipeline framework — Doc 02.
//!
//! The pipeline is a stack of [`Middleware`] layers wrapping an inner
//! [`LlmService`]. Each layer is a Tower-style "wrap the inner service,
//! return a new service" — same shape as `tower::Layer`, but with our
//! own trait so we can stay `async_trait`-native and avoid the
//! pinned-future generics tower forces on you.
//!
//! Composition order (Doc 02 §2):
//!
//! ```text
//! Telemetry (outermost)
//!  └─ Auth / IAM
//!      └─ Budget
//!          └─ Cache Lookup
//!              └─ Prompt Guard
//!                  └─ Routing
//!                      └─ Circuit Breaker
//!                          └─ Retry / Fallback
//!                              └─ Provider call (innermost)
//! ```
//!
//! M1 ships only Telemetry + Retry + the [`ProviderService`] adapter at
//! the bottom. The other layers fill in as their dependencies (cache
//! crate, IAM crate, budget store) come online.
//!
//! ## Building a pipeline
//!
//! ```ignore
//! use std::sync::Arc;
//! use tars_pipeline::{Pipeline, RetryMiddleware, TelemetryMiddleware};
//!
//! let provider: Arc<dyn LlmProvider> = /* registry.get(&id).unwrap() */;
//! let pipeline = Pipeline::builder(provider)
//!     .layer(TelemetryMiddleware::new())   // outermost
//!     .layer(RetryMiddleware::default())   // closest to provider
//!     .build();
//! ```
//!
//! Layer order matches the call order: the first `.layer(...)` wraps
//! everything else and runs first on the inbound, last on the outbound.

mod middleware;
mod service;

pub use middleware::budget::{BudgetConfigError, PerCallBudgetMiddleware};
pub use middleware::cache::{CacheLookupMiddleware, set_cache_policy};
pub use middleware::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
pub use middleware::event_emitter::EventEmitterMiddleware;
pub use middleware::fallback::{FallbackBuilder, FallbackMiddleware, FallbackTrigger};
pub use middleware::latency_stats::{LatencyMetric, LatencyStats, LatencyStatsRegistry};
pub use middleware::retry::{RetryConfig, RetryMiddleware};
pub use middleware::routing::{
    CostPolicy, EnsembleService, LatencyPolicy, RoutingPolicy, RoutingService, StaticPolicy,
    TierPolicy,
};
pub use middleware::telemetry::TelemetryMiddleware;
pub use middleware::tenant_budget::{
    BudgetStore, BudgetStoreError, InMemoryBudgetStore, TenantBudgetMiddleware,
};
pub use middleware::validation::{
    OutputValidator, ValidationMiddleware,
    builtin::{JsonShapeValidator, MaxLengthValidator, NotEmptyValidator, OnExceed, ResponseField},
};
pub use middleware::{EventStores, Middleware, Pipeline, PipelineBuilder, PipelineOpts};
pub use service::{LlmService, ProviderService};

// Re-export the few tars-types items that show up in middleware
// signatures so callers don't need a separate `use tars_types::…`.
pub use tars_provider::LlmEventStream;
pub use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext};
