# TARS — Multi-Agent LLM Runtime

[![ci](https://github.com/leocaolab/tars/actions/workflows/ci.yml/badge.svg)](https://github.com/leocaolab/tars/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](./rust-toolchain.toml)

**Rust-first agent runtime: a dozen LLM providers — direct API, sandboxed
subscription-CLIs, and keyless cloud (Bedrock) — behind one trait; a composable
middleware pipeline; an Agent you hand tasks to; a data-driven model knowledge
base; Python + Node bindings — observability built in.**

---

## Why TARS exists

Most agent frameworks (LangChain, LlamaIndex, AutoGen) optimize for **rapid
prototyping in Python**. They're great for that. They're also where production
teams hit a wall: cache invariants are fuzzy, multi-tenancy isn't a primitive,
observability is bolted on, error semantics drift between providers, "just swap
providers" breaks at the edge cases (tool-use semantics, streaming protocols,
retry behavior), and — once you let an agent run a real CLI — nothing actually
sandboxes it.

TARS picks the other axis. The core engine is **Rust** — Tokio, Serde, typed
errors with a real class hierarchy (`Permanent` / `Retryable` / `RateLimited` /
`Auth`). Python and Node are **first-class bindings**, not wrappers around
`subprocess.run`. Multi-tenancy is enforced at every layer. Cache hit/miss is
observable per call. The same `Pipeline` runs identically locally (in-mem L1) and
in a service (Redis L2 + S3 L3) — same trait, same call sites.

I built it Rust-first because I want a runtime that stays **correct under the
conditions production actually hits**: fan-out tool-use loops at high
concurrency, unreliable providers with confusing retry semantics, prompt caches
that quietly invalidate, multi-tenant isolation where one customer's bad prompt
can't poison another's cache, and black-box coding-agent CLIs that must not be
able to touch anything outside the job's worktree.

If you want to prototype fast, use LangChain. If you want to **serve agents in
production with the predictability of a database**, use TARS.

---

## Philosophy

The goals are **performance, extensibility, and security** — and the design falls out of
a few deliberate bets. They say, more honestly than any feature list, both what TARS *is*
and what it refuses to be.

### Bet 1 — Correctness is a type-system property, not a matter of discipline

This is where TARS is genuinely unlike the rest of the field: it is a **functional,
strongly-typed** runtime in a space that is stringly-typed to the bone. Elsewhere a
rate-limit is a substring you grep out of an exception, a tool result is a dict you hope
has the right keys, and a "structured output" is a blob you re-parse at every call site
and pray over. TARS refuses all of it.

- **Errors are typed values, not strings.** `TarsError → Permanent / Retryable /
  RateLimited / Auth` is a real sum type; `retry_after` is a field, not a regex. You
  match on the variant and the compiler makes you handle it — and no sentinel token
  (`parse_failed`, `unknown`) ever leaks to a human; the raw truth does.
- **Parse, don't validate.** `json_decode` takes *your* `T` and returns a valid `T` or a
  typed error — you can never hold an ill-formed one. The strong type is yours; TARS is
  the generic engine that hands it back intact.
- **The pipeline is an algebra.** telemetry → auth → budget → cache → guard → routing →
  breaker *compose*; capability checks run pre-flight, so an incompatible request fails
  typed and offline instead of burning a network round-trip.
- **Correctness by construction.** A Turn commits or rolls back on `Drop` — there is no
  `armed = false` flag to forget. The invariant is held by the type system, not by a
  reviewer's attention.

This is also precisely **why we don't support MCP.** MCP is the opposite bet — a flat
`Json → Text` bag with no composition law, where the meaning lives in prose an LLM guesses
at; and in 2026 it is insecure and unscalable besides (OWASP publishes a whole *MCP Top
10*; its stateful transport fights a load balancer). Letting that into the core would
dissolve the one property that makes TARS worth building.
See [Doc 33 — Why TARS does not support MCP](docs/architecture/33-no-mcp.md).

### Bet 2 — Agents belong *embedded in software*, not floating above it

The durable, valuable place for an agent is **inside** your service — compiled into your
binary, next to your IAM role, your connection pools, your telemetry — where execution is
**deterministic, more efficient, and easier to maintain**. The model supplies intent;
typed Rust performs the act; every call is sandboxed, budgeted, and audited. An embedded
agent you can reason about beats an autonomous one you can only watch.

### Bet 3 — No autonomous, agent-driven planning — and we say so plainly

Open-ended, self-directed planning — "here is a vague goal, go decide the steps yourself,"
for things like exploratory coding or research — is **deliberately out of scope**. We
haven't found a case where wrapping that in TARS beats the tool already built for it. If
your task genuinely needs a planning agent, **use Claude or Codex directly** — that's what
they're excellent at. TARS owns the layer *underneath*: the typed, sandboxed, multi-tenant
execution a planner — yours, or a CLI delegate — runs on top of. Better a sharp boundary
than a fuzzy "does everything."

### Bet 4 — Skeptical of RAG, and of semantic vectors in general

Vector search buys *fuzzy, approximate* recall — and TARS's whole thesis is that fuzzy and
approximate is the problem, not the tool. Embedding similarity is **not accuracy**: it
returns plausible neighbours, misses exact matches, and can't answer a precise or
structured question the way a `SELECT … WHERE`, a `grep`, or a real index can. It's also
usually **not necessary**: a capable agent retrieves the way it does everything else —
search, read, refine, in exact steps — without a pre-baked embedding store. The one thing
vectors genuinely give you is sub-100 ms approximate lookup at scale, and agentic
workflows are **not latency-bound** the way a search box is — in most cases we don't need
the answer *this instant*. So we won't trade determinism and precision for speed we don't
need, nor take on a whole stateful vector subsystem to maintain (the same wrapper-sprawl
tax as MCP). TARS is deliberately **not a retrieval framework**; if you truly need RAG,
wire it in as one tool.

---

## What you get

- **One trait, a dozen providers.** Direct API (OpenAI, Anthropic, Gemini,
  DeepSeek), any **OpenAI-compatible** endpoint via `base_url` (Groq, xAI,
  OpenRouter, LM Studio, Ollama, …), local models (vLLM, MLX, llama.cpp),
  **subscription CLIs** (claude / gemini / codex / opencode / antigravity), and
  **keyless AWS Bedrock**. Swap providers without touching call sites.
- **Model versions are DATA, not code.** Model ids, prices, context windows, and
  thinking-mode live in [`crates/tars-config/data/models.toml`](crates/tars-config/data/models.toml).
  Bumping a default or refreshing a price is a data edit — no recompile — and cost
  is resolved **per model** from the reply's actual model.
- **Sandboxed by default.** Every black-box CLI-delegate agent runs in an OS
  write-jail (macOS Seatbelt / Linux bubblewrap): the worktree + `$TMPDIR` +
  `/tmp` + the CLI's own state dir are writable, `.git` and everything else are
  read-only. No delegate ever runs unconfined; `--sandbox danger-full-access` is
  the explicit opt-out.
- **A composable middleware pipeline.** Telemetry → Auth/IAM → Budget → Cache →
  Guard → Routing → Breaker. The same pipeline runs in-process or as a service.
- **Typed all the way down.** Typed errors (not stringly), **pre-flight capability
  checks** (catch a tool-use-against-a-non-tool-model before the round-trip), and a
  generic result-decode seam — hand it a `T`, get back a valid `T` or a typed
  error (*parse, don't validate*).
- **An Agent abstraction.** Hand a `Task` to an Agent; LLM-backed or not, both are
  first-class, and you can hedge one task across N agents.
- **First-class Python + Node bindings** (PyO3 / napi-rs).

---

## Quick start

```bash
git clone https://github.com/leocaolab/tars.git && cd tars

# Rust
cargo build --workspace --release

# Python
cd crates/tars-py && maturin develop --release
```

```bash
cargo run -p tars-cli -- init      # writes ~/.tars/config.toml
# Built-ins need only an env key: OPENAI_API_KEY / ANTHROPIC_API_KEY /
# GEMINI_API_KEY / DEEPSEEK_API_KEY. Local + subscription-CLI providers need no key.

cargo run -p tars-cli -- run -P deepseek --prompt "Say hi in five words."
```

### Run a completion (Python)

```python
import tars

# Pipeline = provider + middleware (telemetry, cache, retry).
p = tars.Pipeline.from_default("anthropic")

resp = p.complete(
    # omit `model` to use the provider's current default from the model KB,
    # or pin one explicitly (e.g. "claude-sonnet-5").
    system="You are a precise technical reviewer.",
    user="Review this Rust function for race conditions: ...",
    max_output_tokens=2000,
    thinking=True,
)

print(resp.text)
print(resp.usage)        # input/output/cached/thinking tokens
print(resp.telemetry)    # cache_hit, retry_count, layer trace, latency, cost
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
    model: ModelHint::Default,        // or ::Explicit("claude-sonnet-5".into())
    messages: vec![Message::user_text("...")],
    ..Default::default()
};

let mut stream = pipeline.call(req, ctx).await?;
while let Some(event) = stream.next().await { /* ... */ }
```

### Typed results — decode a completion into your own struct (Rust)

`resp.text` is a string; getting a value you can trust out of it is the same dirty
work in every consumer (strip the fence, find the JSON, handle an out-of-range
int). `tars-types::json_decode` owns that mechanism generically: **the strong type
is yours; tars is a generic engine — hand it a `T`, get back a `T`.** It never
learns your type or your envelope tag, and returns either a valid `T` or a typed
`TarsJsonError` — you can't hold an ill-formed `T` (*parse, don't validate*).

```rust
use tars_types::{decode, DecodeOpts, JsonAgentResponse};

#[derive(serde::Deserialize)]
struct FixReport { id: i64, changed: Vec<String> }

impl JsonAgentResponse for FixReport {
    fn wrapper_tags() -> &'static [&'static str] { &["<fix_report>", "<report>"] }
}

let mode = caps.supports_structured_output;   // provider's StructuredOutputMode
let report: FixReport = decode(&resp.text, mode, DecodeOpts::clamping())?;
```

`mode` drives strict-vs-scrape: `StrictSchema` / `JsonObjectMode` parse `text`
directly; `None` / `ToolUseEmulation` scrape the first balanced JSON out of chatty
prose. Shortcuts: `decode_json::<T>(text, mode)`, `resp.json::<T>(mode)`. Python /
Node callers use `response_schema` + `json.loads` / `JSON.parse`. Full recipe:
[USER-GUIDE → Decoding a structured response](docs/USER-GUIDE.md#decoding-a-structured-response).

---

## Providers

| Provider           | Streaming | Tools  | Vision | Thinking | Auth              |
|--------------------|-----------|--------|--------|----------|-------------------|
| OpenAI             | ✅        | ✅     | ✅     | ✅       | API key           |
| Anthropic          | ✅        | ✅     | ✅     | ✅       | API key           |
| Gemini             | ✅        | ✅     | ✅     | ✅       | API key           |
| DeepSeek           | ✅        | ✅     | —      | ✅       | API key           |
| **Bedrock**        | ✅        | ✅     | ✅     | ✅       | AWS IAM (keyless) |
| vLLM / MLX / llama.cpp | ✅    | varies | varies | varies   | none / optional   |
| Claude CLI         | buffered¹ | ✅     | ✅     | ✅       | subscription      |
| Gemini CLI         | buffered¹ | ✅     | ✅     | —        | subscription      |
| Codex CLI          | buffered¹ | ✅     | —      | —        | subscription      |
| **OpenCode CLI**   | buffered¹ | ✅     | —      | ✅       | subscription / BYO |
| **Antigravity CLI**| buffered¹ | ✅     | —      | —        | OAuth / env key   |

¹ *buffered* = the delegate returns the whole turn at once (event content is
identical; no incremental token stream yet).

**Three ways in, one canonical `ChatRequest`/`ChatResponse`:**

- **HTTP wire.** OpenAI, Anthropic, Gemini, DeepSeek + **any OpenAI-compatible**
  endpoint via `type = "openai_compat"` + `base_url` (Groq, xAI, OpenRouter, LM
  Studio, Ollama, vLLM, MLX, llama.cpp). Per-provider wire quirks live in a small
  `OpenAiDialect`, not `if`-branches in shared code.
- **CLI delegates** (`claude_cli` / `gemini_cli` / `codex_cli` / `opencode` /
  `antigravity`). Reuse the vendor's official CLI + your existing subscription/OAuth
  session — no separate API key. Each is a black-box agent, so **each runs OS-
  sandboxed** (write-jailed). Best-effort behind routing fallback.
- **Keyless cloud.** **Bedrock** via the unified `Converse` API; auth is the AWS
  credential chain (SigV4 by the SDK) — no key at rest, and on AWS the workload
  identity signs. Feature-gated (`--features bedrock`) so the AWS SDK stays out of
  the default build.

Design: [30 — OpenAI dialects](docs/architecture/30-openai-dialect.md) ·
[31 — Bedrock](docs/architecture/31-bedrock.md) ·
[32 — CLI delegates](docs/architecture/32-cli-delegates.md).

---

## The Agent abstraction

> An **Agent** is a set of capabilities (skills) that you hand a **task** to.
> ([Doc 20](docs/architecture/20-agent-abstraction.md))

The contract lives in **`tars-model`** (pure, depends only on `tars-types`):
`trait Agent { id, role, skills, run(task) }` + `Task` (the recursive unit of
intent) + `Permissions` / `AgentContext`. `run` takes a **Task** — user intent —
not a `ChatRequest`; turning a task into LLM calls is a *native* agent's internal
job, so an agent that uses no LLM stays first-class.

- **`TarsAgent`** (`tars-runtime`) — LLM-backed: turns the task into a prompt and
  drives a white-box tool loop over a *pure-inference* provider. Swap the provider
  and the same agent is a "gemini agent" or a "claude_cli agent" — **tars owns the
  loop, tools, `cwd`, and the sandbox**, not the CLI's internal black box.
- **user agents** — anything that implements `Agent::run(task)`.
- **`EnsembleAgent`** runs one task on N agents concurrently and takes the first
  success (a tail-latency hedge at *task* granularity).

**Scope (see Philosophy · Bets 2–3).** `TarsAgent` drives a **bounded, white-box tool
loop** — tars owns the loop, tools, `cwd`, and sandbox. It is *not* an autonomous
planner that decomposes an open-ended goal on its own: a `Task` splits into sub-`Task`s
because *your code* (or an orchestrator) says so, not because the runtime went planning.
The planner — your own, or a black-box CLI delegate — runs *on top of* this typed,
sandboxed execution layer.

---

## Model knowledge base

Model ids, prices, context windows, modalities, and thinking behavior change
faster than releases — so they're **data**, in
[`crates/tars-config/data/models.toml`](crates/tars-config/data/models.toml), not
string literals in code. Built-in defaults, per-model pricing (cost is resolved
from the reply's actual model), and provider quirks (e.g. Gemini 2.5 uses a numeric
`thinkingBudget`, 3.x uses `thinkingLevel`, thinking-only models reject "off") all
read from it. Ship a new model or fix a stale default with a one-line data edit.

---

## A couple of mechanics worth knowing

**Atomic Turn rollback via `Drop` guard.** `Session::send` builds turns through a
`TurnGuard` that rolls back on `Drop`; success calls `commit()` (`mem::forget`).
`?` early-returns, panics, and tokio cancellation are all handled uniformly —
there's no `armed = false` flag to forget and silently keep a half-Turn.

**Capability pre-flight before routing.** The routing layer runs
`ChatRequest::compatibility_check(&Capabilities)` against each candidate *before*
dispatch — tool-use vs. a non-tool model, oversized prompt, unsupported thinking/
vision — so an incompatible request fails **without** burning a network round-trip.
It aggregates *all* reasons (typed, no early-exit); `ProviderError::NoCompatibleCandidate`
carries the full skipped list.

**Typed errors, not strings.** `TarsError` → `TarsConfigError` /
`TarsProviderError` / `TarsRuntimeError`, with subclasses (e.g.
`TarsRoutingExhaustedError` exposing `skipped_candidates`) where structured access
matters. `e.kind == "rate_limited"` carries `retry_after`; `e.is_retriable` tells
you whether the pipeline already exhausted retries.

---

## Documentation

- **[USER-GUIDE.md](docs/USER-GUIDE.md)** — 5-minute orientation for calling tars
  from your own code.
- **[Comparison](docs/comparison.md)** — TARS vs LangChain / LiteLLM / Letta /
  AutoGen / NVIDIA NIM.
- **[Architecture docs](docs/README.md)** — design rationale and trade-offs, by
  subsystem (provider, pipeline, cache, agent runtime, tools, security,
  observability, storage, …); plus [Doc 33 — why TARS does not support
  MCP](docs/architecture/33-no-mcp.md). English, with Chinese mirrors under
  [`docs/architecture/zh/`](docs/architecture/zh/).

---

## License

Apache-2.0.
