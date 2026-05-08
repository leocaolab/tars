//! Shared core types for TARS Runtime.
//!
//! This crate is the single source of truth for the data types that flow
//! between Provider / Pipeline / Runtime / Frontend layers. It deliberately
//! has no business logic — only types, conversions, and pure helpers.
//!
//! Module map:
//! - [`ids`]            — strongly typed IDs (TenantId, SessionId, …)
//! - [`principal`]      — caller identity (Principal, Scope)
//! - [`model`]          — ModelHint / ModelTier / ThinkingMode
//! - [`chat`]           — ChatRequest / Message / ContentBlock
//! - [`tools`]          — ToolSpec / ToolCall as seen by Provider layer
//! - [`schema`]         — JsonSchema wrapper
//! - [`cache`]          — CacheDirective / ProviderCacheHandle / CacheHitInfo
//! - [`events`]         — ChatEvent / StopReason for streaming responses
//! - [`response`]       — ChatResponse + builder for non-streaming consumers
//! - [`usage`]          — Usage / CostUsd / Pricing
//! - [`capabilities`]   — Capabilities / StructuredOutputMode / PromptCacheKind
//! - [`error`]          — ProviderError + ErrorClass
//! - [`context`]        — RequestContext for cross-layer plumbing
//! - [`secret`]         — SecretRef + SecretString (redacting wrapper)
//! - [`auth`]           — Auth specification (None / Delegate / Secret)
//!
//! See `docs/01-llm-provider.md` for the full design rationale.

pub mod auth;
pub mod cache;
pub mod capabilities;
pub mod chat;
pub mod context;
pub mod error;
pub mod events;
pub mod http_extras;
pub mod ids;
pub mod model;
pub mod principal;
pub mod response;
pub mod schema;
pub mod secret;
pub mod telemetry;
pub mod tools;
pub mod usage;
pub mod validation;

pub use auth::Auth;
pub use cache::{systemtime_millis, CacheDirective, CacheHitInfo, ProviderCacheHandle};
pub use capabilities::{
    Capabilities, Modality, PromptCacheKind, StructuredOutputMode,
};
pub use chat::{
    CapabilityRequirements, ChatRequest, CompatibilityCheck, CompatibilityReason, ContentBlock,
    ImageData, Message,
};
pub use context::{CancellationToken, RequestContext};
pub use error::{ErrorClass, ProviderError};
pub use events::{ChatChunk, ChatEvent, PartialUsage, StopReason};
pub use http_extras::HttpProviderExtras;
pub use ids::{
    AgentId, L3HandleId, PrincipalId, ProviderId, SessionId, TaskId, TenantId,
    TraceId, TrajectoryId,
};
pub use model::{ModelHint, ModelTier, ThinkingMode};
pub use principal::{Principal, PrincipalKind, Scope};
pub use response::{ChatResponse, ChatResponseBuilder};
pub use schema::JsonSchema;
pub use secret::{SecretRef, SecretString};
pub use telemetry::{new_shared_telemetry, RetryAttempt, SharedTelemetry, TelemetryAccumulator};
pub use tools::{ToolCall, ToolChoice, ToolSpec};
pub use validation::{
    new_shared_validation_outcome, OutcomeSummary, SharedValidationOutcome, ValidationOutcome,
    ValidationOutcomeRecord, ValidationSummary,
};
pub use usage::{CostUsd, Pricing, Usage};
