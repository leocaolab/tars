Multi-level LLM response cache (Doc 03) — the CacheRegistry port plus its in-process (moka) and persistent (SQLite) implementations, and the key-construction law.

- Role (hex): port + adapter(moka L1, SQLite L2) — the trait and its backends live together
- Effect budget: db (rusqlite L2 — one of the three sanctioned SQLite owners, for the personal cache store ONLY) | clock (via the `Clock` trait; `SystemClock` is the only impl that reads real time) | fs (XDG dir resolution for `default_personal_cache_path`)
- Deps: may depend on [tars-types, moka, rusqlite, tokio, sha2, dirs]; MUST NOT import [tars-provider/tars-pipeline → the cache must not know who calls it (the pipeline's CacheLookupMiddleware adapts pipeline→cache, not here); tars-storage → different SQLite store, different truth (recovery vs cache) — no sharing of connections or schemas]
- Owns concepts: [CacheRegistry, CachedResponse, MemoryCacheRegistry, SqliteCacheRegistry, CacheKey, CacheKeyFactory (hasher_version / tenant+IAM-scoped keys / temperature≠0 fail-fast), CachePolicy, CacheLayerPolicy, CacheError, Clock, SystemClock]
- Reason to change (the ONE): the caching contract changes (key law, eviction, a new cache level)
- Belongs here: an L2 schema migration; a key-scoping rule (IAM/tenant); a new registry backend (Redis) behind the same trait
- Does NOT belong: WHEN to consult the cache in a request flow → tars-pipeline (CacheLookupMiddleware, set_cache_policy); provider-side cache handles (cache_control/cachedContent) → tars-provider + tars-types (CacheDirective); trajectory/event persistence → tars-storage / tars-melt
