Module placement contract — `tars_melt::event` (the E-pillar stores).

This submodule is a DIFFERENT hex role from the rest of tars-melt: the parent crate is telemetry-init (subscriber/format/OTLP wiring); this folder is a port + SQLite adapter — two durable, full-fidelity, never-sampled stores (PipelineEventLog, LlmRecordStore) read back by eval / `tars events` / replay.

- Belongs here: store traits, their SQLite impls, schema migrations, ContentRef-addressed record CAS
- Does NOT belong: subscriber/formatter/exporter code → crate root (lib.rs / otlp.rs / metrics.rs); recovery-truth trajectory events → tars-storage AgentEventLog; the code that EMITS into these stores → tars-pipeline EventEmitterMiddleware
- Effect: db (rusqlite). The crate root's effects (stderr, OTLP network) do not belong in this folder, and this folder's rusqlite must not leak into the crate root.
- The producing pipeline writes once and never reads back; anything here that starts serving the write-path's runtime decisions is misplaced.
