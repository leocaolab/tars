# TARS — Multi-Agent LLM Runtime

[![ci](https://github.com/leocaolab/tars/actions/workflows/ci.yml/badge.svg)](https://github.com/leocaolab/tars/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](./rust-toolchain.toml)

**Rust-first agent runtime: 10+ LLM providers behind one trait, a composable middleware pipeline, an Agent abstraction you hand tasks to, and Python + Node bindings — observability built in.**

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
# Python
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

### Typed results — decode a completion into your own struct (Rust)

`resp.text` is a string; getting a value you can trust out of it is the
same dirty work in every consumer (strip the fence, find the JSON, handle
an out-of-range int). `tars-types::json_decode` owns that mechanism
generically: **the strong type is yours; tars is a generic engine — hand it
a `T`, get back a `T`.** It never learns your type or your envelope tag,
and it returns either a valid `T` or a typed `TarsJsonError` — you can't
hold an ill-formed `T` (*parse, don't validate*).

```rust
use tars_types::{decode, DecodeOpts, JsonAgentResponse};

// 1. Your serde type — mirrors the wire shape, lives in *your* crate.
#[derive(serde::Deserialize)]
struct FixReport { id: i64, changed: Vec<String> }

// 2. impl the generic trait — just declare your envelope tags (first
//    match wins; brackets optional; omit for bare JSON).
impl JsonAgentResponse for FixReport {
    fn wrapper_tags() -> &'static [&'static str] { &["<fix_report>", "<report>"] }
}

// 3. decode::<T> — strip fence → extract <fix_report> → parse-or-scrape
//    by `mode` → optional int-clamp → your strong type.
let mode = caps.supports_structured_output;                 // provider's StructuredOutputMode
let report: FixReport = decode(&resp.text, mode, DecodeOpts::clamping())?;
```

`mode` drives strict-vs-scrape: `StrictSchema` / `JsonObjectMode` parse
`text` directly (a fenced body is a broken promise → `InvalidJson`);
`None` / `ToolUseEmulation` scrape the first balanced JSON out of chatty
prose. A different agent is just a different type — the call is identical.
Shortcuts for the bare case: `decode_json::<T>(text, mode)` and
`resp.json::<T>(mode)`. **Rust-side only today** — Python/Node callers use
`response_schema` + `json.loads` / `JSON.parse` (see below). Full recipe:
[USER-GUIDE → Decoding a structured response](docs/USER-GUIDE.md#decoding-a-structured-response).

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

Buggy validators (raising or returning the wrong type) are caught and translated into the same permanent `TarsProviderError` — the worker is never crashed by user-side bugs. ([Doc 15](./docs/architecture/15-output-validation.md))

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

For a guided tour by role (architect / SDK author / SRE / security), see [docs/architecture/00-overview.md](./docs/architecture/00-overview.md).

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
| DeepSeek    | ✅        | ✅    | —      | ✅       | API key     | shipped  |
| vLLM        | ✅        | ✅    | varies | varies   | optional    | shipped  |
| MLX        | ✅        | varies| —      | varies   | none        | shipped  |
| llama.cpp  | ✅        | varies| —      | —        | none        | shipped  |

CLI providers (`claude_cli` / `gemini_cli` / `codex_cli`) reuse the user's existing subscription session via the vendor's official CLI, so users on Claude Pro / ChatGPT Plus don't need a separate API key. Documented in [docs/architecture/01-llm-provider.md §6](./docs/architecture/01-llm-provider.md). DeepSeek ships as a built-in `openai_compat` provider — available with just `DEEPSEEK_API_KEY`.

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

## Documentation

Three entry points for three audiences (see [docs/README.md](./docs/README.md) for the full map):

- **[USER-GUIDE.md](./docs/USER-GUIDE.md)** — 5-minute orientation for developers calling tars from their own code
- **[Comparison](./docs/comparison.md)** — TARS vs LangChain / LiteLLM / Letta / AutoGen / NVIDIA NIM
- **Architecture docs** (below) — design rationale and trade-off discussion

> **Languages**: the architecture docs are in English; the original Chinese versions are mirrored under [`docs/architecture/zh/`](./docs/architecture/zh/). Code identifiers and cross-references are language-agnostic.

| Doc | Topic |
|---|---|
| [00 — Overview](./docs/architecture/00-overview.md) | Architecture map, design philosophy, reading paths by role |
| [01 — LLM Provider](./docs/architecture/01-llm-provider.md) | 9-class backend abstraction; CLI subprocess reuse; tool-call three-stage; cache directives |
| [02 — Middleware Pipeline](./docs/architecture/02-middleware-pipeline.md) | 10-layer onion model; IAM front-loaded; dual-channel guard; cancel propagation |
| [03 — Cache Registry](./docs/architecture/03-cache-registry.md) | L1/L2/L3; content-addressed keys; ref counting; tenant isolation triple defense |
| [04 — Agent Runtime](./docs/architecture/04-agent-runtime.md) | Trajectory tree; event sourcing; saga compensation; recovery; frontend contract |
| [05 — Tools / MCP / Skills](./docs/architecture/05-tools-mcp-skills.md) | Three-layer concept separation; MCP integration; three Skill backends |
| [06 — Config + Multi-tenancy](./docs/architecture/06-config-multitenancy.md) | 5-layer override; lockdown; secret management; tenant lifecycle |
| [07 — Deployment + Frontends](./docs/architecture/07-deployment-frontend.md) | 4 deployment shapes; CLI/TUI/Web; hybrid control plane |
| [08 — MELT Observability](./docs/architecture/08-melt-observability.md) | Three data flows; cardinality control; mandatory PII redaction |
| [09 — Storage Schema](./docs/architecture/09-storage-schema.md) | Postgres + SQLite + Redis + S3; partitioning; tenant cleanup |
| [10 — Security Model](./docs/architecture/10-security-model.md) | STRIDE threat model; trust boundaries; isolation matrix; prompt injection |
| [11 — Performance + Capacity](./docs/architecture/11-performance-capacity.md) | SLO definitions; bottleneck analysis; cache ROI; load test method |
| [12 — API Specification](./docs/architecture/12-api-specification.md) | Rust / HTTP / gRPC / PyO3 / napi-rs / WASM surface |
| [13 — Operational Runbook](./docs/architecture/13-operational-runbook.md) | On-call playbook; 12 incident scenarios; backup/restore |
| [14 — Implementation Path](./docs/architecture/14-implementation-path.md) | Milestone roadmap M0 → M14 |
| [15 — Output Validation](./docs/architecture/15-output-validation.md) | JSON Schema enforcement; loose vs strict mode |
| [16 — Evaluation Framework](./docs/architecture/16-evaluation-framework.md) | Agent benchmarks; metrics; regression detection |
| [17 — Pipeline Event Store](./docs/architecture/17-pipeline-event-store.md) | Append-only event log feeding evaluation, replay, and audit |
| [18 — Agent Testing](./docs/architecture/18-agent-testing.md) | Deterministic agent tests; mock provider; metamorphic checks |
| [20 — Agent Abstraction](./docs/architecture/20-agent-abstraction.md) | The Agent contract from the user's view: hand a Task to a SkillSet |
| [21 — TarsAgent Impl Notes](./docs/architecture/21-tars-agent-impl-notes.md) | Native-agent build notes; the two-`ToolRegistry` unification |
| [22 — Codex TUI Port](./docs/architecture/22-codex-tui-port.md) | Fork Codex's Rust TUI onto the TARS runtime; how much of its tool layer ports |
| [23 — Unified Tool Layer](./docs/architecture/23-unified-tool-layer.md) | One `Tool` trait + gated dispatch + `ApprovalSink` + sandbox seam; retires the two-registry fork |

---

## Workspace layout

```
crates/
├── tars-types/        Core types (ChatRequest, Capabilities, Errors, ...)
├── tars-config/       5-layer config + secret refs + provider builtins
├── tars-provider/     Provider trait + 10 providers + HTTP base + auth
├── tars-pipeline/     Middleware stack (Telemetry / Cache / Retry / Routing)
├── tars-cache/        Cache registry trait + L1 in-mem + content-addressed keys
├── tars-model/        The Agent contract (trait Agent + Task / SkillSet)
├── tars-runtime/      Agent runtime (Session, Trajectory, TarsAgent, EnsembleAgent, ...)
├── tars-tools/        Built-in tools + MCP integration
├── tars-melt/         MELT observability primitives
├── tars-storage/      Storage trait + SQLite event store (Postgres incoming)
├── tars-server/       Personal-mode HTTP/REST shell (complete + streaming)
├── tars-cli/          `tars` binary (init / probe / bench)
├── tars-py/           PyO3 + maturin wheel (Python bindings)
└── tars-node/         napi-rs native addon (Node / TypeScript bindings)
```

63k+ LOC. Workspace builds clean on stable Rust 1.85+. CI: `cargo test --workspace --all-features` + clippy `-Dwarnings`.

---

## License

Apache-2.0.
