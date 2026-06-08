# TARS — Multi-Agent LLM Runtime

[![ci](https://github.com/leocaolab/tars/actions/workflows/ci.yml/badge.svg)](https://github.com/leocaolab/tars/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](./rust-toolchain.toml)

**Rust-first agent runtime supporting 8+ LLM providers, with PyO3 Python bindings, a 10-layer middleware pipeline, and observability by construction.**

> **⚠️ Pre-1.0 Preview**
>
> Public for transparency, code review, and design feedback — not for broader adoption yet. **No community campaign, no announcement, no roadshow** has happened or is planned before v1.0; star and fork counts are intentionally low at this stage. The only way someone is reading this README right now is targeted (peer review, hiring evaluation, or curiosity from an adjacent project), not organic discovery.
>
> Track the [Releases page](../../releases) for the v1.0 announcement, which will include a stability commitment, migration guide, and a proper README rewrite for a broader audience. Until then, expect breaking API changes between minor versions, and don't put TARS on a critical production path unless you're prepared to follow `main` closely.

> **How this came together**
>
> TARS landed as a focused Pre-1.0 sprint — the goal was to establish a complete architectural baseline (8 providers, middleware pipeline, output validation, per-call observability, multi-turn session, agent runtime) and validate it against a real downstream consumer (dogfooded in production) before any broader announcement. **Documentation is design-ahead in places** — the 17 design docs describe the full target shape; the [Status block below](#tars--multi-agent-llm-runtime) and [CHANGELOG.md](./CHANGELOG.md) are authoritative for what's actually shipped vs. designed. If you spot a doc that describes a layer not yet in `crates/`, that's expected — design first, then build, then `<unreleased>` → release.

> **Status (2026-05, v0.2.0):** M0–M7 shipped (types / config / provider / pipeline / cache / runtime / tools). M8 (`tars-py`) in progress — `Provider`, `Pipeline`, `Session`, `CapabilityRequirements`, `CompatibilityResult`, and Python output validators (Pass / Reject / FilterText / Annotate) all exposed. Workspace builds clean on stable Rust 1.85+ with `cargo clippy -Dwarnings` green. See [CHANGELOG.md](./CHANGELOG.md) for per-milestone shipped detail.

---

## Why TARS exists

Most agent frameworks (LangChain, LlamaIndex, AutoGen) optimize for **rapid prototyping in Python**. They're great for that. They're also where production teams hit a wall: cache invariants are fuzzy, multi-tenancy isn't a primitive, observability is bolted on, error semantics drift between providers, and "just swap providers" routinely breaks at the edge cases (tool-use semantics, streaming protocols, retry behavior).

TARS picks the other axis. The core engine is Rust — built on Tokio, Serde, typed errors with class hierarchy (`Permanent` / `Retryable` / `RateLimited` / `Auth`). Python is a **first-class binding**, not a wrapper around `subprocess.run`. Multi-tenancy is enforced at every layer. Cache hit/miss is observable per call. The same Pipeline runs identically locally (in-mem L1) and in a service (Redis L2 + S3 L3) — same trait, same call sites.

We chose Rust-first because we want a runtime that's correct under the conditions production hits: fan-out tool-use loops at high concurrency, unreliable providers with confusing retry semantics, prompt caches that quietly invalidate, multi-tenant isolation where one customer's bad prompt can't poison another's cache.

If you want to prototype fast, use LangChain. If you want to serve agents in production with the same predictability as a database — TARS.

See [docs/comparison.md](./docs/comparison.md) for head-to-head positioning.

---

## Quick start

### Install

```bash
# Python (M8 in progress)
git clone https://github.com/leocaolab/tars.git
cd tars/crates/tars-py
maturin develop --release

# Or use Rust directly
cd tars
cargo build --workspace --release
```

### Bootstrap config

```bash
cargo run -p tars-cli -- init
# writes ~/.tars/config.toml with starter providers (Anthropic / OpenAI / vLLM / MLX / llama.cpp)
# add your API keys via env vars (referenced from the TOML)
```

### Run a completion (Python)

```python
import tars

# Pipeline = provider + middleware (telemetry, cache, retry).
# Layer-1 raw `Provider` also available if you want to bring your own.
p = tars.Pipeline.from_default("anthropic")

resp = p.complete(
    model="claude-sonnet-4-5",
    system="You are a precise technical reviewer.",
    user="Review this Rust function for race conditions: ...",
    max_output_tokens=2000,
    thinking=True,
)

print(resp.text)
print(resp.usage)        # input/output/cached/thinking tokens
print(resp.telemetry)    # cache_hit, retry_count, layer trace, latency
```

### Run a completion (Rust)

```rust
use tars_pipeline::Pipeline;
use tars_provider::registry::ProviderRegistry;
use tars_types::{ChatRequest, Message, ModelHint};

let cfg = tars_config::ConfigManager::load_from_default_path()?;
let registry = ProviderRegistry::from_config(&cfg.providers, http, auth)?;
let provider = registry.get(&"anthropic".into()).unwrap();

let pipeline = Pipeline::builder(provider)
    .layer(TelemetryMiddleware::new())
    .layer(CacheLookupMiddleware::new(cache, factory, origin))
    .layer(RetryMiddleware::default())
    .build();

let req = ChatRequest {
    model: ModelHint::Explicit("claude-sonnet-4-5".into()),
    messages: vec![Message::user_text("...")],
    ..Default::default()
};

let mut stream = pipeline.call(req, ctx).await?;
while let Some(event) = stream.next().await {
    /* ... */
}
```

### Pre-flight capability check (no model call)

```python
# Verify each agent role's configured provider can satisfy its needs
# at startup, instead of failing at runtime on first request.
roles = {
    "planner":   tars.CapabilityRequirements(requires_thinking=True),
    "executor":  tars.CapabilityRequirements(requires_tools=True,
                                              estimated_max_output_tokens=8000),
    "reviewer":  tars.CapabilityRequirements(requires_structured_output=True),
}

for role, reqs in roles.items():
    p = tars.Pipeline.from_default(provider_for(role))
    r = p.check_capabilities(reqs)
    if not r:
        print(f"role={role!r} can't satisfy: {[x.kind for x in r.reasons]}")
        sys.exit(1)
```

### Python output validators

Attach Python callbacks that run after the model reply, before the response reaches caller code. Validators chain in order — each sees the previous one's filtered output.

```python
def must_be_json(req, resp):
    try:
        json.loads(resp["text"])
        return tars.Pass()
    except ValueError as e:
        return tars.Reject(reason=str(e))

def strip_pii(req, resp):
    return tars.FilterText(text=resp["text"].replace(EMAIL, "[REDACTED]"))

p = tars.Pipeline.from_default("anthropic", validators=[
    ("strip_pii", strip_pii),
    ("must_be_json", must_be_json),
])
```

`tars.Reject` is always classified as `Permanent` — `RetryMiddleware` does not retry on validation failures (same prompt → same model → same output; model retry on validation failure is a near-pure gamble that doesn't belong inside the runtime). Callers that want a model resample on validation failure catch `TarsProviderError(kind="validation_failed")` at their own layer with explicit prompt variation.

Buggy validators (raising or returning the wrong type) are caught and translated into the same permanent `TarsProviderError` — the worker is never crashed by user-side bugs. ([Doc 15](./docs/architecture/zh/15-output-validation.md))

---

## Architecture

```
                       ┌──────────────────────────────┐
                       │   Frontends (CLI / TUI /     │
                       │   Web / CI hooks)            │
                       └──────────────┬───────────────┘
                                      │
                       ┌──────────────▼───────────────┐
                       │   API Layer                  │
                       │   Rust trait / HTTP+SSE /    │
                       │   Python (PyO3) / TS (napi)  │
                       └──────────────┬───────────────┘
                                      │
                       ┌──────────────▼───────────────┐
                       │   Agent Runtime              │
                       │   Trajectory tree + events   │
                       │   + backtrack + recovery     │
                       └────┬─────────────────┬──────┘
                            │                 │
              ┌─────────────▼─────┐  ┌────────▼─────────┐
              │  Tools / MCP /    │  │  PromptBuilder   │
              │  Skills           │  │  (static prefix /│
              │  3-layer abstract │  │  project anchor /│
              └─────────┬─────────┘  │  dynamic suffix) │
                        │            └────────┬─────────┘
                        └──────────┬──────────┘
                                   │
                       ┌───────────▼───────────────┐
                       │  Middleware Pipeline      │
                       │  Telemetry → Auth → IAM   │
                       │  → Budget → Cache → Guard │
                       │  → Routing → Breaker      │
                       └───────────┬───────────────┘
                                   │
              ┌────────────────────┼─────────────────┐
              │                    │                 │
   ┌──────────▼──────┐  ┌──────────▼─────┐  ┌──────▼──────┐
   │ Cache Registry  │  │ LLM Provider   │  │ Tool/MCP    │
   │ L1 mem / L2 sql │  │ HTTP / SSE /   │  │ subprocess  │
   │ + ref counting  │  │ CLI / Embedded │  │ + isolation │
   └─────────────────┘  └────────────────┘  └─────────────┘

   ▲ Cross-cutting (every layer above depends on these)
   │
   ┌─────────────────────────────────────────────────────────┐
   │ Storage:  Postgres / SQLite / Redis / S3                │
   │ Config:   5-layer override + Secret refs                │
   │ Security: Auth / IAM / Encryption / Audit               │
   │ MELT:     Metrics / Events / Logs / Traces (typed)      │
   └─────────────────────────────────────────────────────────┘
```

### The Agent abstraction

> An **Agent** is a collection of capabilities (skills) that you hand a
> **task** to. ([docs/architecture/20-agent-abstraction.md](./docs/architecture/20-agent-abstraction.md))

The contract lives in **`tars-model`** (pure, depends only on `tars-types`):
`trait Agent { id, role, skills, run(task) }` + `Task` (the recursive unit
of intent) + `Permissions` / `AgentContext`. `run` takes a **Task** — user
intent — not a `ChatRequest`; turning a task into LLM calls is a *native*
agent's internal job, so an agent that uses no LLM stays first-class.

Two implementers, one interface (tars is an adaptor over both):
- **`TarsAgent`** (`tars-runtime`) — LLM-backed: turns the task into a
  prompt and drives a white-box tool loop over a *pure-inference* provider.
  Swap the provider and the same agent is a "gemini agent" or a
  "claude_cli agent" — tars owns the loop, tools, and `cwd`, not the CLI's
  internal black box.
- **user agents** — anything that implements `Agent::run(task)`.

Compose them: **`EnsembleAgent`** runs one task on N agents concurrently and
takes the first success (tail-latency hedge at *task* granularity, above
the pipeline's completion-level ensemble).

For a guided tour by role (consumerhitect / SDK author / SRE / security), see [docs/architecture/00-overview.md](./docs/architecture/00-overview.md).

---

## Providers supported

| Provider    | Streaming | Tools | Vision | Thinking | Auth        | Status   |
|-------------|-----------|-------|--------|----------|-------------|----------|
| OpenAI      | ✅        | ✅    | ✅     | ✅       | API key     | shipped  |
| Anthropic   | ✅        | ✅    | ✅     | ✅       | API key     | shipped  |
| Gemini      | ✅        | ✅    | ✅     | ✅       | API key     | shipped  |
| Claude CLI  | ✅        | ✅    | ✅     | ✅       | subscription| shipped  |
| Gemini CLI  | ✅        | ✅    | ✅     | —        | subscription| shipped  |
| Codex CLI   | ✅        | ✅    | —      | —        | subscription| shipped  |
| vLLM        | ✅        | ✅    | varies | varies   | optional    | shipped  |
| MLX        | ✅        | varies| —      | varies   | none        | shipped  |
| llama.cpp  | ✅        | varies| —      | —        | none        | shipped  |

CLI providers (`claude_cli` / `gemini_cli` / `codex_cli`) reuse the user's existing subscription session via the vendor's official CLI, so users on Claude Pro / ChatGPT Plus don't need a separate API key. Documented in [docs/architecture/01-llm-provider.md §6](./docs/architecture/01-llm-provider.md).

---

## Key design choices

**Every choice has a "why"; the [docs](./docs/) are where they live. Headlines:**

1. **Layered, not monolithic.** Each layer is single-responsibility. Replace any one layer without touching the rest. Provider trait → can swap OpenAI to vLLM. Cache trait → can swap mem to Redis. Storage trait → SQLite to Postgres.

2. **Plan as DAG, execute as state machine.** Agent orchestration plans express as DAG; runtime executes as event-sourced state machine with backtrack, retry, and recovery. The two are not mixed. ([Doc 04](./docs/architecture/zh/04-agent-runtime.md))

3. **Tenant isolation is sacred.** Cross-tenant leakage of data, compute, or side effects is a P0 bug. No optimization (cache sharing, model warmup, batch fusing) is allowed to cross a tenant boundary. ([Doc 06](./docs/architecture/zh/06-config-multitenancy.md))

4. **Fail closed.** Every safety layer (auth, IAM, cache lookup, budget, schema validation) defaults to denying when it errors. No "open by default for now."

5. **Observable by construction.** MELT (Metrics / Events / Logs / Traces) is not a layer you add later — every component emits typed signals from day 1, with cardinality control and PII redaction enforced at the layer ([Doc 08](./docs/architecture/zh/08-melt-observability.md)).

6. **Trust nothing you didn't compute.** LLM output, user input, tool returns, MCP server responses — all are external and pass through explicit validators before influencing system state.

7. **Cost is a first-class concern.** LLM calls dominate (~95%) production cost. Cache, routing, model tier, budget enforcement are co-designed.

8. **Single source of truth.** The Rust trait is canonical. HTTP / gRPC / Python (PyO3) / TypeScript (napi-rs) are projections of it. No binding is allowed to drift from the core.

---

## Notable runtime mechanics

A few that aren't obvious from docs scanning:

### Atomic Turn rollback via Drop guard

`Session::send` builds turns through a `TurnGuard` that defaults-rollback on `Drop`. Success path calls `commit()` which `mem::forget`s the guard. Catches `?` early-return, panics, and tokio cancellation **uniformly** — there's no way to forget a single `armed = false` flag and silently keep a half-Turn.

```rust
let guard = TurnGuard::new(&mut self.turns, boundary);
// ... build turn ...
guard.commit();  // success — keeps the turn
// any return without commit() — rollback truncates back to boundary
```

### Capability pre-flight before routing

Routing layer runs `ChatRequest::compatibility_check(&Capabilities)` against each candidate **before** dispatch. Catches incompatibilities (tool-use against non-tool model, oversized prompt, etc.) without burning a network round-trip. Six axes ([B-31 v4](./CHANGELOG.md#b-31)):

- `ToolUseUnsupported { tool_count }`
- `StructuredOutputUnsupported`
- `ThinkingUnsupported { mode }`
- `VisionUnsupported`
- `ContextWindowExceeded { estimated_prompt_tokens, max_context_tokens }`
- `MaxOutputTokensExceeded { requested, max }`

Aggregates **all** incompatibilities (no early-exit), so the caller sees the full list. When all candidates are filtered out: `ProviderError::NoCompatibleCandidate { skipped: Vec<(ProviderId, Vec<CompatibilityReason>)> }` — typed, not a string-mashed error message.

### Auto tool loop

When `ToolRegistry` is set, `Session::send` dispatches tools and re-invokes the model until it returns a text-only reply. Parallel tool calls dispatched in order and packaged into one user message with N `tool_result` blocks (Anthropic's wire protocol requires this). Manual mode reachable by leaving the registry empty and consuming `Response.tool_calls` from the caller side.

### History versioning for cache invalidation

`Session.history_version: u64` increments on visible mutations (successful send, reset). Does **not** increment on rollback (truncating back to pre-send is observably unchanged) or during in-flight tool loops. From an established pattern. Useful for cache invalidation and `(session_id, history_version)` log correlation. `fork()` preserves the parent value so caches recognize shared prefixes.

### Typed errors, not strings

```python
try:
    p.complete(model="...", user="...")
except tars.TarsRoutingExhaustedError as e:
    # e.skipped_candidates: list[tuple[provider_id, list[CompatibilityReason]]]
    for pid, reasons in e.skipped_candidates:
        log.warn(f"{pid} skipped: {[r.kind for r in reasons]}")
except tars.TarsProviderError as e:
    if e.kind == "rate_limited":
        await asyncio.sleep(e.retry_after or 30)
    elif e.kind == "unknown_tool":
        log.fatal(f"register tool {e.tool_name}")
    elif e.is_retriable:
        # Pipeline already retried; this is the final failure.
        ...
```

Class hierarchy: `TarsError` → `TarsConfigError` / `TarsProviderError` / `TarsRuntimeError`. Subclasses (e.g. `TarsRoutingExhaustedError`) for variants needing structured access. Generic catch-all (`except TarsProviderError`) still matches.

---

## What's shipped vs. designed

This repo is **design-ahead**. Some docs describe systems that don't fully exist yet — that's deliberate. We write the doc first to align on what we're building, then build it. We don't try to keep every doc current with every commit; deferred surfaces are tagged in [TODO.md §D-1..D-13](./TODO.md).

| Surface              | Status                          |
|----------------------|---------------------------------|
| Type system          | Shipped (`tars-types`)          |
| Config + multi-layer | Shipped (`tars-config`)         |
| Provider trait + 8+ backends | Shipped (`tars-provider`) |
| Middleware pipeline  | Shipped (`tars-pipeline` — Telemetry / Cache / Retry / Routing) |
| L1 in-memory cache   | Shipped (`tars-cache`)          |
| Agent runtime core   | Shipped (`tars-runtime` — Session, Trajectory, Events) |
| Tools + MCP          | Shipped (`tars-tools` — built-ins + MCP integration) |
| CLI (`tars init`, `tars probe`, `tars bench`) | Shipped (`tars-cli`) |
| Python bindings      | **In progress** (`tars-py` M8 — Provider/Pipeline/Session live; output validators live; routing/capability surface live) |
| TypeScript bindings  | Designed only (Doc 12 §6)       |
| HTTP / gRPC service  | Designed only (Doc 12 §3, §5)   |
| Postgres + Redis storage | Designed only (Doc 09)      |
| Multi-tenant runtime | Partial (sketches in Doc 06)    |
| Web / TUI dashboards | Designed only (Doc 07)          |

---

## Engineering practice

Two things you'll notice if you spend time in the repo:

### Trigger-or-delete contracts

Scaffolds and speculative abstractions are tracked in [TODO.md §O-1..O-10](./TODO.md). Each entry has:

- **Where it lives** (file path)
- **Why deferred** (the ergonomic or scope reason)
- **Trigger to commit** (condition that justifies the abstraction)
- **Trigger to delete** (condition where it should be ripped out)

Carry-cost vs. removal-cost is explicit. Nothing is "we keep it just in case" — it's "we keep it until X or Y, then act."

### Audit-driven evolution

The 2026-05-03 downstream-consumer self-review surfaced ~330 issues across three rounds. Critical + error tier was triaged and shipped (commits `9683ce8` / `67de40d` / `cf1605e` / `af2d8f1`). Non-critical residue lives in [TODO.md §A-1..A-6](./TODO.md) with revisit triggers, not "we'll fix it eventually."

The B-31 capability pre-flight feature went through five review passes (v1 → v5), each adding a structured improvement: typed enum, `#[non_exhaustive]`, context-window check, PyO3 expose, structured tracing fields, dedicated exception subclass, typed config-time API. Each pass is in CHANGELOG with rationale.

---

## Documentation

Three entry points for three audiences (see [docs/README.md](./docs/README.md) for the full map):

- **[USER-GUIDE.md](./docs/USER-GUIDE.md)** — 5-minute orientation for developers calling tars from their own code
- **[Comparison](./docs/comparison.md)** — TARS vs LangChain / LiteLLM / Letta / AutoGen / NVIDIA NIM
- **Architecture docs** (below) — design rationale and trade-off discussion

> **Translation status**: Doc 00 + Doc 17 are in English; Doc 01–16 are currently Chinese-only at `docs/architecture/zh/` (English translation deferred). Architectural cross-refs and code identifiers are language-agnostic; the design discussions are readable via machine translation if needed.

| Doc | Topic |
|---|---|
| [00 — Overview](./docs/architecture/00-overview.md) | Architecture map, design philosophy, reading paths by role |
| [01 — LLM Provider](./docs/architecture/zh/01-llm-provider.md) [zh] | 9-class backend abstraction; CLI subprocess reuse; tool-call three-stage; cache directives |
| [02 — Middleware Pipeline](./docs/architecture/zh/02-middleware-pipeline.md) [zh] | 10-layer onion model; IAM front-loaded; dual-channel guard; cancel propagation |
| [03 — Cache Registry](./docs/architecture/zh/03-cache-registry.md) [zh] | L1/L2/L3; content-addressed keys; ref counting; tenant isolation triple defense |
| [04 — Agent Runtime](./docs/architecture/zh/04-agent-runtime.md) [zh] | Trajectory tree; event sourcing; saga compensation; recovery; frontend contract |
| [05 — Tools / MCP / Skills](./docs/architecture/zh/05-tools-mcp-skills.md) [zh] | Three-layer concept separation; MCP integration; three Skill backends |
| [06 — Config + Multi-tenancy](./docs/architecture/zh/06-config-multitenancy.md) [zh] | 5-layer override; lockdown; secret management; tenant lifecycle |
| [07 — Deployment + Frontends](./docs/architecture/zh/07-deployment-frontend.md) [zh] | 4 deployment shapes; CLI/TUI/Web; hybrid control plane |
| [08 — MELT Observability](./docs/architecture/zh/08-melt-observability.md) [zh] | Three data flows; cardinality control; mandatory PII redaction |
| [09 — Storage Schema](./docs/architecture/zh/09-storage-schema.md) [zh] | Postgres + SQLite + Redis + S3; partitioning; tenant cleanup |
| [10 — Security Model](./docs/architecture/zh/10-security-model.md) [zh] | STRIDE threat model; trust boundaries; isolation matrix; prompt injection |
| [11 — Performance + Capacity](./docs/architecture/zh/11-performance-capacity.md) [zh] | SLO definitions; bottleneck analysis; cache ROI; load test method |
| [12 — API Specification](./docs/architecture/zh/12-api-specification.md) [zh] | Rust / HTTP / gRPC / PyO3 / napi-rs / WASM surface |
| [13 — Operational Runbook](./docs/architecture/zh/13-operational-runbook.md) [zh] | On-call playbook; 12 incident scenarios; backup/restore |
| [14 — Implementation Path](./docs/architecture/zh/14-implementation-path.md) [zh] | Milestone roadmap M0 → M14 |
| [15 — Output Validation](./docs/architecture/zh/15-output-validation.md) [zh] | JSON Schema enforcement; loose vs strict mode |
| [16 — Evaluation Framework](./docs/architecture/zh/16-evaluation-framework.md) [zh] | Agent benchmarks; metrics; regression detection |
| [17 — Pipeline Event Store](./docs/architecture/17-pipeline-event-store.md) | Append-only event log feeding evaluation, replay, and audit |

---

## Workspace layout

```
crates/
├── tars-types/        Core types (ChatRequest, Capabilities, Errors, ...)
├── tars-config/       5-layer config + secret refs + provider builtins
├── tars-provider/     Provider trait + 9 backends + HTTP base + auth
├── tars-pipeline/     Middleware stack (Telemetry / Cache / Retry / Routing)
├── tars-cache/        Cache registry trait + L1 in-mem + content-addressed keys
├── tars-runtime/      Agent runtime (Session, Trajectory, Worker, Critic, ...)
├── tars-tools/        Built-in tools + MCP integration
├── tars-melt/         MELT observability primitives
├── tars-storage/      Storage trait (sketched; SQLite / Postgres backends incoming)
├── tars-cli/          `tars` binary (init / probe / bench)
└── tars-py/           PyO3 + maturin wheel (M8, in progress)
```

23k+ LOC. Workspace builds clean on stable Rust 1.85+. CI: `cargo test --workspace --all-features` + clippy `-Dwarnings`.

---

## License

Apache-2.0.
