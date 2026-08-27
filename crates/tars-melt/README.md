Telemetry initialization (tracing subscriber install + format switch + opt-in OTLP export) PLUS the read-able E-pillar event stores (PipelineEventLog + LlmRecordStore) moved here from tars-storage per Doc 17 §7.

- Role (hex): adapter(tracing/OTLP) + port + adapter(SQLite E-pillar stores) — honestly TWO crates in one; see friction
- Effect budget: db (rusqlite — sanctioned SQLite owner #3, for observability/eval stores ONLY) | network (OTLP gRPC span/metric export — `otlp` feature only, off by default) | (stderr log emission via tracing-subscriber)
- Deps: may depend on [tars-types (env:: reads only — Cargo.toml documents the accepted edge), tracing-subscriber, rusqlite, tokio, bytes, opentelemetry* (feature-gated)]; MUST NOT import [tars-pipeline/tars-provider → the emitting side must depend on melt, never the reverse; tars-storage → the two event planes stay separate (no shared generic EventStore, Doc 17 Q1)]
- Owns concepts: [init/init_or_warn, TelemetryConfig, TelemetryFormat, TelemetryGuard, TelemetryError, event::{PipelineEventLog, SqlitePipelineEventLog, LlmRecordStore, SqliteLlmRecordStore}]
- Reason to change (the ONE): cannot be stated as one — (a) how tars binaries emit/format/export telemetry changes, OR (b) the E-pillar store contract/schema changes. This is the crate's structural smell.
- Belongs here: an EnvFilter/format rule; the OTel layer composition; a PipelineEventLog schema migration
- Does NOT belong: emitting events during a request → tars-pipeline (EventEmitterMiddleware calls in); recovery-truth trajectory events → tars-storage (AgentEventLog); reading events for a CLI view → tars-cli (`tars events`)

Friction (flagged): "telemetry init" and "durable SQLite event stores" share one crate and one version bump. The move was deliberate (Doc 17 §7) but the crate now claims two hex roles and two reasons to change; `src/event/README.md` marks the internal boundary.
