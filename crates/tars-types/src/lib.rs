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
//! - [`capabilities`]   — ProviderProfile / StructuredOutputMode / PromptCacheKind
//! - [`error`]          — ProviderError + ErrorClass
//! - [`context`]        — RequestContext for cross-layer plumbing
//! - [`secret`]         — SecretRef + SecretString (redacting wrapper)
//! - [`auth`]           — Auth specification (None / Delegate / Secret)
//!
//! See `docs/architecture/01-llm-provider.md` for the full design rationale.

pub mod bless;
pub mod env;
pub mod ids;
pub mod principal;
pub mod providers;
pub mod runtime;
pub mod secret;
pub mod telemetry;
pub mod validation;

pub use bless::{Assert, Bless, BlessError, BlessOutcome, Codec, Drift, MatchTier};
pub use ids::{
    AgentId, BatchItemId, BatchJobId, L3HandleId, PrincipalId, ProviderId, SessionId, TaskId,
    TenantId, TraceId, TrajectoryId,
};
pub use principal::{Principal, PrincipalKind, Scope};
pub use providers::auth::Auth;
pub use providers::cache::{CacheDirective, CacheHitInfo, ProviderCacheHandle, systemtime_millis};
pub use providers::chat::{
    CapabilityRequirements, ChatRequest, CompatibilityCheck, CompatibilityReason, ContentBlock,
    ImageData, Message,
};
pub use providers::context::{CancellationToken, RequestContext};
pub use providers::error::{ErrorClass, ProviderError, ProviderErrorKind};
pub use providers::events::{ChatChunk, ChatEvent, PartialUsage, StopReason};
pub use providers::http_extras::HttpProviderExtras;
pub use providers::model::{ModelHint, ModelTier, ThinkingMode};
pub use providers::provider_profile::{
    InterfaceKind, Modality, PromptCacheKind, ProviderProfile, StructuredOutputMode,
};
pub use providers::response::{ChatResponse, ChatResponseBuilder};
pub use providers::schema::JsonSchema;
pub use providers::tools::{ToolCall, ToolChoice, ToolSpec};
pub use providers::usage::{CostUsd, Pricing, Usage};
pub use runtime::batch::{BatchResultItem, BatchStatus};
pub use runtime::run_context::{RUN_CONTEXT, spawn_with_context};
pub use runtime::run_report::{
    AgentBreakdown, ProviderBreakdown, RunErrorSummary, RunReport, RunStatus,
};
pub use secret::{SecretRef, SecretString};
pub use telemetry::{RetryAttempt, SharedTelemetry, TelemetryAccumulator, new_shared_telemetry};
pub use validation::{
    OutcomeSummary, SharedValidationOutcome, ValidationOutcome, ValidationOutcomeRecord,
    ValidationReason, ValidationSummary, new_shared_validation_outcome,
};

// ── Module paths kept reachable ────────────────────────────────────────────
//
// Consumers name almost everything flat (`tars_types::ChatRequest`), but three
// module paths appear in the tree and one of them is now a directory down.
// Re-exporting the module — not just its items — keeps `tars_types::error::…`
// resolving after the move.
pub use providers::error;
pub use providers::provider_profile as capabilities;
