The middleware pipeline framework (Doc 02) — LlmService = one provider + one model + an ordered handler chain of Middleware; the one public service concept, no service trait, no dyn.

- Role (hex): core (the chain framework + middleware policies) — but see friction: it composes over concrete sibling crates, not pure ports
- Effect budget: none directly — every effect is reached through an injected dependency (provider streams via tars-provider, cache via tars-cache's CacheRegistry, event emission via tars-melt's stores); clock (retry/circuit-breaker timing via tokio)
- Deps: may depend on [tars-types, tars-provider (the terminal of the chain), tars-cache (CacheLookupMiddleware), tars-melt (EventEmitterMiddleware → E-pillar stores), tokio, sha2, uuid]; MUST NOT import [reqwest → raw HTTP is tars-provider's; rusqlite → the three store owners; tars-runtime/tars-tools → upper layers call the pipeline, never the reverse]
- Owns concepts: [LlmService, LlmServiceBuilder, Middleware, Next, ChainOpts, EventStores, RetryMiddleware, CacheLookupMiddleware/set_cache_policy, PerCallBudgetMiddleware, TenantBudget*, CircuitBreaker, TelemetryMiddleware, EventEmitterMiddleware, validation middleware]
- Reason to change (the ONE): the chain contract or a cross-cutting request policy changes (order law, a new middleware)
- Belongs here: a new Middleware impl; a chain-order rule; RequestContext-attribute plumbing for a policy
- Does NOT belong: a provider backend or SSE handling → tars-provider; cache key law / storage → tars-cache; the agent loop (call→tools→re-call) → tars-runtime; event store schemas → tars-melt

Friction (flagged): hex-purity would have the framework depend on ports only; in reality the crate imports the concrete tars-cache / tars-melt / tars-provider crates (each of which co-locates port + adapter). Accepted as workspace convention — but it means "core" here is transitively wired to SQLite and reqwest.
