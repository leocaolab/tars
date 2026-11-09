//! Defines the types that cross between the provider, pipeline, runtime and frontend crates:
//! what describes an LLM call (`ChatRequest`, the provider profile, ids, secrets, validation),
//! the [`step`] contract agents are built from, and the [`task`] records of the durable store. It
//! makes no provider calls and does no network or file IO.
//!
//! Items are re-exported flat (`tars_types::ChatRequest`, `tars_types::Cap`); the modules are
//! the filing system, not the address. The design rationale is in
//! `docs/architecture/01-llm-provider.md`.

#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod ask;
pub mod env;
pub mod ids;
pub mod principal;
pub mod providers;
pub mod run_context;
pub mod secret;
pub mod step;
pub mod task;
pub mod telemetry;
pub mod validation;

#[doc(hidden)]
// Lets `json_object!` name serde_json from a crate that does not depend on it.
pub mod __private {
    pub use serde_json;
}

pub use ask::{AgentAsk, AgentReply, Approver, Approvers, ApproversError, fits};
pub use ids::{
    AgentId, AgentKey, BatchItemId, BatchJobId, CallId, CallIdError, CapName, DurableStoreId,
    JobId, L3HandleId, ParseAgentIdError, ParseAgentKeyError, ParseBatchItemIdError,
    ParseBatchJobIdError, ParseCapNameError, ParseDurableStoreIdError, ParseJobIdError,
    ParseL3HandleIdError, ParsePrincipalIdError, ParseProviderIdError, ParseTaskIdError,
    ParseTenantIdError, ParseTraceIdError, ParseTrajectoryIdError, PrincipalId, ProviderId,
    SessionId, SessionIdError, TaskId, TenantId, TraceId, TrajectoryId, UtcDay,
};
pub use principal::{Principal, PrincipalKind, Scope};
pub use providers::auth::Auth;
pub use providers::cache::{CacheDirective, CacheHitInfo, ProviderCacheHandle, systemtime_millis};
pub use providers::chat::{
    ChatRequest, CompatibilityCheck, CompatibilityReason, ContentBlock, Degradation, ImageData,
    Message, ProviderNeeds,
};
pub use providers::context::{Caller, CancellationToken, LoggedCall};
pub use providers::error::{ErrorClass, LlmErrorKind};
pub use providers::events::{ChatEvent, StopReason};
pub use providers::http_extras::HttpOverrides;
pub use providers::model::ThinkingMode;
pub use providers::output_limit::{CatalogSource, OutputLimit};
pub use providers::provider_profile::{
    InterfaceKind, Modality, PromptCacheKind, ProviderProfile, ProviderProfileError,
    StructuredOutputMode, Support,
};
pub use providers::response::{ChatResponse, ChatResponseBuilder};
pub use providers::schema::{JsonSchema, SchemaRootError, schema_root};
pub use providers::tools::{
    ArgumentText, BlankToolNameError, JsonKind, MalformedArguments, ToolCall, ToolChoice, ToolSpec,
    arguments_object, parse_arguments,
};
pub use providers::usage::{CostUsd, InvalidRateError, Pricing, Rate, Usage};
pub use run_context::{RUN_CONTEXT, spawn_with_context};
pub use secret::{SecretRef, SecretString};
pub use step::cap::{
    ApprovalRequest, ApprovalSink, Cap, CapCtx, CapError, CapGate, CapSpec, CapSpecError,
    CapSpecProblem, Effect, EffectTierGate, EmptyIdempotencyKey, GatedCap, IdempotencyKey,
    InvalidArgs, NoopGate,
};
pub use step::context::StepContext;
pub use step::fn_step::FnStep;
pub use step::goal::{Acceptance, Goal, GoalExt, Judgement, SimpleGoal};
pub use step::intent::{ChatIntent, Intent, JsonIntent, StrIntent};
pub use step::outcome::{
    Allowance, Audience, JournaledReason, Outcome, RECORD_RAW_MAX, Reason, Refs, Release, Wait,
    expired,
};
pub use step::permission::{Permissions, PolicyVerdict};
pub use step::requires::{CapReq, Requires, Unmet};
pub use step::schedule::Schedule;
pub use step::typed_cap::{TypedCap, schema_of};
pub use step::{FailedRelease, RetainedResource, Step};
pub use step::{cap, goal, intent, outcome, permission, requires};
pub use task::{
    ClaimedTask, ClockError, Event, EventKind, InterruptionStreak, Lease, LeaseToken, Millis,
    RunnerId, Status, Task, TaskSpec, unix_millis,
};
pub use telemetry::{CallTelemetry, RetryAttempt};
pub use validation::{OutcomeSummary, ValidationOutcome, ValidationReason, ValidationSummary};
