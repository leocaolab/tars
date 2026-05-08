# Document 14 — Implementation Path / Migration Strategy

> **Note**: This roadmap reflects priorities at the time of writing.
> Actual milestone order may shift as we learn from production usage
> and feedback. M8-M14 sections describe the design ahead — see
> [CHANGELOG.md](../../CHANGELOG.md) for what's actually shipped, and
> [TODO.md](../../TODO.md) §"Roadmap status" for the current
> per-milestone state. Plan, not commitment.

> Scope: concrete implementation plan from current state (design complete, code minimal) to a releasable v1.0 — milestones, dependencies, risks, decision points, resource estimates.
>
> Context: this document expands Doc 00 §8.2, breaking the 9 milestones into executable deliverables and acceptance criteria.
>
> **Status**: this document is updated continuously as implementation proceeds; completed milestones are marked ✅ and retained for reference.

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Verifiable milestones** | Each milestone has a clear Definition of Done, acceptable to external teams |
| **Small steps, fast cadence** | Each milestone completes in 2-6 weeks; avoid 6-month closed-door builds |
| **Vertical slice first** | End-to-end (Provider → Pipeline → Runtime) runs early, even if every layer is rudimentary |
| **Risk up front** | High-risk / high-uncertainty parts (FFI / subprocess management / streaming) done early, not deferred |
| **Interruptible and resumable** | After each milestone the system is in a working state; long vacations may pause work |
| **Design before implementation** | If implementation finds design issues → fix the doc before fixing code; "code-doc divergence" is not allowed |
| **Test-coverage driven** | Code without tests is not done, even if the demo runs |

**Anti-goals**:
- Not pursuing "perfect architecture in one shot" — refactors are acceptable before v1.0; only after v1.0 does API stabilize
- Not pursuing "feature-complete" — v1.0 covers core scenarios only; long-tail features go to v2
- Not pursuing all-Provider support on day one — OpenAI compat first, others incrementally
- Not skipping tests for milestone progress — tech debt is scarier than going slower

---

## 2. Start and End

### 2.1 Current State (Day 0)

```
✅ Done:
- 14 design documents (Doc 00-13)
- Project directory created (tars/)
- Git repo initialized

⚠️  Exists but needs handling:
- interview_app/ — Python prototype
- interview_app_en/ — English version of prototype
- ube_core/ — early core code (language?)
- ube_project/ — early project abstraction
- example.py — demo script
- blackboard_5rounds.json — blackboard mode data sample
- selfplay_5rounds.txt — self-play log
- report_5rounds.md — experiment report
- README.md — project description (to be updated)

❌ Not started:
- Cargo workspace
- Any Rust crate
- CI/CD pipeline
- Test infrastructure
- Deployment scripts
```

### 2.2 v1.0 Target State (~6-9 months later)

```
✅ Must have:
- Personal mode end-to-end working (single user / SQLite / local web dashboard / OpenAI+Anthropic+Gemini API)
- Team mode end-to-end working (multi-tenant / Postgres / Redis / OIDC / IAM)
- All 14 core crates implemented
- HTTP API + Python (PyO3) + TypeScript (napi-rs) FFI
- CLI / TUI / Web Dashboard — three Frontends
- Full MELT observability
- Full audit + billing
- 1 reference MCP server integration usable (filesystem)
- At least 3 built-in skills (code-review / doc-summary / security-audit)
- Full test suite (unit + integration + conformance)
- Docs consistent with code

Optional (not required for v1.0 but recommended):
- WASM client subset
- Hybrid deployment mode
- gRPC server
- More than 3 MCP servers
- Advanced Skills (declarative YAML / SubAgent)

Explicitly not in v1.0:
- SaaS multi-region production deployment (requires dedicated infra)
- Full enterprise SSO / SAML support (basic OIDC suffices)
- AI-assisted incident analysis
- Real-time collaborative dashboard
```

---

## 3. Strategic Choice: Disposition of Old Code

`interview_app` / `ube_core` / `ube_project` are early exploration, divergent from the new design. Three disposition options:

### 3.1 Option A: Greenfield (start over)

**Approach**: archive old code to `archive/legacy-prototype/`, start new code from scratch.

**Pros**:
- Not constrained by old design
- Strong Rust consistency
- Clear learning cost (learn new architecture from docs)

**Cons**:
- Loses experimental data / business understanding from old code
- Users perceive "rewrite without delivery"
- May repeat old mistakes

### 3.2 Option B: In-place Migration

**Approach**: keep `ube_core` / `ube_project` interfaces, gradually replace internals with Rust impls.

**Pros**:
- Business continuity
- Incremental delivery; every milestone has visible output

**Cons**:
- Python ↔ Rust boundary is complex
- Old design may constrain new design
- Limited room for performance optimization (FFI overhead)

### 3.3 Option C: Hybrid (recommended)

**Approach**:
- **New Rust code** in `crates/`, strictly per docs
- **Old Python code** retained in `interview_app/` etc., serving as:
  - Business reference (understanding requirements)
  - Data samples (`blackboard_5rounds.json` etc. as test fixtures)
  - Reference Skill (`interview` as the new architecture's first real skill implementation)
- **Bridge**: during v0.x, the CLI layer may invoke old Python (subprocess); by v1.0 the old code has been replaced by new Skills and can be archived

**Recommend C** — neither wasting prior exploration nor being constrained by it.

```
project root:
├── crates/                     ← new Rust code (implemented per docs)
│   ├── tars-types/
│   ├── tars-runtime/
│   ├── tars-provider/
│   └── ...
├── archive/legacy-prototype/    ← old Python (archived after M3)
│   ├── interview_app/
│   ├── interview_app_en/
│   ├── ube_core/
│   └── ube_project/
├── examples/                    ← migrated old example.py (M5)
├── fixtures/                    ← test data (migrated old json files)
│   └── blackboard-5rounds.json
└── docs/                        ← 14 design documents
```

---

## 4. Crate Workspace Structure

Per Doc 12 §3.1's split, `Cargo.toml` workspace:

```toml
# Cargo.toml (root)
[workspace]
resolver = "2"
members = [
    "crates/tars-types",         # M0
    "crates/tars-config",        # M0
    "crates/tars-storage",       # M0
    "crates/tars-melt",          # M0 (basics; full at M5)
    "crates/tars-security",      # M1 (basics), M6 (full)
    "crates/tars-provider",      # M1
    "crates/tars-cache",         # M1 (L1+L2), M3 (L3)
    "crates/tars-pipeline",      # M2
    "crates/tars-runtime",       # M3
    "crates/tars-tools",         # M4
    "crates/tars-frontend-cli",  # M5
    "crates/tars-frontend-tui",  # M5
    "crates/tars-frontend-web",  # M7
    "crates/tars-server",        # M6 (HTTP + later gRPC)
    "crates/tars-py",            # M8 (PyO3 binding)
    "crates/tars-node",          # M8 (napi-rs binding)
    "crates/tars-cli",           # M5 (main CLI binary)
    "crates/tars-server-bin",    # M6 (server binary)
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "Apache-2.0"
authors = ["TARS Team"]

[workspace.dependencies]
# Shared dependency versions pinned at workspace level; each crate references workspace = true

# async runtime
tokio = { version = "1.40", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }
async-trait = "0.1"
futures = "0.3"

# serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# errors
thiserror = "1"
anyhow = "1"

# logging/tracing
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }

# HTTP
reqwest = { version = "0.12", features = ["json", "stream"] }
axum = "0.7"
tower = "0.5"

# database
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres", "sqlite", "macros", "migrate"] }

# crypto / security
ring = "0.17"
argon2 = "0.5"
subtle = "2"

# observability
opentelemetry = "0.27"
opentelemetry-otlp = "0.27"
tracing-opentelemetry = "0.28"

# testing
proptest = "1"
criterion = "0.5"
mockall = "0.13"
```

### 4.1 Responsibility Boundaries Across Crates

| Crate | Dependencies (in-project) | Key third-party deps |
|---|---|---|
| `tars-types` | (none) | serde / thiserror |
| `tars-config` | tars-types | serde / toml / config |
| `tars-storage` | tars-types | sqlx / r2d2 (sqlite pool) |
| `tars-melt` | tars-types | tracing / opentelemetry |
| `tars-security` | tars-types, tars-config | ring / argon2 / jsonwebtoken |
| `tars-provider` | tars-types, tars-melt, tars-security | reqwest / async-trait |
| `tars-cache` | tars-types, tars-storage, tars-security | moka / redis |
| `tars-pipeline` | tars-types, tars-provider, tars-cache, tars-security, tars-melt | tower |
| `tars-runtime` | tars-types, tars-pipeline, tars-storage, tars-melt | (no new) |
| `tars-tools` | tars-types, tars-runtime, tars-security | tokio (process) / serde_json |
| `tars-server` | tars-runtime, tars-melt | axum / tonic (later) |
| `tars-frontend-cli` | tars-runtime | clap |
| `tars-frontend-tui` | tars-runtime | ratatui / crossterm |
| `tars-frontend-web` | tars-server | rust-embed |
| `tars-py` | tars-runtime | pyo3 / pyo3-asyncio |
| `tars-node` | tars-runtime | napi / napi-derive |

---

## 5. Key Dependency Choices

"Build vs buy" decisions to settle before implementation. Each lists options; **bold** is the recommendation.

### 5.1 Async runtime
- **tokio** ✅ — ecosystem standard, no debate
- async-std — not recommended, ecosystem shrinking

### 5.2 HTTP client
- **reqwest** ✅ — mainstream, mature reqwest::Client
- hyper directly — too low-level
- ureq — synchronous, unsuitable

### 5.3 HTTP server
- **axum** ✅ — Tower ecosystem, fits Pipeline design
- actix-web — excellent perf but trait design less clean than axum
- warp — low maintenance activity

### 5.4 Database ORM/Query
- **sqlx** ✅ — compile-time SQL checking + multi-DB + async
- diesel — synchronous (async extension exists but not mature)
- sea-orm — higher abstraction but complex

### 5.5 Postgres connection pool
- **sqlx built-in** ✅ (PgPool)
- deadpool-postgres — complex to use standalone

### 5.6 SQLite
- **rusqlite + r2d2_sqlite** for low-level — mature, precise WAL config
- sqlx + sqlite feature — shares query syntax with Postgres, recommended ✅

### 5.7 Redis
- **redis-rs (with tokio)** ✅ — mainstream
- fred — excellent perf but heavy API

### 5.8 LLM SDK
- **Write our own HTTP client** ✅ (based on reqwest) — 100% control + consistent with docs
- async-openai — exists, but the gap with our abstraction is large; routing through it is more trouble than it's worth

### 5.9 Tokenizer
- **tiktoken-rs** for OpenAI models ✅
- **tokenizers (HF Rust binding)** for Anthropic / Gemini / local ✅
- Others not mainstream

### 5.10 ONNX Runtime
- **ort** ✅ — mainstream Rust binding
- candle — pure Rust but maturity low

### 5.11 Embedded LLM inference
- **mistral.rs** ✅ — Rust-native, active
- candle — same as above; mistral.rs is more mature
- llama-cpp-2 — FFI to llama.cpp, not pure Rust but broadly compatible

### 5.12 TUI
- **ratatui** ✅ — best currently
- cursive — old guard but immediate-mode is inferior

### 5.13 Python FFI
- **pyo3 + pyo3-asyncio** ✅ — recommended
- UniFFI (Mozilla) — **not suitable for this project**, because:
  - No TypeScript/Node support (we have a separate napi-rs need; UniFFI doesn't solve it)
  - Weak streaming expressiveness (callback-based; Python-side `async for` UX is poor)
  - Serialization overhead accumulates significantly in streaming scenarios
  - Complex enums (data-bearing AgentMessage) are verbose in UDL
  - True sweet spot is iOS/Android multi-language scenarios; v1.0 not in scope
  - **If a mobile SDK is added later, UniFFI can coexist with PyO3**, each serving different languages
- rustpython etc. — not suitable for this scenario

### 5.14 Node.js FFI
- **napi-rs** ✅ — Node.js mainstream
- neon — under-maintained
- UniFFI — unsupported

### 5.15 Configuration
- **figment** ✅ — friendly to multi-layer merge
- config-rs — also fine
- Roll our own — unnecessary

### 5.16 Secret Manager clients
- vault: **vaultrs** ✅
- AWS Secrets Manager: **aws-sdk-secretsmanager**
- GCP Secret Manager: **google-cloud-secretmanager**
- Azure: **azure-sdk-rust**
- Wrapped via the unified trait `SecretResolver` (Doc 06 §5)

### 5.17 OpenTelemetry
- **opentelemetry-rust + opentelemetry-otlp** ✅ — official
- No alternative

### 5.18 Cache (L1 in-process)
- **moka** ✅ — concurrent + size-based eviction + TTL
- Roll our own LRU — unnecessary

### 5.19 Schema (JSON Schema handling)
- **schemars** for derive Schema from Rust types ✅
- **jsonschema** for runtime validation ✅
- canonical JSON: roll our own or **cjson-rs**

### 5.20 Testing
- **proptest** ✅ for property-based
- **mockall** ✅ for mocking traits
- **criterion** ✅ for benchmarks
- **wiremock** for HTTP mocking ✅

---

## 6. M0 — Foundation (3-4 weeks)

**Goal**: stand up the skeleton, lock in infrastructure, ensure subsequent milestones have a clean starting point.

### 6.1 Deliverables

```
crates/tars-types/
  ├─ TenantId / SessionId / TaskId / TraceId etc. — strongly typed IDs
  ├─ Principal / Scope / ResourceRef
  ├─ TaskSpec / TaskHandle / TaskResult / TaskBudget
  ├─ ChatRequest / ChatResponse / ChatEvent / Message / ContentBlock
  ├─ ToolDescriptor / ToolOutput / ToolEvent
  ├─ AgentEvent / TrajectoryEvent (enum only; storage not implemented)
  ├─ ProviderError / ToolError / etc. all error types
  └─ #[derive(Schema)] — can generate JSON Schema (for Doc 12 automation)

crates/tars-config/
  ├─ Config / TenantConfig / ProvidersConfig and other top-level schemas
  ├─ figment-based 5-layer loading (built-in / system / user / tenant / request)
  ├─ HotReloadability annotation handling
  ├─ validate_config() startup validation
  └─ ConfigManager (basics; no hot reload yet)

crates/tars-storage/
  ├─ EventStore trait
  ├─ ContentStore trait
  ├─ KVStore trait (for cache / idempotency)
  ├─ SqliteEventStore impl + migrations
  ├─ FsContentStore impl
  └─ MemoryKVStore impl (temporary; upgraded to SQLite at M1)

crates/tars-melt/
  ├─ TelemetryGuard (start + shutdown)
  ├─ tracing_subscriber JSON formatter
  ├─ SecretField<T> type (forced redaction)
  ├─ basic metrics registration helper
  └─ OpenTelemetry SDK config (basic; full at M5)

repo-level:
  ├─ Cargo.toml workspace complete
  ├─ rustfmt.toml / clippy.toml
  ├─ .github/workflows/ci.yml (cargo test + clippy + fmt)
  ├─ .github/workflows/security.yml (cargo-audit + cargo-deny)
  ├─ Makefile or justfile (common commands)
  ├─ Docker base image (for M6)
  └─ README.md updated (project structure + how to develop)
```

### 6.2 Definition of Done (M0)

- [ ] `cargo test --workspace` all green
- [ ] `cargo clippy --workspace -- -D warnings` no warnings
- [ ] `cargo deny check` passes
- [ ] CI runs automatically on PRs
- [ ] Can write a minimal Rust program: load config → write an event to SQLite → read it back
- [ ] tracing emits JSON
- [ ] Old prototype code deleted/archived to `archive/`
- [ ] README describes how to set up dev env

### 6.3 Risks

- Type design churn → **mitigation**: follow docs strictly; allow adjustment within v0.x
- sqlx compile-time checks need a real DB → **mitigation**: CI starts SQLite + Postgres test container

---

## 7. M1 — Single Provider, Single Path (4-6 weeks)

**Goal**: a simplified end-to-end LLM call path runs, validating the core abstractions.

### 7.1 Deliverables

```
crates/tars-provider/
  ├─ LlmProvider trait (Doc 01 §3)
  ├─ ChatRequest / ChatEvent normalize (reuses tars-types)
  ├─ HttpProviderBase (shared reqwest logic + retry + timeout)
  ├─ OpenAiProvider impl (HTTP only, strict mode + tool use + streaming)
  ├─ Capabilities descriptors
  ├─ Auth resolver (basic: env var + inline)
  └─ Mock provider for testing (replay fixtures)

crates/tars-cache/
  ├─ CacheRegistry trait
  ├─ CacheKey + CacheKeyFactory (Doc 03 §3.2, complete with IAM scopes placeholder)
  ├─ L1: moka in-memory cache
  ├─ L2: SQLite-backed cache (Personal mode)
  ├─ Singleflight (Doc 03 §6, simplified)
  └─ L3 not implemented yet (added at M3)

crates/tars-pipeline/
  ├─ LlmService trait
  ├─ Middleware trait
  ├─ Pipeline builder
  ├─ TelemetryMiddleware (basic)
  ├─ CacheLookupMiddleware
  ├─ RetryMiddleware (basic)
  └─ IAM / Budget / Guard not implemented yet (M2-M6)

crates/tars-cli (basic):
  ├─ `tars run --prompt "hello"` exercises the full path
  ├─ Streams text to stdout
  └─ Displays token usage + cost (estimated)
```

### 7.2 End-to-end validation script

```bash
# setup
export OPENAI_API_KEY=sk-...
mkdir -p ~/.config/tars
cat > ~/.config/tars/config.toml <<EOF
mode = "personal"

[providers.openai]
type = "openai"
auth = { source = "env", var = "OPENAI_API_KEY" }
default_model = "gpt-4o-mini"

[storage]
backend = "sqlite"
path = "~/.local/share/tars/test.db"

[cache]
hasher_version = 1

[pipeline]
order = ["telemetry", "cache_lookup", "retry"]
EOF

# run
tars run --prompt "Write a haiku about Rust"

# expected output:
# - haiku streamed out
# - trailing line: tokens: 87, cost: $0.0001

# second run with the same prompt
tars run --prompt "Write a haiku about Rust"
# expected:
# - returns immediately (cache hit)
# - trailing line: cached, cost saved: $0.0001
```

### 7.3 Definition of Done (M1)

- [ ] The end-to-end script above runs
- [ ] On cache hit, OpenAI is not called
- [ ] Cancel (Ctrl+C) stops cleanly
- [ ] Errors (invalid API key / no network) produce clear messages
- [ ] Unit test coverage > 70%
- [ ] At least one conformance test passes (provider mock replay)

### 7.4 Risks

- Streaming BoxStream lifetime issues → **mitigation**: Doc 01 §3.1 already designed the Arc<Self> pattern; follow strictly
- OpenAI API changes → **mitigation**: pin the API version, subscribe to announcements
- SQLite WAL config → **mitigation**: Doc 09 §4.2 provides specific PRAGMAs

---

## 8. M2 — Multi-Provider + Routing (3-4 weeks)

**Goal**: add Anthropic / Gemini, full routing/fallback, production-grade error handling.

### 8.1 Deliverables

```
crates/tars-provider/
  + AnthropicProvider impl (HTTP + explicit cache_control)
  + GeminiProvider impl (HTTP + responseSchema)
  + LocalOpenAiCompatProvider (vLLM / llama.cpp server)
  + Tool call format normalization (three vendors → unified ToolCall struct)
  + StructuredOutput format normalization
  + Full error classification (Permanent / Retriable / MaybeRetriable)

crates/tars-pipeline/
  + RoutingMiddleware
  + RoutingPolicy trait + 4 internal impls (Explicit/Tier/Cost/Latency)
  + FallbackChain
  + CircuitBreakerMiddleware (basic: failure_rate based)
```

### 8.2 Validation

- [ ] Same ChatRequest behaves consistently across providers (conformance test)
- [ ] Tier-based routing works
- [ ] Fallback works when the primary provider fails
- [ ] Circuit breaker opens at high failure rate

---

## 9. M3 — Agent Runtime Core (6-8 weeks)

**Goal**: full Trajectory + event sourcing + single-worker mode.

### 9.1 Deliverables

```
crates/tars-runtime/
  ├─ Runtime trait (Doc 04 §12)
  ├─ Trajectory + AgentEvent + ContentRef (full data model)
  ├─ Agent trait + AgentMessage protocol
  ├─ Default OrchestratorAgent (calls LLM to emit a task DAG)
  ├─ Default WorkerAgent (executes tasks)
  ├─ Default CriticAgent
  ├─ Trajectory store (built on EventStore)
  ├─ ContextStore + ContextCompactor (basic: schema-aware filter)
  ├─ PromptBuilder trait + default impl
  ├─ Recovery: replay-from-checkpoint
  └─ Backtrack: basic (no side-effect compensation; added at M4)

crates/tars-cache/
  + L3 Provider explicit cache (Anthropic + Gemini)
  + L3 reference counting + Janitor

crates/tars-pipeline/
  + BudgetMiddleware (Token bucket via tars-storage MemoryKVStore for now)
  + PromptGuardMiddleware (fast lane: aho-corasick only; slow lane at M4)
```

### 9.2 Validation

- [ ] Run a multi-step task: orchestrator + 2 workers + critic
- [ ] After interrupt + restart, the task can recover (event replay)
- [ ] Intentionally let a worker emit schema-invalid output; critic rejects + replans
- [ ] L3 cache reference counting is correct (acquired multiple times; deletion only after grace period post-release)

---

## 10. M4 — Tools + MCP + Guard ML (4-6 weeks)

**Goal**: Tool ecosystem usable, Skill framework ready, full security Guard.

### 10.1 Deliverables

```
crates/tars-tools/
  ├─ Tool trait + ToolRegistry
  ├─ Tool-call mini-pipeline (IAM/Idempotency/SideEffect/Budget/Audit/Timeout)
  ├─ Built-in tools: fs.read_file / fs.write_file / git.fetch_pr_diff
  ├─ MCP Provider (stdio transport + long-lived session pool)
  ├─ Reference MCP server integration: mcp-filesystem
  ├─ Skill trait + Native Skill executor
  ├─ Built-in Skill: code-review (orchestrator + 2 workers + critic)
  └─ SideEffectKind enforcement (Irreversible only in commit phase)

crates/tars-pipeline/
  + PromptGuardMiddleware slow lane: ONNX classifier (DeBERTa)
  + ClassifierProvider trait + OnnxClassifier impl

crates/tars-runtime/
  + Saga compensation (Doc 04 §6)
  + Backtrack with compensations
```

### 10.2 Validation

- [ ] `tars run --skill code-review --repo . --pr 42` works end to end
- [ ] MCP filesystem server starts + invocation succeeds
- [ ] Attempting to call a commit-phase tool during worker phase → rejected
- [ ] Backtrack triggered → compensations execute in reverse + audit recorded
- [ ] Prompt-injection test set → both fast lane and ML lane catch the main cases

---

## 11. M5 — CLI + TUI + Full MELT (3-4 weeks)

**Goal**: developer-usable CLI + TUI; full observability.

### 11.1 Deliverables

```
crates/tars-cli/
  + Full command set: run / chat / dash (placeholder) / task list / task get / task cancel
  + --output json/yaml/markdown support
  + Config management: config validate / config show
  + Auto-completion (bash/zsh/fish)

crates/tars-frontend-cli/
  + CiAdapter (full CI mode impl)
  + Output formats: stdout / json / github-comment / junit-xml
  + Exit-code semantics (0/1/2/124)

crates/tars-frontend-tui/
  + TuiAdapter (ratatui)
  + Trajectory tree visualization
  + Live event stream
  + Cancel/Suspend/Resume keys

crates/tars-melt (full):
  + All Doc 08 §5 metrics implemented
  + Cardinality validator (startup + runtime)
  + LabelValidator
  + Logs strictly use SecretField
  + Trace head + tail sampling
  + AdaptiveSampler
  + Full OTel SDK + OTLP exporter integration
```

### 11.2 Validation

- [ ] User can run a task in TUI and see progress
- [ ] CI mode can output GitHub PR comment
- [ ] OTel Collector (local docker-compose) receives metric/trace/log
- [ ] Intentional cardinality violation → startup failure
- [ ] Log attempts to emit raw secret → fails at compile time or unit test

---

## 12. M6 — Multi-tenant + Postgres + Security (4-6 weeks)

**Goal**: upgrade from Personal to Team mode; production-grade security.

### 12.1 Deliverables

```
crates/tars-storage/
  + PostgresEventStore impl
  + Postgres migrations (per Doc 09 §3)
  + S3ContentStore impl (large objects)
  + RedisKVStore impl

crates/tars-config/
  + Full hot reload (file watcher + DB polling)
  + Full TenantConfig + 5-layer merge implementation
  + ConfigSubscriber pattern

crates/tars-security/
  + AuthResolver impls: oidc / jwt / mtls / oskey
  + IamEngine (in-memory + LDAP/Postgres backends)
  + OutputSanitizer (Doc 10 §8)
  + SecretRef + SecretResolver impls (Vault / env / file)

crates/tars-runtime/
  + Full tenant lifecycle (provision / suspend / resume / delete)
  + Audit (full AuditEvent set)
  + Per-tenant subprocess HOME isolation

crates/tars-pipeline/
  + Full AuthMiddleware
  + IamMiddleware (hard constraint pre-Cache)
  + Audit decisions at each layer

crates/tars-cache/
  + Three lines of defense (Doc 03 §10) complete
  + Cross-instance invalidation (Redis pub/sub)

crates/tars-server/
  + axum HTTP server
  + REST API (full Doc 12 §4 endpoints)
  + SSE streaming
  + OpenAPI auto-generation (utoipa)
  + JWT auth middleware (axum layer)
  + Health/Readiness probes
  + Prometheus /metrics endpoint
```

### 12.2 Validation

- [ ] Two tenants share a deployment with full data isolation
- [ ] Tenant A delete → all related data truly gone, audit retained
- [ ] OIDC login → obtain token → call API
- [ ] Missing IAM scope → 403
- [ ] Config file change → auto-reload (no restart needed)
- [ ] HTTP API passes full conformance test

---

## 13. M7 — Web Dashboard (3-4 weeks)

**Goal**: usable browser dashboard.

### 13.1 Deliverables

```
crates/tars-frontend-web/
  + WebAdapter (axum + embedded SPA)
  + REST + SSE + WebSocket
  + Auth: token / OIDC
  + 0.0.0.0 forces auth

ui/
  + SvelteKit / SolidJS choice (Svelte recommended for smaller bundle)
  + Pages: tasks / task detail / cache stats / cost dashboard / tenant admin
  + Real-time event stream (SSE consumer)
  + Trajectory tree visualization (D3)
  + Build → static files → rust-embed
```

### 13.2 Validation

- [ ] `tars dash` starts → browser opens automatically
- [ ] Task list visible
- [ ] Enter task detail; trajectory tree grows in real time
- [ ] Token / cost updates live
- [ ] Admin can see tenant list + switch tenants

---

## 14. M8 — FFI Bindings (parallel, 2-4 weeks each)

**Goal**: Python + Node users can import / require TARS.

### 14.1 Deliverables

```
crates/tars-py/
  + Full PyO3 binding (Doc 12 §6.2)
  + maturin build matrix
  + .pyi stubs auto-gen
  + asyncio integration tests
  + PyPI release pipeline

crates/tars-node/
  + Full napi-rs binding (Doc 12 §7.2)
  + d.ts auto-gen
  + npm release pipeline (per-platform .node files)

packages/types/ (TS only)
  + Generated from OpenAPI (openapi-typescript)
  + npm: @tars/types

packages/client-http/ (TS only)
  + Pure HTTP client (browser + Node)
  + npm: @tars/client-http
```

### 14.2 Validation

- [ ] `pip install tars` + Python sample script works
- [ ] `npm install @tars/runtime` + Node sample works
- [ ] Python conformance tests all pass (Doc 12 §10)
- [ ] TypeScript conformance tests all pass

---

## 15. M9 — Production Readiness (continuous)

**Goal**: all "lines of defense" established before v1.0 release.

### 15.1 Deliverables

```
- Full backup/restore drill passes (Doc 13 §10)
- DR drill (cross-region failure)
- Performance load test meets Doc 11 §2 SLOs
- Security pentest (third party)
- Full audit log workflow (write + SIEM mirror + verification)
- Runbook actual commands filled in (Doc 13 §5 each playbook)
- Status page deployed
- PagerDuty integration
- Customer-facing docs (distinct from internal design docs; for SDK users)
- Release notes / changelog
- Migration guide (0.x → 1.0, even for internal use)
- Training material for on-call
- v1.0 RC → 4 weeks of internal dogfooding → GA
```

### 15.2 Definition of Done (v1.0)

- [ ] All "Test strategy" sections in the 13 docs pass 100%
- [ ] Conformance tests (across bindings) 100% pass
- [ ] Performance benchmarks meet Doc 11 §2 SLOs
- [ ] Security pentest has no P0/P1 issues
- [ ] Backup/restore RTO 4h / RPO 1h achieved
- [ ] On-call team completes runbook training
- [ ] Customer docs complete
- [ ] CHANGELOG complete
- [ ] At least 1 internal prod deployment runs stably for 4 weeks

---

## 16. Critical Path

Dependency graph (thick arrow = strong dependency, thin arrow = recommended order):

```
M0 ──━━━━━▶ M1 ──━━━━━▶ M2 ──━━━━━▶ M3 ──━━━━━▶ M4 ──━━━━━▶ M5
                                       │             │             │
                                       │             │             ▼
                                       │             │            M7 (Web Dash)
                                       ▼             ▼
                                       └────━━━━━▶ M6 (Multi-tenant)
                                                     │
                                                     ▼
                                                    M8 (FFI bindings - parallelizable)
                                                     │
                                                     ▼
                                                    M9 (Production)
```

**Critical path**: M0 → M1 → M2 → M3 → M4 → M6 → M9
- Total: 5+5+4+7+5+5+8(M9 ongoing) = ~34 weeks
- M5 / M7 / M8 can run in parallel with later milestones

**Actual estimate**: **6-9 months to v1.0 GA**, assuming 1-2 full-time Rust engineers.

---

## 17. Risk Register

| Risk | Probability | Impact | Mitigation |
|---|---|---|---|
| Core abstractions need major rework | Medium | High | M0 vertical slice prototype validates |
| Major LLM provider API change | Medium | Medium | Abstraction layer (Doc 01) is prepared for this |
| FFI binding complexity underestimated | Medium | Medium | Evaluate M8 early (after M5); push to v1.1 if too complex |
| Performance below SLO | Low | High | Continuous benchmarking from M5; catch early |
| Multi-tenant isolation hole | Low | Very High | Pentest + fuzz mandatory after M6 completes |
| Conformance tests inconsistent across bindings | Medium | Medium | Build conformance framework early in M8; new bindings must pass |
| Team Rust experience insufficient | Medium | High | Training during M0 + strict code review |
| Major version upgrade of third-party deps (sqlx/axum) | Low | Medium | Pin versions, periodic review |
| Loss of business understanding from old prototype | Medium | Medium | Extract fixtures + business doc during M0 |
| Provider API key cost runaway (during dev) | Medium | Low | Add dev-mode budget guard / mock provider first |
| Team member turnover | Medium | High | Complete docs + pair programming + bus factor |

---

## 18. Decision Points (Pivot Triggers)

When to stop and re-evaluate direction:

### 18.1 M1 validation fails
**Symptom**: vertical slice doesn't work; core abstractions have a fundamental issue.
**Action**: pause implementation, return to docs to revise, redo M1. **Accept 1-2 weeks of slip in exchange for long-term correctness**.

### 18.2 M3 performance below SLO
**Symptom**: end-to-end single trajectory is 50%+ slower than raw API call.
**Action**: profile + analyze bottleneck. If architectural, discuss simplification (e.g. fewer middleware layers).

### 18.3 FFI binding extremely complex
**Symptom**: M8 estimate > 6 weeks (per binding).
**Action**: decision — downgrade to "v1.0 HTTP client only, FFI in v1.1".

### 18.4 Single-tenant prototype users churn
**Symptom**: during dogfood, internal users revert to old Python tools.
**Action**: pause feature work, do user interviews; may be a product positioning issue, not a technical one.

### 18.5 Provider relationship issue
**Symptom**: an LLM provider raises prices significantly / deprecates an API / tightens rate limits.
**Action**: prioritize local inference (mistral.rs / vLLM); may shift routing default.

---

## 19. Resource Planning

### 19.1 Minimum team configuration

| Role | Headcount | Responsibilities |
|---|---|---|
| Rust core engineer | 2 | Main implementation (M0-M6) |
| Frontend engineer | 1 (joins from M5) | TUI + Web Dashboard |
| SRE / DevOps | 1 (joins from M6) | Deployment / monitoring / runbook |
| Security engineer | 0.5 (M6 + M9) | Security review + pentest coordination |
| Product + user research | 0.5 | Continuous requirements input |
| **Total** | **~5 FTE** | |

Minimum viable: 1 Rust + 0.5 Frontend + 0.5 SRE, but timeline extends to 12-18 months.

### 19.2 Infrastructure cost estimate

**Dev environment** (per person):
- Laptop (M3 Max / Ryzen 7840HS+) — already on hand
- LLM API quota: $200/month (dev + test)
- IDE / tools — company provided

**CI** (GitHub Actions / similar):
- Matrix builds (Linux/macOS/Windows × Python 4 versions × Node 3 versions): ~$200/month
- Self-hosted runner (large builds): ~$300/month

**Staging**:
- 1× medium K8s cluster: ~$500/month
- Postgres + Redis: ~$200/month
- Test LLM API quota: $300/month

**Total during v1.0**: ~$1500-2500/month (excluding personnel)

### 19.3 Time-allocation guidance

Suggested weekly work split (per engineer):
- 60% coding + testing
- 15% design review (incl. doc evolution)
- 10% code review (peer)
- 10% learning / exploration
- 5% communication / documentation

Avoid 100% coding — tech debt accumulates rapidly.

---

## 20. Detailed Plan for Old-Prototype Disposition

### 20.1 During M0

- [ ] Audit `interview_app` / `ube_core` / `ube_project` code
- [ ] Extract business knowledge into docs (as supplemental examples for Doc 01-13)
- [ ] Extract data samples to `fixtures/`
- [ ] Extract prompt templates to `examples/prompts/`

### 20.2 After M3

- [ ] Rewrite the `interview` business as the first reference Skill using the new Skill framework (M4)
- [ ] Verify the new implementation is equivalent to the old Python (output comparison)

### 20.3 After M5

- [ ] Move old code to `archive/legacy-prototype/`
- [ ] Add README stating "this is reference code, no longer maintained"
- [ ] Tag git as `legacy-end`

### 20.4 At v1.0 release

- [ ] `archive/` may be retained or migrated to a separate repo
- [ ] Main repo is 100% Rust + necessary TS / Python packages

---

## 21. Anti-pattern Checklist

1. **Don't skip tests to make milestone dates** — tech debt compounds.
2. **Don't introduce new features during M0-M3** — focus on the core; new requests go to backlog.
3. **Don't pursue 100% config externalization before prod** — some hardcoding is OK during dev; clean up at M5.
4. **Don't let docs and code diverge persistently** — fix doc or code immediately on inconsistency; "for now, deal with it later" is not allowed.
5. **Don't implement multi-provider before M2** — single provider validates abstractions; premature multi-provider is harder to debug.
6. **Don't think about K8s before M6** — local docker-compose is sufficient for dev/test.
7. **Don't let any single milestone go > 8 weeks without delivery** — either split smaller or admit slippage.
8. **Don't optimize for "performance" before M5** — only at M5 do you have full metrics; optimization needs data.
9. **Don't think about conformance tests only at M8** — from M3 onward, every new feature needs a cross-binding mindset.
10. **Don't fully stabilize APIs before v1.0** — breaking changes are OK in v0.x; SemVer strictness begins after v1.0.
11. **Don't let team members look only at their own milestone** — weekly all-hands sync on progress to avoid silos.
12. **Don't over-abstract during PoC** — M0-M2 may have "we know we'll trait-ify later but use concrete types for now" code.
13. **Don't ignore customer feedback even in v0.x** — internal users mean feedback, which is highly valuable.
14. **Don't put on-call on production before v1.0** — all v0.x is best-effort; no pages.
15. **Don't equate "it runs" with "done"** — DoD includes tests + docs + acceptability by others.

---

## 22. Contracts with Up- and Downstream

### Upstream (business/product) commitments

- Provide a clear list of v1.0 must-have features (this doc §2.2)
- Provide continuous dogfood feedback from M3
- Don't commit to external users during v0.x

### Downstream (infrastructure / third-party) dependencies

- LLM Provider: maintain API stability (deprecation notice 6 months ahead)
- Cargo crates: workspace pins versions, upgrade prudently
- Cloud provider: K8s / managed DB / S3 available (M6+)

### Within the team

- Doc maintainers: any architectural change updates docs first
- Implementers: implement strictly per docs; discuss the doc when in doubt
- Reviewers: PR review checks doc-code consistency

---

## 23. Todos and Open Questions

- [ ] Adjust milestone timing once actual team size is known
- [ ] Choose Web Dashboard framework (before M7; Svelte / Solid)
- [ ] Decide whether v1.0 includes a gRPC server (depends on customer demand)
- [ ] CI choice (GitHub Actions vs self-hosted vs Buildkite)
- [ ] Documentation publishing approach (GitHub Pages / Mkdocs / Docusaurus)
- [ ] Final license decision (Apache-2.0 / MIT / dual / proprietary)
- [ ] Customer support channels (Discord / Slack Connect / email)
- [ ] Release cadence (continuous / monthly / quarterly)
- [ ] Whether to make the roadmap public
- [ ] Business model (open core / fully open source + service / closed source)
- [ ] Trademark / domain registration (`tars.ube` / `tarsruntime.dev` / etc.)
