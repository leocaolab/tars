Shared core types for TARS — the single source of truth for the data shapes that flow between Provider / Pipeline / Runtime / Frontend layers; types, conversions, pure helpers, no business logic.

- Role (hex): core (shared kernel — every other crate sits above it)
- Effect budget: none (no I/O at runtime; NOTE: dep list is not clean — see friction below)
- Deps: may depend on [tars-sandbox (SandboxPolicy carried on Caller), serde, schemars, async-trait, thiserror, chrono, sha2, url, uuid]; MUST NOT import [rusqlite → tars-storage/tars-melt own SQLite; axum → tars-server; middleware/provider machinery → tars-llm]
- Owns concepts: [ChatRequest, Message, ContentBlock, ChatEvent, StopReason, ChatResponse(+Builder), Usage, CostUsd, Pricing, ErrorClass, LlmErrorKind, Caller, RUN_CONTEXT, typed IDs (TenantId, SessionId, TraceId, TrajectoryId, …), Principal, Scope, ToolSpec, ToolCall, JsonSchema, SecretString, the Step / Cap / Outcome / Requires contract (`step`), the durable store records (`task`)]
- Reason to change (the ONE): the cross-layer data vocabulary changes (a new field/variant in a shared contract type)
- Belongs here: a new ChatEvent variant; a new strongly-typed ID; a pure conversion/builder over these types
- Does NOT belong: an HTTP call or SSE parsing → tars-llm; a middleware or chain concern → tars-llm; a multi-step algorithm over these types → tars-utils

Friction (known, accepted-with-eyebrow): `tokio` is a dependency of this "pure types" crate, for the `RUN_CONTEXT` task-local. The effect budget stays none (nothing is called), but the dep graph makes the types crate pay for a runtime.
