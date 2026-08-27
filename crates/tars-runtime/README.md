The Agent Runtime (Doc 04 + Doc 14 §9) — the event-sourced trajectory core plus the NATIVE agent implementations (Session loop, orchestrator/worker/critic, judges, durable sync) built on the lower layers' abstractions.

- Role (hex): core (application/use-case layer — implements the tars-agent port; all effects reached through injected lower-layer abstractions)
- Effect budget: none directly — persistence via tars-storage traits (AgentEventLog, DurableStore), LLM via tars-pipeline (LlmService), tools via tars-tools (ToolRegistry); clock/async via tokio. src/durable/ explicitly gains NO rusqlite dep (documented in durable/mod.rs)
- Deps: may depend on [tars-agent (the contract it implements), tars-types, tars-storage, tars-pipeline, tars-tools, sha2 (idempotency keys), uuid]; MUST NOT import [rusqlite → tars-storage owns recovery SQLite (the durable module's own comments enforce this); reqwest → tars-provider; tars-provider directly → the pipeline is the LLM surface (tars-provider appears only in dev-dependencies, for test fixtures)]
- Owns concepts: [AgentEvent, Runtime, LocalRuntime, TarsAgent, EnsembleAgent, Session, SessionOptions, Budget, bind, CheckRunner/Invariant, judges (judge, judge_stats, arg_judge, metamorphic, trajectory_match), orchestrator/worker/critic, prompt, run_report, durable sync]
- Reason to change (the ONE): how a native agent RUNS a task changes (loop, recovery, judging, orchestration)
- Belongs here: a Session-loop behavior; an AgentEvent variant + its replay; a judge/critic protocol
- Does NOT belong: the Agent trait itself → tars-agent (abstractions must not live in the implementation crate — that smell is why tars-agent was extracted); a store schema → tars-storage; a middleware → tars-pipeline; a builtin tool → tars-tools

Friction (mild, flagged): 25+ top-level modules (sessions, judges, sync, prompts, orchestration) share one crate — the "ONE reason to change" above is honest only at the coarse grain; this crate accretes every runtime-adjacent concern and is the most likely future split candidate.
