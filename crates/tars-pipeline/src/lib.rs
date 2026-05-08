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

mod cache;
mod circuit_breaker;
mod event_emitter;
mod middleware;
mod retry;
mod routing;
mod service;
mod telemetry;
mod validation;

pub use cache::{CacheLookupMiddleware, set_cache_policy};
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
pub use event_emitter::EventEmitterMiddleware;
pub use middleware::{Middleware, Pipeline, PipelineBuilder};
pub use retry::{RetryConfig, RetryMiddleware};
pub use routing::{RoutingPolicy, RoutingService, StaticPolicy, TierPolicy};
pub use service::{LlmService, ProviderService};
pub use telemetry::TelemetryMiddleware;
pub use validation::{
    OutputValidator, ValidationMiddleware,
    builtin::{JsonShapeValidator, MaxLengthValidator, NotEmptyValidator, OnExceed, ResponseField},
};

// Re-export the few tars-types items that show up in middleware
// signatures so callers don't need a separate `use tars_types::…`.
pub use tars_provider::LlmEventStream;
pub use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext};
