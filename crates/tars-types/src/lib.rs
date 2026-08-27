//! Shared core types for TARS Runtime.
//!
//! This crate is the single source of truth for the data types that flow
//! between Provider / Pipeline / Runtime / Frontend layers. It deliberately
//! has no business logic — only types, conversions, and pure helpers.
//!
//! Module map:
//! - [`ids`]        — strongly typed IDs (TenantId, SessionId, …)
//! - [`principal`]  — caller identity (Principal, Scope)
//! - [`secret`]     — SecretRef + SecretString (redacting wrapper)
//! - [`env`]        — process-environment reads, in one place
//! - [`telemetry`]  — the span/metric vocabulary
//! - [`validation`] — the typed reasons a response is rejected
//! - [`providers`]  — everything the provider layer speaks: chat, tools,
//!   schema, cache, events, response, usage, model, provider_profile, error,
//!   context, auth, http_extras, batch
//! - [`run_context`] — the ambient RequestContext a run carries
//! - [`run_report`]  — the roll-up a finished run produces
//!
//! Items are named flat (`tars_types::ChatRequest`); the modules above are the
//! filing system, not the address.
//!
//! See `docs/architecture/01-llm-provider.md` for the full design rationale.

pub mod env;
pub mod ids;
pub mod principal;
pub mod providers;
pub mod run_context;
pub mod run_report;
pub mod secret;
pub mod telemetry;
pub mod validation;

pub use ids::{
    AgentId, BatchItemId, BatchJobId, L3HandleId, PrincipalId, ProviderId, SessionId,
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
pub use providers::batch::{BatchResultItem, BatchStatus};
pub use run_context::{RUN_CONTEXT, spawn_with_context};
pub use run_report::{
    AgentBreakdown, ProviderBreakdown, RunErrorSummary, RunReport, RunStatus,
};
pub use secret::{SecretRef, SecretString};
pub use telemetry::{RetryAttempt, SharedTelemetry, TelemetryAccumulator, new_shared_telemetry};
pub use validation::{
    OutcomeSummary, SharedValidationOutcome, ValidationOutcome, ValidationOutcomeRecord,
    ValidationReason, ValidationSummary, new_shared_validation_outcome,
};
