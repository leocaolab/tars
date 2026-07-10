# Doc 06 — Configuration & Runtime Handle (multi-tenant & multi-workspace)

> Status: **current architecture.** Supersedes the earlier shared-process SaaS config design,
> which is kept **verbatim and DEPRECATED** in the Appendix at the end of this file (historical
> record — do not resurrect without re-opening the process-isolation decision).
> Angle: **tars is a general agentic-runtime library.** arc / concer / tars-py are
> *consumers* (examples), never the subject. Every abstraction here is tars's.
> Event-store internals are **out of scope** — deferred to Task 4 (only the
> *placement* decision is fixed here).

> **⚠️ Update (post scope-facade simplification).** The single bundled **`Tars` handle**
> this doc designed (§C3 / §4 SCOPE box / §10) was **subsequently removed**. `tars-handle`
> is now a *thin bundle of standalone pieces*, not a per-scope handle:
> - **global config + registry:** `tars_handle::init(Config)` (DI) / `init_from_home(home)`
>   (CLI) install the process-global `Config` (`Config::get()`) and eagerly build the one
>   `ProviderRegistry` (`ProviderRegistry::global()` / `try_global()`); `is_initialized()`.
> - **role → provider/service:** the free functions `resolve_role` / `resolve_role_bound` /
>   `resolve_service` / `resolve_provider_id` (against an explicit `&cfg.roles`,
>   `&cfg.routing`, `&registry`) — no scope object.
> - **paths:** `resolve_workspace_root`, `workspace_store_dir`, `tars_home_store_dir`,
>   `standalone_store_dir`, `StoreScope` (the §7 path law, still live).
> The load-bearing *decisions* below (process isolation, config layering, the path law,
> observability-only store) all stand; only the **bundling** into one `Tars` struct — and
> its per-scope sink + cancel + deterministic-Drop lifecycle — is gone. Scope/lifecycle is
> now the **consumer's** to own (tars ships DI seams, not a handle). Read the sections below
> with that substitution in mind.

---

## 1. Overview & goal

tars today has all the pieces of an agentic runtime — `Config`, `ProviderRegistry`,
`LlmService`, `LocalRuntime`, `EventStore` — but **no single entry that binds them for a
scope**, so every consumer hand-wires `load_from_file → from_config → builder → new`
independently (the "scatter"). This refactor introduces:

1. A **global layer** (process-wide, immutable): `Config` + `ProviderRegistry`,
   loaded once, shared.
2. A **scope layer**: per-*workspace* (local) or *tenant×workspace* (server) roles +
   observability + cancellation. **(As shipped, this is not a bundled `Tars` handle —
   see the update banner: tars provides standalone pieces and the consumer owns the
   scope.)**
3. A **path-resolution law**: where config lives, where the store lives, how a
   workspace is discovered — one rule for CLI, GUI/DMG, standalone, and server.
4. A **context law**: `tokio::task_local!` inside Rust; explicit ctx at every language
   boundary.
5. A **deterministic lifecycle**: open → switch → close (Drop + cancel) → reconstruct.

### The load-bearing decision — process isolation

**One Runtime = one user (local) / one tenant (server). We NEVER mix tenants in one
process.** This is cloud-native **process/container isolation**, not shared-process
multi-tenancy. Consequence: within a process there is exactly one tenant, so the global
`Config`/`ProviderRegistry` singleton is *correct*; "multi-tenant server" = orchestrate
**N single-tenant processes**, each with its own singleton. This collapses an entire
class of complexity (per-request registry keying, cross-tenant locks, shared-process
authz) into "spin another process."

### Non-goals

- Not redesigning the event store (Task 4; only placement fixed here).
- Not building shared-process SaaS multi-tenancy (process isolation replaces it).
- Not a generic config framework — this serves the agentic runtime's three layers
  (provider / pipeline / runtime).
- No config hot-reload (desktop-tool convention: close+reopen workspace for
  workspace config; restart process for global config).

---

## 2. Prior art — rig / OpenClaw / Hermes

| Axis | **rig** (lib) | **OpenClaw** (daemon) | **Hermes** (daemon) | **tars → this design** |
|---|---|---|---|---|
| Shape | library, everything a trait | opinionated daemon | opinionated daemon | **library** (like rig) |
| Entry | `openai::Client::from_env()` (stateless, cheap) | `$OPENCLAW_STATE_DIR` (`~/.openclaw`) | `$HERMES_HOME` (`~/.hermes`) | **`tars_handle::init` + global `Config::get()` / `ProviderRegistry::global()`; role via `resolve_service`** |
| Config home | app-provided | `~/.openclaw/openclaw.json` | `~/.hermes/*.json` | `~/.tars/config.toml`, **flag > env > default** |
| Data location | app decides (pluggable stores) | `~/.openclaw/memory/<agentId>.sqlite` | `~/.hermes/state.db` | **per-workspace** `<root>/.<tool>/tars/` (default); `~/.tars` fallback |
| Partition key | `PromptRequest::conversation` | **agentId** | **session** | **`tenant_id` × workspace** |
| Global init/singleton | **none** — all DI | daemon owns it | daemon owns it | **global singleton for immutable config/registry** (process isolation makes it safe); **DI handle** for scope |
| Backend | pluggable (LanceDB/Qdrant/…) | SQLite | SQLite / Postgres | **`EventStore` trait**: SQLite (Personal) / Postgres (Team) |
| Env override | — | `$OPENCLAW_STATE_DIR` | `$HERMES_HOME` | **`$TARS_HOME`** |

**What we take from each:** rig's *library + trait + DI-handle* shape and stateless
provider client; OpenClaw/Hermes's *home-dir + partition-by-id + env-override + pluggable
SQLite/Postgres backend*. **Where tars diverges:** it is directory-based (a workspace/repo),
which neither daemon models — so the store follows the **workspace**, not a global home,
and the global home is the *fallback* for the non-directory (standalone) case.

---

## 3. Consumer journeys (CUJ)

- **CUJ-1 — CLI tool on a repo.** `arc` runs in a git repo → resolves workspace root =
  git-root → per-workspace store under `<root>/.arc/tars/`. Global providers from `~/.tars`.
- **CUJ-2 — GUI app (DMG) opens a folder.** concer.app launched from `/Applications`
  (irrelevant) → user Opens Folder X → workspace = X → store `X/.concer/tars/`. First open
  bootstraps `X/.concer/`.
- **CUJ-3 — switch / multiple workspaces.** GUI opens A then B; closes A. Global registry
  is *not* rebuilt; B gets its own handle; A's in-flight jobs keep running (bound to A) and
  A's Drop is deferred until they finish.
- **CUJ-4 — standalone (no directory).** GUI launched with no folder open, or a bare CLI
  task → no workspace → global fallback `~/.tars`, partition by session id.
- **CUJ-5 — restart / reconstruct.** Process restarts → reopen remembered workspace root(s)
  → rebuild handle (registry shared, roles from `<root>/.<tool>/config.toml`, store
  reconnected to the on-disk file) → domain-derived resume of unfinished work.
- **CUJ-6 — multi-tenant server (future).** N tenants → **N single-tenant processes** (one
  per tenant / container), each identical to CUJ-1..5 with `tenant_id` = the tenant and
  `EventStore` = Postgres (Team mode). Same handle shape; different bindings.

---

## 4. The layered model

```
┌─ GLOBAL (process-wide, immutable, built once) ──────────────────────────┐
│  Config::get()          ~/.tars/config.toml  (providers, keys, routing)  │
│  ProviderRegistry::global()   Arc, all providers built once, shared      │
└──────────────────────────────────────────────────────────────────────────┘
             │ shared Arc ref
┌─ SCOPE  (per workspace [local] / per tenant×workspace [server]) ─────────┐
│  Standalone pieces — no bundled handle. The CONSUMER owns the scope:      │
│     cfg.roles / cfg.routing            // role → provider id (from Config) │
│     ProviderRegistry::global()         // shared ref to the global        │
│     resolve_service(roles, routing, registry, role) -> LlmService         │
│     (store dir via workspace_store_dir(...); StoreScope §C4 — consumer-    │
│      opened; cancel/lifecycle consumer-owned)                             │
└──────────────────────────────────────────────────────────────────────────┘
             │ DI (Tauri State keyed by root / CLI main / per-request)
┌─ CALL   (per operation) ────────────────────────────────────────────────┐
│  RUN_CONTEXT (task_local): { tenant, session, trace, tags }  — ids only  │
│  resolve_role(...) / resolve_service(...)   (Rust)                        │
│  tars.provider(role) / tars.pipeline(role)  (py/node bindings)           │
└──────────────────────────────────────────────────────────────────────────┘
```

**Why the split:** the registry is a function of the *global* config → build once, share
(a global immutable singleton is correct under process isolation). The roles are a
function of the *workspace* config → resolved per call against `cfg` (a global that a
consumer swaps per workspace). The call context is per-operation → task_local, ids only
(kept light; see §9).

### Three entry paths (a consumer picks its depth)

A consumer enters at exactly the layer it needs — the three nest internally:

| Entry | Rust | py/node binding | Use |
|---|---|---|---|
| provider-only | `resolve_role(...)` → `Arc<dyn LlmProvider>` | `tars.provider(role)` | one raw LLM call, stateless |
| service (bypass runtime) | `resolve_service(...)` / `LlmService::default_chain(...)` | `tars.pipeline(role)` | single agent call w/ retry/cache/obs — no DAG |
| runtime (DAG) | `LocalRuntime::new(store)` → `run_plan(...)` | *(deferred in bindings)* | dependency-scheduled multi-step workflow |

A single agent call does **not** go through `run_plan` — it's one call at the service layer,
not a one-node DAG.

---

## 5. Config layering

`manager.rs:16-18` reserves a `tenants` field; `manager.rs:88-89` names an aspirational
**5-layer merge** (*Compiled → Built-in → System → User → Tenant → Per-Request*) — the full
shared-process version of which is the **DEPRECATED design in the Appendix**. This (current)
design realizes only the two layers that matter now and leaves the rest as declared seams:

| Layer | Source | Scope | Status |
|---|---|---|---|
| Compiled / Built-in | `merge_builtin_with_user` (`manager.rs:10,108`) | global | exists |
| System | `/etc/tars/config.toml` (optional) | global | seam (future) |
| **User** | `~/.tars/config.toml` (`paths.rs:25`) | global | **this design** |
| **Tenant** | per-tenant overlay (server: config store; local: n/a) | tenant | **seam wired** (§8) |
| Per-Request | `RUN_CONTEXT` overrides (`tags`, model hint) | call | **this design** (§9) |
| Workspace (tars-specific) | `<root>/.<tool>/config.toml` `[roles]` (`routing.rs:26`) | workspace | **this design** |

> tars adds a **Workspace** layer the 5-layer model didn't name — it sits below User and
> above Per-Request, and holds only role→provider mapping (`RoutingConfig`), no secrets.

---

## 6. Components

### C1 — `Config` (global, immutable singleton)
- **Reuse:** `Config` struct `manager.rs:21`; `ConfigManager::load_from_file` `:94`,
  `load_from_str` `:115`; `default_config_path` `paths.rs:25`; `merge_builtins_into`
  `:108`; `Config::validate` `:66`.
- **New:**
  ```rust
  static CONFIG: OnceLock<Config> = OnceLock::new();
  impl Config {
      /// Composition root, once. `home`: explicit override (from --tars_home).
      /// Named `load`, NOT `init` — it's a one-time global *load*, not a
      /// side-effecting "initialize the framework" call.
      pub fn load(home: Option<PathBuf>) -> Result<(), ConfigError>;
      pub fn get() -> &'static Config;
      /// --tars_home flag > $TARS_HOME > ~/.tars
      pub fn resolve_home(flag: Option<PathBuf>) -> PathBuf;
  }
  ```
  `load` loads+validates (fallible handled here) then `CONFIG.set`. `get` is infallible
  (`expect("Config::load not called")`). No hot-reload.

### C2 — `ProviderRegistry` (global, built-once, Arc-shared)
- **Reuse:** `ProviderRegistry` `registry.rs:64` ("Cheap to clone (everything is Arc)"
  `:62`); `from_config` `:86` (eager — builds *all* declared providers).
- **New:**
  ```rust
  static REGISTRY: OnceLock<Arc<ProviderRegistry>> = OnceLock::new();
  impl ProviderRegistry {
      pub fn global() -> Result<Arc<ProviderRegistry>, RegistryError>; // lazy from Config::get()
  }
  ```
  - **Correctness note:** a global static singleton is safe *only* under process isolation
    (one tenant/process). It is the one thing that would break shared-process multi-tenancy;
    we choose process isolation precisely so this stays simple.
  - **Open (measure later):** `from_config` is eager (builds every declared provider even if
    unused). Acceptable for now; a lazy per-id build is a future optimization, not a blocker.

### C3 — role resolution (standalone functions; **supersedes** the earlier `Tars` handle)
- **Reuse:** `LlmService::default_chain(provider, model, opts)` (canonical onion);
  `LlmService::of(provider, model)` (leaf); `LocalRuntime::new(event_store)`
  `runtime.rs`; `run_plan` `executor.rs:695`; `RoutingConfig` `routing.rs`.
- **Shipped surface** (`tars-handle`, no scope struct — every input a plain argument):
  ```rust
  // Composition root: install the process-global Config + build the one registry.
  pub fn init(config: Config) -> Result<(), InitError>;          // DI (embedder)
  pub fn init_from_home(home: Option<PathBuf>) -> Result<(), InitError>; // CLI
  pub fn is_initialized() -> bool;

  // role → provider / model-bound service, against an EXPLICIT registry + routing + roles.
  pub fn resolve_role(roles, routing, registry, role) -> Result<(ProviderId, Arc<dyn LlmProvider>), TarsError>;
  pub fn resolve_role_bound(roles, routing, registry, role) -> Result<(ProviderId, Arc<dyn LlmProvider>, String), TarsError>;
  pub fn resolve_service(roles, routing, registry, role) -> Result<LlmService, TarsError>; // business-facing leaf
  pub fn resolve_provider_id(roles, routing, registry, role) -> Result<ProviderId, TarsError>;
  ```
  Role resolution = `cfg.roles` (flat `[roles]` map) → tier → literal id → `default` tier
  → sole provider → `UnknownRole`. Two config layers meet here (§5). The provider+model
  come from `cfg`/`registry`; `resolve_service` returns a leaf `LlmService` (wrap it in the
  onion with `LlmService::default_chain` / `builder_with_inner`). **The per-scope
  observability sink + cancel + deterministic-Drop lifecycle of the old handle are no longer
  a tars type — the consumer owns the scope + its store handle** (the store is a write-only
  side-log, Task 4; §C4 placement still applies).

### C4 — `StoreScope` (placement; sink internals = Task 4)
- **Reuse:** `default_personal_event_store_path` `sqlite.rs:292`; Personal/Team split
  `event_store.rs:5-6`.
- **New:**
  ```rust
  enum StoreScope {
      Workspace(PathBuf), // <root>/.<tool>/tars/    default (data follows project)
      TarsHome(PathBuf),  // ~/.tars/ws/<path-hash>/ fallback (read-only dir / policy)
      Off,                // opt-out
  }
  ```
  Resolved by the consumer at workspace-root resolution (§7). **Only placement is fixed here; the sink (MPSC single
  writer, best-effort, EventStore-trait backend) is Task 4.**
  - **What the store is NOT (fixed now, so Task 4 stays honest):** observability only — a
    write-only side-log. It is **never** the scheduler and **never** the durability source
    (durability = consumer domain state). The executor (`run_plan`) is already an async
    dependency-DAG and does **not** read the store to schedule. *(Reviews that claim "a
    dropped event stalls the DAG" or "a lost event breaks idempotency" assume the store
    drives scheduling/reconcile — it does not; both are rejected on that premise.)*
  - **Task 4 backlog (carried from review, not decided here):** (1) multi-process
    `SQLITE_BUSY` when two instances of the *same* tool write one `.<tool>/tars/` — needs a
    `busy_timeout` + backoff (per-tool isolation stops arc-vs-concer, not arc-vs-arc);
    (2) fat-payload blob offloading — large bodies to `.<tool>/tars/blobs/` with a pointer in
    SQLite (tars already splits `bodies.db` from `pipeline_events.db` — partial); (3) a CLI
    `flush().await` / `shutdown(self)` so a bare `tokio::main` exit drains the writer instead
    of truncating the log tail (best-effort only — not a durability guarantee); (4) fold
    `RUN_CONTEXT` ids into `tracing::Span` for OTel/Datadog correlation;
    (5) **schema migration + version skew** — embed `sqlx::migrate!()` in `EventSink::open`,
    read forward-compatibly (ignore unknown columns); if the on-disk schema is *newer* than the
    binary, degrade gracefully ("upgrade the client") instead of a raw SQL panic (DBs scatter
    across user project dirs; an old binary / a second machine must not crash);
    (6) **order by monotonic sequence, never wall-clock** — replay/inspection orders by
    `events.sequence_no` (already the PK, `sqlite.rs`), NOT `SystemTime::now()` (NTP / sleep-wake
    can invert two events); timestamps are UI-only (reconcile reads domain state, not this log,
    so drift can't stall the DAG — but keep the invariant);
    (7) **corruption graceful-degrade** — store-open catches `SQLITE_CORRUPT` (power loss
    mid-WAL, NFS/Dropbox mounts), renames the bad file to `events.db.corrupted.bak`, opens a
    fresh empty store, and **starts anyway** — never lock the user out of a workspace over a
    corrupt *observability* log (precisely why the store is observability, not durability).

### C5 — `RUN_CONTEXT` (call context)
- **Reuse:** existing `RequestContext` (ids-only: tenant/session/trace/tags — the shape
  `concer_context` already sets); `service.call(req, ctx)`.
- **New:**
  ```rust
  tokio::task_local! { pub static RUN_CONTEXT: RequestContext; }
  ```
  Established once at the operation entry (Tauri command / run_plan entry / FFI boundary).
  Deep calls read implicitly; the sink reads ids from it. **Ids only — never providers**
  (kept light, §9).

---

## 7. Path resolution law

### Config home (global)
```
--tars_home <path>   >   $TARS_HOME   >   ~/.tars       (Config::resolve_home)
```

### Workspace root (deterministic, entry-independent)
```
resolve_workspace_root(entry):
  1. canonicalize(entry)                       // symlink / relative / trailing-slash → one path
  2. explicit open / --workspace / `tool .`    → that dir            (highest — user said so)
  3. walk up; STOP at the FIRST level with an existing .<tool>/   → that level is the root
       (the marker is the persisted declaration; the CLOSEST marker wins)
  4. else walk up to .git                       → git-root
  5. none of the above (bare dir, no git, no marker) → NO workspace → standalone
```
Same folder via DMG-GUI, CLI, or a CLI subdir all canonicalize + resolve to the **same**
root → the **same** store. Launch mode is irrelevant; only `(tool, canonical root)` matters.

> **Marker beats `.git` — the monorepo trap.** In a monorepo (`/mono/.git`, subprojects
> `/mono/backend`, `/mono/frontend`), a GUI that opened `/mono/backend` bootstraps
> `/mono/backend/.<tool>/`. A CLI later run in `/mono/backend/src` **must** stop at that
> marker, **not** walk past it to `/mono/.git` — otherwise GUI and CLI split into two stores
> for the same project. Hence `.<tool>/` (step 3) is checked **before** `.git` (step 4).

### Store scope
```
declared workspace + writable      → Workspace(<root>/.<tool>/tars/)   (bootstrap .<tool>/ on first use)
declared workspace + read-only,
  or [store] location="tars_home"  → TarsHome(~/.tars/ws/<hash>)
no workspace (standalone)          → ~/.tars (global), tenant = session
[store] enabled=false              → Off
```
**Never** a single shared cross-workspace store: it mixes projects' private I/O (privacy /
blast-radius) **and** breaks the per-workspace deterministic Drop (one shared writer can't
close on a single-workspace close).

### Multi-tool in one directory, and who resolves the location

When several tars consumers touch the **same** directory (e.g. `arc` reviewing a repo that
also holds `concer` docs):

- **Providers are shared, written once.** All tools read the same global `~/.tars/config.toml`
  — a tool does **not** copy provider defs. Only the tiny per-tool `[roles]`
  (`<root>/.<tool>/config.toml`) differs.
- **Stores are per-tool, isolated — not merged.** Each tool keeps its own
  `<root>/.arc/tars/`, `<root>/.concer/tars/`, … We do **not** fold them into one shared
  `<root>/.tars/`. Rationale: privacy (arc's code-review I/O ≠ concer's doc I/O), per-tool
  cleanup (delete one tool's dir without touching another), and zero cross-tool coordination.
  Three dirs is *isolation*, not duplication.

**tars never discovers the location itself.** The library must not call `current_dir()` — a
GUI has no meaningful CWD, and self-sourcing breaks DI. The **consumer** resolves the root
from its entry (GUI: OS Open-Folder dialog → held in app state; CLI: CWD + walk-up to
git-root, or `--workspace`) via `resolve_workspace_root(tool, entry)`, then builds its
scope from the global registry + `cfg.roles` (e.g. `resolve_service`).

---

## 8. Multi-tenant — minimal seam (deliberately NOT designed out)

Multi-tenant is a **small future addition**, not a thing to design now (minimal principle).
The current abstractions already don't preclude it:

- **Process isolation** — multi-tenant = **N single-tenant processes** (one per
  tenant/container), each identical to the local design. No shared-process work, no
  cross-tenant locks/authz in the runtime.
- **`tenant_id`** is already the partition key (local: the single user; server: from the
  request boundary). **`EventStore` is a trait** (SQLite Personal → Postgres Team later).

That's the whole seam. Carrying a `tenant_id` into the handle when a server needs it is an
**L4 contract on top** — **out of scope here**. Do **not** pre-build `for_tenant`,
per-tenant registry keying, or server authz until a real server needs them.

---

## 9. Context law (task_local inside; explicit at boundaries)

- **Inside Rust:** `RUN_CONTEXT.scope(ctx, async { … }).await` at the operation entry; deep
  `pipeline.call` / `runtime` never thread `ctx`; `EventSink::emit` reads ids via
  `RUN_CONTEXT.with(...)`.
- **Across `tokio::spawn`:** task_local does **not** propagate. Detached jobs (the async
  agent job that must survive the command returning) MUST re-establish
  `RUN_CONTEXT.scope(ctx.clone(), job)` inside the spawn. (`FuturesUnordered` in
  `run_plan` is the same task → inherits; `spawn` does not.) To kill this footgun at the
  source, tars ships a helper and **all internal detached tasks go through it**:
  ```rust
  pub fn spawn_with_context<F>(fut: F) -> JoinHandle<F::Output>
  where F: Future + Send + 'static, F::Output: Send + 'static {
      let ctx = RUN_CONTEXT.with(|c| c.clone());
      tokio::spawn(async move { RUN_CONTEXT.scope(ctx, fut).await })
  }
  ```
- **Across language boundaries (PyO3 / napi / Tauri command):** the boundary passes `ctx`
  **explicitly**; the binding re-establishes the scope. task_local is a Rust-internal
  elegance, never crossing FFI. (Detailed in Tasks 2/3.)
- **Weight:** context carries **ids only**; providers live in the handle as `Arc` (built
  once, shared). Passing a handle = a few `Arc` refcount bumps, not a provider copy.

---

## 10. Lifecycle (deterministic Drop + cancel)

The lifecycle is now the **consumer's** (tars no longer bundles a scope handle). A typical
Tauri consumer keeps its own per-root scope struct — the registry stays global/shared:

```
tauri::State<Mutex<HashMap<PathBuf, ConsumerScope>>>   // consumer-owned; no LRU/TTL

open_workspace(X):   root=resolve_workspace_root(tool, X); map.entry(root)
                       .or_insert_with(|| ConsumerScope::new(ProviderRegistry::global()?, &cfg.roles, root))
switch A→B:          registry NOT rebuilt (global, shared); B gets/creates its scope; A stays cached
close_workspace(A):  map.remove(A) → the consumer cancels its own jobs + drains its own store
restart:             reopen remembered root(s) → rebuild scope → reconnect on-disk store → domain resume
```

- **Drop is deterministic:** `remove` → (in-flight jobs still hold `Arc` → they finish/cancel
  → release) → last `Arc` drops → `EventSink` Sender drops → writer drains → SQLite pool
  closes. **`cancel()` before Drop** prevents a hung job pinning the handle.
- **In-flight jobs are bound to their workspace, not the active view** — they hold their own
  `Arc<sink>`/factory, so switching/closing doesn't corrupt them; they finish and write to
  their own store.
- **Reconstruct ≠ rebuild data:** the store file is on disk (reconnect); durable truth is the
  **domain state** (consumer-owned artifacts), which drives resume.

---

## 11. Interfaces with other modules

| Direction | Symbol | Signature / note |
|---|---|---|
| tars-config → runtime | `Config::get() -> &'static Config` | global immutable |
| tars-provider → handle | `ProviderRegistry::global() -> Arc<…>` | built once |
| scope → tars-pipeline | `LlmService::default_chain(provider, model, opts)` | events wired via `ChainOpts.events` (`EventStores`) |
| handle → tars-runtime | `LocalRuntime::new(store)` `runtime.rs:128`; `run_plan(...)` `executor.rs:695` | cancel token threaded |
| runtime/pipeline → sink | `EventSink::emit(ev)` (Task 4) | ids from `RUN_CONTEXT` |
| bindings → tars-handle | `tars_handle::init_from_home` / `resolve_role` | Tasks 2 (PyO3) / 3 (napi) — ctx explicit at boundary |

---

## 12. Reliability / Security / Performance

- **Reliability:** deterministic Drop (no leak — `Arc`→0 closes the pool); `cancel()` on
  close; reconstruct from on-disk store + domain-derived resume; no hot-reload race (config
  is snapshot-at-init).
- **Security:** `~/.tars/config.toml` holds provider *names* + `api_key_env` (never inline
  keys); keys from env. **GUI env-void:** a DMG/Launchpad-launched app does **not** inherit
  the shell's `export`ed keys, so `Config::load` must also read `~/.tars/.env` (dotenv)
  before resolving `api_key_env` — else GUI LLM calls fail "key not found" while the CLI
  works. Per-workspace store holds raw LLM I/O (prompts + user content) →
  gitignored, lives with the project (privacy). Server (future seam, §8): per-tenant
  secrets + DB isolation — not designed here; process isolation already means a tenant
  can't read another tenant's process memory.
- **Performance:** global singletons built once; handle = `Arc` clones; context = ids;
  registry eager-build is the one measured-later cost (§C2). Store writes are async
  (Task 4: MPSC single writer, non-blocking `try_send`).

---

## 13. Reuse map (Phase 0)

| Symbol | file:line | How we use it |
|---|---|---|
| `Config` | `tars-config/src/manager.rs:21` | global singleton payload |
| `ConfigManager::load_from_file` / `load_from_str` | `manager.rs:94` / `:115` | `Config::load` |
| 5-layer merge / `tenants` seam | `manager.rs:16-18`, `:88-89` | §5 layering |
| `default_config_path` | `paths.rs:25` | `resolve_home` default |
| `RoutingConfig` | `routing.rs:26` | workspace `[roles]` layer |
| `ProviderRegistry` / `from_config` | `registry.rs:64` / `:86` | global registry (built once) |
| `LlmService::default_chain` / `EventStores` | `tars-pipeline` `service.rs` / `middleware.rs` | `resolve_service(role)` / `tars.pipeline(role)` |
| `LocalRuntime::new` / `run_plan` | `runtime.rs` / `executor.rs:695` | `LocalRuntime::new(store)` → `run_plan(...)` |
| `CancellationToken` | `executor.rs:748` | cancel-on-close |
| `default_personal_event_store_path` / Personal-Team | `sqlite.rs:292` / `event_store.rs:5-6` | StoreScope fallback / backend axis |
| tars-py `Provider` / `Pipeline` / `EventStorePair` | `tars-py/src/lib.rs:350` / `:504` / `:605` | Task 2 binding baseline |

New abstractions (justified): `Config::get`/`Registry::global` (collapse the scatter under
process isolation), the `tars-handle` standalone resolvers (`resolve_role` /
`resolve_service` — the "single entry" that, post-simplification, replaced the originally
proposed bundled `Tars` handle, rig-style), `StoreScope` (placement law), `RUN_CONTEXT`
task_local (context law). *(The per-scope deterministic-Drop lifecycle is now
consumer-owned — see the update banner.)*

---

## 14. Roadmap (Task 1 scope)

- **M1 — global layer.** `Config::load/get/resolve_home` (`$TARS_HOME`/flag) +
  `ProviderRegistry::global()`. Verify: two consumers resolve the same registry; flag>env>default
  path test. *(no behavior change to existing load)*
- **M2 — role resolvers + StoreScope placement.** `resolve_role` / `resolve_role_bound` /
  `resolve_service` over the global registry + workspace roles; StoreScope resolution
  (workspace / tars_home / off) — **store still the current impl** (real consolidation is
  Task 4). Verify: CUJ-1/2/4 resolve correct paths; role→provider through both layers.
  *(As shipped: standalone functions, not a bundled `Tars` handle — see banner.)*
- **M3 — lifecycle (consumer-owned).** The consumer's `Mutex<HashMap<root, ConsumerScope>>`
  pattern, cancel + drain, workspace-root resolution (canonical + walk-up),
  reconstruct-on-restart. Verify: CUJ-3/5 (switch/close/restart) — registry not rebuilt,
  in-flight survives switch.
- **M4 — context law.** `RUN_CONTEXT` task_local; entry scoping; spawn re-scope rule
  documented + a spawn test. Verify: deep call reads ctx without threading; spawned job
  re-scopes.
- **M5 — multi-tenant seam (minimal, §8).** Only ensure `tenant_id` flows as the partition
  key and `EventStore` stays backend-swappable — **no** `for_tenant`, per-tenant keying, or
  authz until a server needs them. Verify: local `tenant_id`=user path unchanged; nothing
  precludes a later tenant param.

Sequenced risk-up-front: the singleton/handle split (M1/M2) and the deterministic lifecycle
(M3) are the load-bearing, least-reversible pieces — done first. Bindings (Tasks 2/3) and the
event-store consolidation (Task 4) build on the frozen handle shape.

---

---

# Appendix — DEPRECATED: shared-process SaaS config / multi-tenancy design

> ⚠️ **This appendix is the ORIGINAL Doc 06, preserved verbatim as a historical record. It is
> DEPRECATED and is NOT the current architecture.**
>
> Its core premise — **shared-process multi-tenancy** (one process serving many tenants via
> in-process config layering, hot-reload, `ArcSwap`, per-tenant cache/event partitioning) — was
> **deliberately replaced by process isolation** (see the current design above: *one Runtime =
> one tenant; multi-tenant = N single-tenant processes*).
>
> It is kept here **only** so the decision is on the record: this approach was considered in
> full and set aside for performance / complexity / deadlock reasons. **Do not resurrect any
> part of it as a requirement** without first re-opening the process-isolation decision.
>
> The genuinely-still-relevant *concerns* below (secret resolution, tenant lifecycle / GDPR,
> quotas & billing, audit) remain valid as **future server work** — but they will be
> implemented **per-process**, not via the in-process machinery described here.

---

# Doc 06 — Configuration & Multi-Tenancy Management

> Scope: define the layers, sources, priority, and hot-reload mechanism for configuration; the multi-tenant data model and isolation guarantees; secret management; tenant lifecycle; quotas and billing.
>
> Cross-cutting: this doc introduces no new runtime components — it standardizes the unified shape of the "configuration / tenant" dimensions already mentioned across Doc 01-05.

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Configuration as code** | All configuration is expressed in versioned text files; Git is the single source of truth (the DB is just a hot-reload cache) |
| **Hard tenant isolation** | tenant_id is a security boundary — isolates IAM / Cache / Budget / Auth / MCP subprocesses / event log |
| **Layered overrides** | Defaults → System → User → Tenant → Request; deeper layers override shallower ones, but some layers are forbidden from overriding |
| **Secrets never live in files** | All secrets are pulled by reference, resolved at runtime, never persisted in plaintext |
| **Hot-reloadability is explicit** | Not all config can be hot-reloaded; what can and cannot must be explicitly marked in the schema |
| **Validation up front** | Full validation at startup + on hot reload; validation failure rejects startup / rejects apply, no partial loading allowed |
| **Complete tenant lifecycle** | Provision / Suspend / Resume / Delete end-to-end; Delete must cascade-clean |
| **Observable quotas** | Per-tenant token / cost / cache usage queryable in real time, exportable as billing reports |

**Anti-goals**:
- Don't hardcode secrets in config — not even in dev (use a dev profile of the secret manager instead)
- Don't let tenant config override security constraints (IAM order, cache hasher_version, etc.)
- No "dynamic tenant discovery" — every tenant is explicitly registered through the provisioning flow
- Don't let config schema evolution break old tenants — there must be a migration path

---

## 2. Configuration Layers and Priority

```
                        ┌──────────────────┐
                        │ Per-Request      │  ← rarely used, mainly testing
                        │ overrides        │
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ Tenant overrides │  ← Postgres, hot-reloadable
                        │ (DB-backed)      │
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ User config      │  ← ~/.config/tars/*.toml
                        │ (file-backed)    │     (local deploy / dev env)
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ System config    │  ← /etc/tars/*.toml
                        │ (file-backed)    │     (production deploy default)
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ Built-in config  │  ← embedded in binary (ship with code)
                        │ (embedded)       │
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ Compiled         │  ← const in Default impl
                        │ defaults         │
                        └──────────────────┘
```

### 2.1 Priority Rules

- **Deeper overrides shallower** — Per-Request > Tenant > User > System > Built-in > Compiled
- **Arrays / Maps merge rather than replace** (unless explicitly marked `replace = true`)
- **Presence > default** — a config item written out, even with an empty value, counts as "explicitly set"
- **All merging happens at startup / hot reload** — at runtime you get an already-collapsed effective config, with no runtime branching

### 2.2 Layers Forbidden from Override

Certain layers must be locked at the system level; tenants/users cannot override them:

| Config item | Locked layer | Rationale |
|---|---|---|
| Pipeline layer ordering constraints (Doc 02 §7) | System | Security constraint, IAM must precede Cache |
| Cache hasher_version (Doc 03 §11) | System | Changing it invalidates cache for all tenants |
| Provider list itself | System | Tenants can only choose to enable, not introduce new provider instances |
| Audit log toggle | System | Compliance requirement, tenants are not allowed to disable |
| Tool `side_effect` classification (Doc 05 §3.1) | System | Security constraint, Irreversible cannot be downgraded by a tenant to Reversible |
| MCP server binary allowlist (Doc 05 §5.5) | System | Prevents arbitrary code execution |

```rust
pub struct ConfigLayer {
    pub source: ConfigSource,
    pub locked_keys: Vec<String>,          // key paths that downstream cannot override
}

// Startup validation: if Tenant config tries to override a locked key, fail immediately
fn validate_layer_overrides(...) -> Result<(), ConfigError> {
    for (key, _value) in tenant_overrides.flatten() {
        if system_layer.locked_keys.contains(&key) {
            return Err(ConfigError::AttemptedLockedOverride { key });
        }
    }
    Ok(())
}
```

---

## 3. Configuration Data Model

### 3.1 Top-level schema

```rust
pub struct Config {
    pub version: ConfigVersion,            // for migration
    pub providers: ProvidersConfig,        // Doc 01
    pub pipeline: PipelineConfig,          // Doc 02
    pub cache: CacheConfig,                // Doc 03
    pub agents: AgentsConfig,              // Doc 04
    pub tools: ToolsConfig,                // Doc 05 (incl. mcp_servers, skills)
    pub tenants: HashMap<TenantId, TenantConfig>,
    pub secrets: SecretsConfig,
    pub observability: ObservabilityConfig,
    pub deployment: DeploymentConfig,
}

pub struct TenantConfig {
    pub id: TenantId,
    pub display_name: String,
    pub status: TenantStatus,             // Active / Suspended / PendingDeletion
    pub created_at: SystemTime,
    pub provisioned_by: Principal,
    
    /// Tenant-level overrides (deep-merged into corresponding global section)
    pub overrides: TenantOverrides,
    
    /// Quota hard limits
    pub quotas: TenantQuotas,
    
    /// Subset of Providers visible to this tenant
    pub allowed_providers: Vec<ProviderId>,
    
    /// Subset of Tools / Skills visible to this tenant
    pub allowed_tools: Vec<ToolId>,
    pub allowed_skills: Vec<SkillId>,
    
    /// Visible MCP servers (will spawn isolated subprocesses)
    pub allowed_mcp_servers: Vec<McpServerId>,
    
    /// Isolation configuration
    pub isolation: TenantIsolation,
}

pub enum TenantStatus {
    Active,
    Suspended { since: SystemTime, reason: String },
    PendingDeletion { scheduled_for: SystemTime },
    Deleted { deleted_at: SystemTime, audit_ref: AuditRef },
}

pub struct TenantIsolation {
    /// HOME directory for CLI / MCP subprocesses (Doc 01 §6.2 + Doc 05 §5.3)
    pub subprocess_home: PathBuf,
    
    /// Cache key namespace prefix (Doc 03 §3.2 hard constraint)
    pub cache_namespace: String,
    
    /// Logical partition for the event log
    pub event_log_partition: String,
    
    /// Tenant-scoped secret namespace
    pub secret_namespace: String,
}
```

### 3.2 TenantOverrides shape

```rust
pub struct TenantOverrides {
    pub middleware_budget: Option<BudgetOverrides>,
    pub middleware_prompt_guard: Option<PromptGuardOverrides>,
    pub cache: Option<CacheOverrides>,
    pub agent_blueprints: Vec<AgentBlueprint>,        // tenant-defined Agents
    pub default_models: Option<HashMap<ModelTier, ProviderId>>,
}
```

Merge rules (deep merge):
- `Option<T>` field: Some(value) overrides, None inherits from parent layer
- `Vec<T>` field: **append** (not replace, unless explicit replace)
- `HashMap<K, V>` field: merge by key, deeper layer overrides on key collision

### 3.3 Workspace and Session

Below Tenant there are two more concepts — but these are not config layers, they are runtime entities:

```rust
pub struct Workspace {
    pub id: WorkspaceId,
    pub tenant: TenantId,
    pub display_name: String,
    pub principal_owners: Vec<Principal>,
    pub iam_scopes: Vec<Scope>,           // scopes provided by this workspace
    pub created_at: SystemTime,
}

pub struct Session {
    pub id: SessionId,
    pub workspace: WorkspaceId,
    pub principal: Principal,
    pub started_at: SystemTime,
    pub last_activity_at: AtomicSystemTime,
    pub ephemeral_state: SessionState,    // Cache handle ref / agent state
}
```

| Dimension | Tenant | Workspace | Session |
|---|---|---|---|
| Duration | Long-term (company / team level) | Mid-term (project / repo level) | Short-term (single workflow) |
| Isolation strength | Hard (security boundary) | Logical (IAM differentiated) | Soft (cache-sharing scenarios) |
| Order of magnitude | 10²-10³ | 10³-10⁴/tenant | 10⁵-10⁶/day |
| Config overrides | ✅ | ❌ (express differences via IAM scope) | ❌ |

---

## 4. Tenant Isolation Guarantees (Summary)

The isolation points discussed across earlier docs, consolidated along the tenant dimension:

### 4.1 Data isolation

| Data | Isolation mechanism | Doc reference |
|---|---|---|
| Cache key | TENANT + IAM_SCOPES go into the SHA-256 prefix | Doc 03 §3.2 |
| L3 Provider cache handle | tenant_namespace field enforced, cross-tenant reject | Doc 03 §10.2 |
| Provider-side prefix cache | tenant_marker injected into system prompt | Doc 03 §10.3 |
| Trajectory event log | event_log_partition logical partition | §3.1 |
| Content Store | tenant dimension prefixed onto hash | (Doc 04 §3.3 default behavior) |
| Budget Store | tenant_id is the first-level prefix of the Redis key | Doc 02 §4.3 |
| Idempotency Cache | tenant_id + trajectory_id is part of the key | Doc 05 §4.3 |

### 4.2 Process / resource isolation

| Resource | Isolation mechanism | Doc reference |
|---|---|---|
| CLI subprocess (Claude / Gemini) | per_tenant_home, independent OAuth state | Doc 01 §6.2 |
| MCP server subprocess | per_tenant_home + independent session pool | Doc 05 §5.3 |
| Embedded models (mistral.rs / ONNX) | not isolated (stateless inference), shared instance | Doc 01 §6.3 |

### 4.3 Network / auth isolation

| Credential | Isolation mechanism | Doc reference |
|---|---|---|
| Provider API key | per-tenant secret reference | §5 + Doc 01 §7 |
| OAuth token | secret_namespace isolation | §5 |
| MCP server auth | independent subprocess HOME | Doc 05 §5.3 |

### 4.4 Quota isolation

| Resource | Limit mechanism | Doc reference |
|---|---|---|
| Token consumption rate | per-tenant TPM/RPM, Redis token bucket | Doc 02 §4.3 + §9 (this doc) |
| Cost cap | per-tenant daily/monthly USD hard cap | same as above |
| L3 cache storage | per-tenant storage_quota_bytes | Doc 03 §11 |
| Trajectory concurrency | per-tenant max_concurrent_tasks | §9 |
| MCP subprocess count | per-tenant max_subprocess_count | §9 |

---

## 5. Secret Management

### 5.1 Never goes into config files

```toml
# ❌ Wrong: plaintext secret
[providers.openai]
api_key = "sk-proj-xxxxxxxxxxxxxxxxxxxxxx"

# ❌ Wrong: encrypted but stored next to its decryption key
[providers.openai]
api_key_encrypted = "AES256:abc..."
api_key_decrypt_key_path = "/etc/tars/master.key"  # on the same host

# ✅ Correct: reference an external secret manager
[providers.openai]
api_key = { source = "vault", path = "secret/data/tenants/${tenant_id}/openai/api_key" }

# ✅ Correct: reference an environment variable (suitable for dev)
[providers.openai]  
api_key = { source = "env", var = "OPENAI_API_KEY" }
```

### 5.2 SecretRef type

```rust
pub struct SecretRef {
    pub source: SecretSource,
    pub identifier: String,              // path / var name / KMS key id
    pub cache_ttl: Duration,             // cache duration after resolution, default 5min
}

pub enum SecretSource {
    Env,                                  // env var
    File,                                 // file path, suits K8s secret mount
    Vault,                                // HashiCorp Vault
    GcpSecretManager,
    AwsSecretsManager,
    AzureKeyVault,
    Inline,                               // dev only, warn at startup
}

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, refr: &SecretRef, ctx: &SecretContext) 
        -> Result<SecretValue, SecretError>;
    
    /// Proactive notification on secret invalidation (used for OAuth token refresh)
    fn invalidate(&self, refr: &SecretRef);
}
```

### 5.3 Per-tenant secret namespace

Each tenant's secrets live in an independent namespace to avoid cross-use:

```toml
[providers.openai]
api_key = { source = "vault", path = "secret/data/tenants/${tenant_id}/openai/api_key" }
```

`${tenant_id}` is a template variable, substituted by the SecretResolver with the tenant_id from the request context. So:
- The config file itself is tenant-agnostic, sharing a single template
- The actual secrets are physically isolated under different paths in the secret manager
- Cross-tenant secret access necessarily fails (path doesn't exist)

### 5.4 Secret caching and refresh

Secret resolution has a cost (tens of ms network round trip) and must be cached:

```rust
pub struct CachedSecretResolver {
    inner: Arc<dyn SecretResolver>,
    cache: Arc<DashMap<SecretCacheKey, CachedSecret>>,
}

struct CachedSecret {
    value: SecretValue,
    resolved_at: Instant,
    expires_at: Instant,
}
```

Refresh strategy:
- **Passive**: re-fetch on the next resolve after the cache TTL expires
- **Active**: OAuth refresh — on receiving 401, call `invalidate` + re-resolve immediately
- **Warm-up**: on tenant startup, pre-fetch its commonly used secrets to avoid first-request latency

**Never persisted**: secret cache lives only in memory; on process restart all entries are lost. Never written to disk / DB / Redis.

---

## 6. Configuration Hot Reload

### 6.1 Hot-reload classification

```rust
pub enum HotReloadability {
    /// Fully hot-reloadable, no runtime impact
    Hot,
    
    /// Hot-reloadable, but requires draining in-flight requests (e.g. changing routing policy)
    HotWithDrain,
    
    /// Requires restarting subprocesses (CLI / MCP server)
    SubprocessRestart,
    
    /// Requires full Runtime restart
    FullRestart,
    
    /// Never changeable (would break data integrity)
    Immutable,
}
```

Each config schema field is annotated with its hot-reload capability (via attribute):

```rust
#[derive(Config)]
pub struct CacheConfig {
    #[reload(Immutable)]                   // changing invalidates all cache
    pub hasher_version: u32,
    
    #[reload(Hot)]                         // takes effect immediately
    pub l1_capacity: u64,
    
    #[reload(HotWithDrain)]                // wait for current lookup to complete
    pub l2_url: String,
    
    #[reload(SubprocessRestart)]           // change restarts mcp server
    pub mcp_server_args: Vec<String>,
}
```

### 6.2 Hot-reload flow

```rust
pub struct ConfigManager {
    current: Arc<ArcSwap<EffectiveConfig>>,
    watchers: Vec<Arc<dyn ConfigWatcher>>,
    subscribers: broadcast::Sender<ConfigChangeEvent>,
}

impl ConfigManager {
    /// Trigger reload (sources: file change notification / DB change notification / explicit API)
    pub async fn reload(&self) -> Result<ReloadReport, ConfigError> {
        // 1. Read new config
        let new_raw = self.collect_all_layers().await?;
        let new_effective = self.merge_layers(new_raw)?;
        
        // 2. Validate
        self.validate(&new_effective)?;
        
        // 3. Diff against old config, classify changes
        let diff = self.diff(&self.current.load(), &new_effective);
        
        // 4. Check reloadability of each change
        for change in &diff.changes {
            match change.reloadability() {
                HotReloadability::Immutable => {
                    return Err(ConfigError::AttemptedImmutableChange(change.key.clone()));
                }
                HotReloadability::FullRestart => {
                    return Err(ConfigError::RequiresFullRestart(change.key.clone()));
                }
                _ => {}
            }
        }
        
        // 5. Apply — bucket by reloadability
        let drain_tasks = diff.changes.iter()
            .filter(|c| c.reloadability() == HotReloadability::HotWithDrain)
            .map(|c| self.drain_for(c))
            .collect::<Vec<_>>();
        futures::future::join_all(drain_tasks).await;
        
        // 6. swap
        self.current.store(Arc::new(new_effective.clone()));
        
        // 7. Notify subprocess restart
        for change in &diff.changes {
            if change.reloadability() == HotReloadability::SubprocessRestart {
                self.restart_subprocess_for(change).await?;
            }
        }
        
        // 8. Broadcast
        self.subscribers.send(ConfigChangeEvent { diff }).ok();
        
        Ok(ReloadReport { applied: diff.changes.len(), warnings: vec![] })
    }
}
```

### 6.3 Sources of reload

```toml
[config_manager]
sources = ["file_watcher", "db_polling", "explicit_api"]

[config_manager.file_watcher]
paths = ["/etc/tars/", "/etc/tars/tenants/"]
debounce_ms = 500                         # coalesce file flap window

[config_manager.db_polling]
interval_secs = 30                        # tenant DB change polling
table = "tenant_configs"

[config_manager.explicit_api]
listen = "127.0.0.1:9001"                 # admin API, triggers immediate reload
```

---

## 7. Configuration Validation

### 7.1 Startup-time validation

```rust
pub fn validate_config(config: &Config) -> Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();
    
    // Schema completeness
    errors.extend(validate_schema(config));
    
    // Pipeline layer ordering constraints (Doc 02 §7)
    errors.extend(validate_pipeline_order(&config.pipeline));
    
    // Provider config: auth resolvable / models exist / capabilities consistent
    errors.extend(validate_providers(&config.providers));
    
    // Tenant reference integrity: each tenant.allowed_providers exists in providers
    errors.extend(validate_tenant_references(&config.tenants, config));
    
    // Secret references reachable (do a ping test, but don't actually fetch)
    errors.extend(validate_secret_references(&config));
    
    // Tool / MCP config: binary path exists / scope reference exists
    errors.extend(validate_tools(&config.tools));
    
    // Locked-layer override check (§2.2)
    errors.extend(validate_layer_locks(config));
    
    // PromptBuilder stability (Doc 04 §11)
    errors.extend(validate_prompt_builder_stability(&config.agents));
    
    // Cross-section consistency: model tier referenced by routing policy is reachable in providers
    errors.extend(validate_cross_section(config));
    
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
```

Startup validation **must pass entirely before startup is allowed** — half-startup is an anti-pattern (some features work, others don't, leading to inexplicable runtime errors).

### 7.2 Runtime validation

Validation triggered by hot reload is stricter: in addition to all startup checks, it must check reloadability constraints (§6.2).

### 7.3 Handling validation failures

```rust
pub enum ConfigError {
    /// Fatal: startup fails / reload fails
    Fatal(String),
    
    /// Warning: config is usable but discouraged (e.g. inline secret)
    Warning(String),
    
    /// Known incompatibility: a field deprecated when an old schema is bumped to a new version
    Deprecated { field: String, removed_in_version: ConfigVersion },
}
```

Startup-time Fatal → process exit(1) + full error list written to stderr (not just the first error).
Startup-time Warning → starts normally, all warnings listed in the startup banner.
Deprecated → starts normally, recorded to a migration TODO file (`/var/lib/tars/migration_todo.json`).

---

## 8. Tenant Lifecycle

### 8.1 Provision

```rust
pub struct ProvisionRequest {
    pub display_name: String,
    pub initial_quotas: TenantQuotas,
    pub initial_owners: Vec<Principal>,
    pub allowed_providers: Vec<ProviderId>,
    pub allowed_tools: Vec<ToolId>,
}

pub async fn provision_tenant(req: ProvisionRequest) -> Result<TenantConfig, ProvisionError> {
    // 1. Allocate TenantId
    let tenant_id = TenantId::generate();
    
    // 2. Create isolation resources
    let isolation = TenantIsolation {
        subprocess_home: PathBuf::from(format!("/var/lib/tars/tenants/{}/home", tenant_id)),
        cache_namespace: format!("ns:{}", tenant_id),
        event_log_partition: format!("evt_{}", tenant_id),
        secret_namespace: format!("tenants/{}", tenant_id),
    };
    
    // 3. Physical initialization
    fs::create_dir_all(&isolation.subprocess_home)?;
    db.execute(&format!("CREATE TABLE IF NOT EXISTS {}_events (...)", 
        isolation.event_log_partition)).await?;
    secret_manager.create_namespace(&isolation.secret_namespace).await?;
    
    // 4. Write TenantConfig
    let config = TenantConfig {
        id: tenant_id.clone(),
        display_name: req.display_name,
        status: TenantStatus::Active,
        created_at: SystemTime::now(),
        provisioned_by: current_principal(),
        overrides: Default::default(),
        quotas: req.initial_quotas,
        allowed_providers: req.allowed_providers,
        allowed_tools: req.allowed_tools,
        allowed_skills: vec![],
        allowed_mcp_servers: vec![],
        isolation,
    };
    
    db.insert_tenant_config(&config).await?;
    
    // 5. Trigger ConfigManager reload
    config_manager.reload().await?;
    
    // 6. Audit
    audit_log.write(AuditEvent::TenantProvisioned { 
        tenant: tenant_id, 
        by: current_principal() 
    }).await?;
    
    Ok(config)
}
```

### 8.2 Suspend / Resume

Suspend doesn't delete data, only blocks new requests:

```rust
pub async fn suspend_tenant(tenant: &TenantId, reason: String) -> Result<(), SuspendError> {
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::Suspended { 
        since: SystemTime::now(), 
        reason: reason.clone(),
    };
    db.update_tenant_config(&config).await?;
    
    // 1. Immediately reject new requests for this tenant (Pipeline IAM layer checks status)
    config_manager.reload().await?;
    
    // 2. Gracefully drain in-flight requests (per deadline)
    runtime.drain_tenant(tenant, Duration::from_secs(60)).await;
    
    // 3. Proactively purge L3 cache (avoid continuing to accumulate storage charges)
    cache_janitor.purge_tenant(tenant).await?;
    
    // 4. Kill this tenant's MCP / CLI subprocesses
    subprocess_manager.kill_tenant_processes(tenant).await;
    
    audit_log.write(AuditEvent::TenantSuspended { tenant: tenant.clone(), reason }).await?;
    Ok(())
}

pub async fn resume_tenant(tenant: &TenantId) -> Result<(), ResumeError> {
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::Active;
    db.update_tenant_config(&config).await?;
    config_manager.reload().await?;
    audit_log.write(AuditEvent::TenantResumed { tenant: tenant.clone() }).await?;
    Ok(())
}
```

### 8.3 Delete (GDPR-style)

Delete is irreversible, **two-phase commit**:

```rust
pub async fn schedule_deletion(
    tenant: &TenantId, 
    delay: Duration,
) -> Result<DeletionHandle, DeleteError> {
    // Phase 1: mark PendingDeletion, defer the real delete by N days (default 30)
    // During this window the data still exists, abort_deletion can revert it
    
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::PendingDeletion {
        scheduled_for: SystemTime::now() + delay,
    };
    db.update_tenant_config(&config).await?;
    
    // The tenant enters suspended state (no longer usable)
    suspend_tenant(tenant, "pending_deletion".into()).await?;
    
    // Register a scheduled task to fire at scheduled_for
    scheduler.schedule_at(SystemTime::now() + delay, 
        Box::new(move || actually_delete(tenant.clone()))).await?;
    
    Ok(DeletionHandle { tenant: tenant.clone(), scheduled_for: ... })
}

async fn actually_delete(tenant: TenantId) -> Result<(), DeleteError> {
    // Phase 2: cascade delete
    
    // 1. Abort any trajectories that may still be running
    runtime.abort_tenant(&tenant).await?;
    
    // 2. Delete event log (drop by partition)
    db.execute(&format!("DROP TABLE {}_events", 
        config.isolation.event_log_partition)).await?;
    
    // 3. Delete ContentStore objects (by tenant prefix)
    content_store.purge_tenant(&tenant).await?;
    
    // 4. Delete cache (L2 Redis: prefix scan + delete; L3: list + delete)
    cache_registry.invalidate_tenant(&tenant).await?;
    
    // 5. Delete budget store history
    budget_store.purge_tenant(&tenant).await?;
    
    // 6. Delete subprocess HOME directory
    fs::remove_dir_all(&config.isolation.subprocess_home)?;
    
    // 7. Delete secret namespace
    secret_manager.delete_namespace(&config.isolation.secret_namespace).await?;
    
    // 8. Delete tenant config (last step)
    db.delete_tenant_config(&tenant).await?;
    
    // 9. Write tamper-proof audit record
    audit_log.write(AuditEvent::TenantDeleted { 
        tenant: tenant.clone(),
        deleted_at: SystemTime::now(),
        completed_steps: vec!["events", "content", "cache", "budget", "fs", "secrets", "config"],
    }).await?;
    
    Ok(())
}
```

**Key invariants**:
- If any step between phase 1 and phase 2 fails, the entire deletion is aborted + alerted
- The 9 steps in phase 2 must execute in order; on failure stop there (don't keep deleting blindly)
- Each step must emit an "X objects deleted" metric for audit verification
- audit records are **never deleted**, even after the tenant itself is deleted

---

## 9. Quotas and Billing

### 9.1 Quota model

```rust
pub struct TenantQuotas {
    /// Rate limits (hard caps, exceeding triggers 429)
    pub max_rpm: u32,                      // requests per minute
    pub max_tpm: u64,                      // input+output tokens per minute
    pub max_concurrent_tasks: u32,         // concurrently running trajectories
    pub max_subprocess_count: u32,         // total CLI + MCP subprocess cap
    
    /// Capacity limits
    pub max_l3_storage_bytes: u64,
    pub max_event_log_size_bytes: u64,
    
    /// Cost caps
    pub daily_cost_usd_soft: f64,          // triggers alert
    pub daily_cost_usd_hard: f64,          // triggers circuit break
    pub monthly_cost_usd_hard: f64,
    
    /// Tool/Skill call frequency caps
    pub max_tool_calls_per_day: HashMap<ToolId, u64>,
}
```

### 9.2 Billing data flow

```
Each LLM call / Tool call completes
       │
       ▼
Telemetry (Doc 02 §4.1) extracts usage + computes cost
       │
       ▼
BudgetStore::commit (Redis atomic decrement)
       │
       ▼
Async dual-write:
  ├─→ Billing log (PostgreSQL `billing_events` table) - per-event auditable
  └─→ Aggregation service - real-time aggregation to hour/day/month dimensions
       │
       ▼
Triggers:
  - exceeds soft threshold → alert (Slack / email)
  - exceeds hard threshold → circuit break (BudgetMiddleware rejects)
  - month-end close → export CSV/JSON to billing system
```

### 9.3 Billing report export

```rust
#[async_trait]
pub trait BillingExporter: Send + Sync {
    async fn export(
        &self,
        period: BillingPeriod,
        format: ExportFormat,
    ) -> Result<ExportArtifact, BillingError>;
}

pub struct BillingPeriod {
    pub start: SystemTime,
    pub end: SystemTime,
    pub tenant_filter: Option<TenantId>,
}

pub enum ExportFormat {
    Csv,
    Json,
    StripeWebhook,                         // push directly to Stripe metered billing
    InternalKafka { topic: String },
}
```

Report contents:
- Aggregate token / cost / call count by tenant
- Breakdown by model / tool
- Time series by day
- Separately list cache savings (Doc 03 §12 `cache.l3.cost_saved_usd`)

---

## 10. Audit and Compliance

### 10.1 Tamper-proof audit log

```rust
pub enum AuditEvent {
    // Tenant lifecycle
    TenantProvisioned { tenant: TenantId, by: Principal },
    TenantSuspended { tenant: TenantId, reason: String },
    TenantResumed { tenant: TenantId },
    TenantDeleted { tenant: TenantId, deleted_at: SystemTime, completed_steps: Vec<String> },
    
    // Configuration changes
    ConfigReloaded { changes: Vec<ConfigChange>, by: Principal },
    ConfigReloadRejected { reason: String, by: Principal },
    
    // Security events
    IamDenied { principal: Principal, resource: ResourceRef, reason: String },
    SecurityAlert { kind: String, details: serde_json::Value },
    CompensationFailed { trajectory: TrajectoryId, compensation: CompensationId, error: String },
    
    // Data access
    SecretAccessed { ref: SecretRef, by: Principal },
    
    // Billing events
    BudgetSoftLimitExceeded { tenant: TenantId, period: String, amount: f64 },
    BudgetHardLimitExceeded { tenant: TenantId, period: String, amount: f64 },
}

#[async_trait]
pub trait AuditLog: Send + Sync {
    async fn write(&self, event: AuditEvent) -> Result<AuditRef, AuditError>;
}
```

**Implementation requirements**:
- Write to append-only storage (Postgres + immutable column / WORM S3 / blockchain)
- Sign every event (HMAC with rotated key)
- Asynchronous dual-write to an external SIEM (Splunk / Datadog / ELK)
- Even after the tenant is deleted, audit records are retained for 7 years (compliance requirement)

### 10.2 GDPR compliance

- **Right to portability**: `export_tenant_data` API exports all that tenant's events / cache keys (excluding LLM response content, since it is derivative) / billing
- **Right to be forgotten**: §8.3's 30-day delay + cascade delete
- **Data localization**: Provider config can specify region; tenant config selects providers in the corresponding region (e.g. EU tenants can only use Anthropic / Gemini in EU regions)

```toml
[providers.claude_eu]
type = "anthropic"
base_url = "https://api.anthropic.com"     # Anthropic has no explicit EU endpoint, but routing via VPC works
region = "eu-west-1"
data_residency = "EU"

[tenants.eu_customer_acme]
allowed_providers = ["claude_eu", "gemini_eu"]
data_residency_required = "EU"             # enforces only providers tagged EU may be used
```

---

## 11. Configuration Shape Summary

The full schema spans all preceding docs; here is a minimal working example:

```toml
# config.toml
version = "1.0"

# === Doc 01 ===
[providers.claude_api]
type = "anthropic"
auth = { source = "vault", path = "secret/data/tenants/${tenant_id}/anthropic" }
default_model = "claude-opus-4-7"

[providers.local_qwen]
type = "openai_compat"
base_url = "http://ryzen-node-1:8000/v1"
auth = { source = "none" }

# === Doc 02 ===
[pipeline]
order = ["telemetry", "auth", "iam", "budget", "cache_lookup", 
         "prompt_guard", "schema_validation", "routing", 
         "circuit_breaker", "retry"]

[pipeline.constraints]
"iam" = { must_be_before = ["cache_lookup"] }
"telemetry" = { must_be_outermost = true }

[middleware.budget]
backend = "redis"
default_tpm = 100000
default_daily_cost_usd = 50

# === Doc 03 ===
[cache]
hasher_version = 1                         # locked item

[cache.l1]
max_capacity = 10000

[cache.l2]
url = { source = "env", var = "REDIS_URL" }

[cache.l3]
storage_quota_bytes = 10737418240

# === Doc 04 ===
[agents]
prompt_builder_stability_check = true

[[agents.blueprints]]
id = "code_reviewer"
orchestrator_tier = "default"
worker_tier = "reasoning"
critic_tier = "default"

# === Doc 05 ===
[mcp_servers.filesystem]
type = "stdio"
binary = "/usr/local/bin/mcp-filesystem"
mode = "long_lived"
auth = { source = "delegate", per_tenant_home = true }

# === Doc 06 (this doc) ===
[secrets]
default_resolver = "vault"

[secrets.vault]
url = { source = "env", var = "VAULT_ADDR" }
token = { source = "env", var = "VAULT_TOKEN" }

[observability]
otel_endpoint = { source = "env", var = "OTEL_EXPORTER_OTLP_ENDPOINT" }
audit_log_backend = "postgres"
audit_log_replication = ["splunk"]

[deployment]
node_id = { source = "env", var = "NODE_ID" }
discovery = "static"                       # vs k8s / consul
peers = ["node-1:7000", "node-2:7000"]

# === Tenants ===
[tenants.acme_corp]
display_name = "ACME Corporation"
status = "active"
allowed_providers = ["claude_api", "local_qwen"]
allowed_tools = ["fs.read_file", "git.fetch_pr_diff"]
allowed_mcp_servers = ["filesystem"]

[tenants.acme_corp.quotas]
max_tpm = 500000
daily_cost_usd_hard = 500

[tenants.acme_corp.overrides.cache]
l3_storage_quota_bytes = 53687091200       # 50 GB

[tenants.acme_corp.isolation]
subprocess_home = "/var/lib/tars/tenants/acme_corp/home"
cache_namespace = "ns:acme_corp"
event_log_partition = "evt_acme_corp"
secret_namespace = "tenants/acme_corp"
```

---

## 12. Testing Strategy

### 12.1 Configuration schema tests

```rust
#[test]
fn schema_round_trip() {
    let toml_str = include_str!("../examples/full_config.toml");
    let parsed: Config = toml::from_str(toml_str).unwrap();
    let re_serialized = toml::to_string(&parsed).unwrap();
    let re_parsed: Config = toml::from_str(&re_serialized).unwrap();
    assert_eq!(parsed, re_parsed);
}

#[test]
fn pipeline_constraint_violation_rejected() {
    let mut config = test_config();
    config.pipeline.order = vec!["cache_lookup".into(), "iam".into()];  // IAM after cache!
    
    let errors = validate_config(&config).unwrap_err();
    assert!(errors.iter().any(|e| matches!(e, ConfigError::Fatal(s) if s.contains("iam"))));
}

#[test]
fn locked_key_override_rejected() {
    let mut config = test_config();
    config.tenants.insert("evil".into(), TenantConfig {
        overrides: TenantOverrides {
            cache: Some(CacheOverrides {
                hasher_version: Some(99),    // attempt to override a locked item
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    });
    
    let errors = validate_config(&config).unwrap_err();
    assert!(errors.iter().any(|e| matches!(e, ConfigError::Fatal(s) 
        if s.contains("locked"))));
}
```

### 12.2 Tenant isolation end-to-end tests

```rust
#[tokio::test]
async fn tenant_a_cache_does_not_leak_to_tenant_b() {
    let runtime = test_runtime_with_two_tenants("a", "b").await;
    
    // Tenant A makes a call with the same prompt, populating cache
    let req = test_request_with_tenant("a");
    runtime.execute(req.clone()).await.unwrap();
    
    // Tenant B makes a call with the same prompt
    let req_b = ChatRequest { tenant_id: "b".into(), ..req };
    let stats_before = mock_provider.invocation_count();
    runtime.execute(req_b).await.unwrap();
    let stats_after = mock_provider.invocation_count();
    
    // B must actually hit the provider (cannot hit A's cache)
    assert_eq!(stats_after - stats_before, 1);
}

#[tokio::test]
async fn deleted_tenant_data_completely_purged() {
    let runtime = test_runtime();
    let tenant = provision_test_tenant(&runtime).await;
    
    // Create some data
    create_trajectories(&runtime, &tenant, 10).await;
    create_cache_entries(&runtime, &tenant, 50).await;
    
    schedule_deletion(&tenant, Duration::ZERO).await.unwrap();
    actually_delete(&tenant).await.unwrap();
    
    // Verify all resources are gone
    assert!(db.tenant_exists(&tenant).await.unwrap() == false);
    assert!(content_store.tenant_object_count(&tenant).await.unwrap() == 0);
    assert!(cache_registry.tenant_key_count(&tenant).await.unwrap() == 0);
    assert!(!fs::exists(&format!("/var/lib/tars/tenants/{}", tenant)));
    
    // But the audit log must remain
    let audit_records = audit_log.query_for_tenant(&tenant).await.unwrap();
    assert!(audit_records.iter().any(|r| matches!(r.event, AuditEvent::TenantDeleted { .. })));
}
```

### 12.3 Hot-reload tests

```rust
#[tokio::test]
async fn budget_change_hot_reloads() {
    let manager = test_config_manager();
    
    // Initial budget 100
    assert_eq!(manager.current().middleware.budget.default_daily_cost_usd, 100.0);
    
    // Modify the config file
    update_config_file(|c| { c.middleware.budget.default_daily_cost_usd = 200.0 });
    manager.reload().await.unwrap();
    
    // Takes effect immediately
    assert_eq!(manager.current().middleware.budget.default_daily_cost_usd, 200.0);
}

#[tokio::test]
async fn immutable_change_rejected() {
    let manager = test_config_manager();
    
    update_config_file(|c| { c.cache.hasher_version = 999 });
    let result = manager.reload().await;
    
    assert!(matches!(result, Err(ConfigError::AttemptedImmutableChange(s)) 
        if s.contains("hasher_version")));
}
```

### 12.4 Secret tests

```rust
#[tokio::test]
async fn secret_template_resolves_per_tenant() {
    let resolver = test_secret_resolver();
    
    let refr = SecretRef {
        source: SecretSource::Vault,
        identifier: "secret/data/tenants/${tenant_id}/openai/api_key".into(),
        ..Default::default()
    };
    
    let key_a = resolver.resolve(&refr, &ctx_for_tenant("a")).await.unwrap();
    let key_b = resolver.resolve(&refr, &ctx_for_tenant("b")).await.unwrap();
    
    // Different tenants get different secrets
    assert_ne!(key_a, key_b);
}

#[tokio::test]
async fn cross_tenant_secret_access_rejected() {
    // Tenant A's code attempts to read tenant B's secret
    let resolver = test_secret_resolver();
    let refr_b = SecretRef {
        source: SecretSource::Vault,
        identifier: "secret/data/tenants/b/openai/api_key".into(),
        ..Default::default()
    };
    
    let result = resolver.resolve(&refr_b, &ctx_for_tenant("a")).await;
    assert!(matches!(result, Err(SecretError::AccessDenied)));
}
```

---

## 13. Anti-pattern Checklist

1. **Don't write plaintext secrets in config files** — always use SecretRef.
2. **Don't let tenant config override locked security constraints** — cache hasher_version, pipeline ordering, tool side_effect classification, etc.
3. **Don't silently accept partially loaded config** — validation failure = startup failure / reload failure, no half-broken running.
4. **Don't assume all config is hot-reloadable** — annotate reloadability explicitly on the schema.
5. **Don't skip isolation resource initialization on Tenant Provision** — if subprocess_home doesn't exist, the first MCP call crashes.
6. **Don't miss any data store on Tenant Delete** — cascade delete must cover events / cache / content / budget / fs / secret — all 7 categories.
7. **Don't put audit logs in the same store as business data** — must have an independent write path so audit can still write when the business DB is down.
8. **Don't let Tenant Suspend immediately affect in-flight requests** — Drain window defaults to 60s, allow already-started work to complete.
9. **Don't persist values in the secret cache** — memory only, lost on process restart.
10. **Don't let the Configuration type hold mutable global state** — atomic swap via ArcSwap, lock-free read path.
11. **Don't delay notification when quotas trigger circuit-break** — alerts must be real-time; finding out after the fact means the money is already lost.
12. **Don't allow deletion events to be overwritten or edited** — audit log is append-only, even admins can't modify.
13. **Don't mix system / tenant config** — tenant overrides are the explicit `overrides: TenantOverrides`, not direct mutation of the global section.
14. **Don't let tenant_id strings be freely generated** — must go through `TenantId::generate()`; user-supplied IDs are forbidden (collision-prone / injection attacks).
15. **Don't silently ignore deprecated fields** — record them in migration_todo, surface to ops.

---

## 14. Contracts with Upstream and Downstream

### Upstream (Application / Frontend Adapter) commitments

- What you get from `ConfigManager::current()` is an already merged / validated EffectiveConfig
- All tenant switching is conveyed through RequestContext.tenant_id, never via global variables
- Don't directly access a tenant's isolation paths (subprocess_home / cache_namespace); go through the corresponding trait interfaces (SubprocessManager / CacheRegistry)

### Downstream contracts (each Doc 01-05 component)

- Receive EffectiveConfig as a constructor argument; don't read files yourself
- Implement the `ConfigSubscriber` trait to listen to changes:
  ```rust
  #[async_trait]
  pub trait ConfigSubscriber: Send + Sync {
      fn interested_in(&self) -> Vec<ConfigKeyPattern>;
      async fn on_change(&self, change: &ConfigChange) -> Result<(), SubscriberError>;
  }
  ```
- Don't cache resolved secrets longer than SecretRef.cache_ttl
- On Tenant Suspend / Delete, clean up owned resources (subprocess kill / cache purge / etc.)

### Cross-node contracts

In multi-node deployments:
- Each node has an independent `ConfigManager`, synchronized via file watcher / DB polling
- Config changes don't require simultaneous activation across all nodes, but must be eventually consistent
- Nodes don't communicate config changes directly — the filesystem / DB is the intermediary
- audit_log must be replicated to centralized storage (local + Splunk); a node going down loses nothing

---

## 15. TODOs and Open Questions

- [ ] Which DSL for the configuration schema: TOML + serde / Cue / Pkl / Dhall
- [ ] Multi-region deployment config sync strategy (push vs pull / DB region routing)
- [ ] Per-tenant encryption (encryption at rest with tenant-specific keys, for financial compliance)
- [ ] How to technically guarantee audit log immutability (HMAC vs WORM vs blockchain)
- [ ] Tenant ID naming rules and readability (UUID vs short_id vs human-readable)
- [ ] Schema and API for the per-tenant quota visualization dashboard
- [ ] Configuration migration tooling (automatic v1 → v2 conversion + dry-run)
- [ ] Automation of Secret rotation (calendar-based vs event-driven)
- [ ] Enforcement of multi-region tenant data residency (startup-time vs runtime validation)
- [ ] Whether Workspace needs an independent quota / IAM sub-layer (this doc currently says no — tenant is sufficient)
