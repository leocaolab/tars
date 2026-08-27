Shared core types for TARS — the single source of truth for the data shapes that flow between Provider / Pipeline / Runtime / Frontend layers; types, conversions, pure helpers, no business logic.

- Role (hex): core (shared kernel — every other crate sits above it)
- Effect budget: none (no I/O at runtime; NOTE: dep list is not clean — see friction below)
- Deps: may depend on [tars-sandbox (SandboxPolicy carried on RequestContext), serde, thiserror, chrono, sha2, url, uuid]; MUST NOT import [rusqlite → tars-storage/tars-cache/tars-melt own SQLite; axum → tars-server; middleware/provider machinery → tars-pipeline/tars-provider]
- Owns concepts: [ChatRequest, Message, ContentBlock, ChatEvent, StopReason, ChatResponse(+Builder), Usage, CostUsd, Pricing, Capabilities, ProviderError, ErrorClass, RequestContext, RUN_CONTEXT, typed IDs (TenantId, SessionId, TraceId, TrajectoryId, …), Principal, Scope, ModelHint, ToolSpec, ToolCall, JsonSchema, SecretString, PipelineEvent, ContentRef, Bless]
- Reason to change (the ONE): the cross-layer data vocabulary changes (a new field/variant in a shared contract type)
- Belongs here: a new ChatEvent variant; a new strongly-typed ID; a pure conversion/builder over these types
- Does NOT belong: an HTTP call or SSE parsing → tars-provider; a middleware or chain concern → tars-pipeline; a multi-step algorithm over these types → tars-utils

Friction (known, accepted-with-eyebrow): `reqwest` is a dependency of this "pure types" crate — for `impl From<reqwest::Error> for ProviderError` (src/error.rs:255) and header types in src/http_extras.rs — and `tokio` for the `RUN_CONTEXT` task-local. The effect budget stays none (nothing is called), but the dep graph makes the types crate pay for an HTTP client.
