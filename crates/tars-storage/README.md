Persistent stores for the TARS Runtime — the recovery-truth plane: AgentEventLog (trajectory replay), Blackboard (coordination), DurableStore (job/result board), each a port + its SQLite impl.

- Role (hex): port + adapter(SQLite)
- Effect budget: db (rusqlite — this crate is a sanctioned SQLite owner, for RECOVERY-TRUTH stores only) | fs (XDG path resolution for default store locations)
- Deps: may depend on [tars-types, rusqlite, tokio, dirs]; MUST NOT import [tars-pipeline/tars-runtime → stores must not know their consumers; reqwest → tars-provider; tracing-subscriber init → tars-melt]
- Owns concepts: [AgentEventLog, EventRecord, Blackboard (+laws), DurableStore, StorageError, sqlite open helpers (open_agent_event_log_at_path, …)]
- Reason to change (the ONE): a recovery/coordination persistence contract or its schema changes
- Belongs here: an AgentEventLog schema migration; a new Blackboard law + its SQLite impl; a new durable store behind a trait
- Does NOT belong: the observability E-pillar stores (PipelineEventLog, LlmRecordStore) → tars-melt::event (Doc 17 §7: MELT, not recovery truth — lib.rs states the split); LLM response caching → tars-cache (its own SQLite, its own truth); WHAT events mean / when to append → tars-runtime

Boundary note: rusqlite is legitimately imported by exactly three crates — tars-storage (recovery), tars-cache (L2 cache), tars-melt (E-pillar). Any other crate reaching for rusqlite is bypassing an owning layer.
