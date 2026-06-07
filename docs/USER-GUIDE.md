# tars User Guide

A 5-minute orientation for developers who want to call tars from their
own code. Covers the three common call shapes + when tars is the wrong
tool. For *why* tars is shaped this way, jump to
[`architecture/`](./architecture/) — most callers don't need to.

> **Pre-1.0 disclaimer**: API surfaces may change between minor versions
> until v1.0. The shapes shown here are what currently work; track the
> [Releases page](../../../releases) for stability commitments.

---

## What tars is

A Rust-first multi-provider LLM runtime: one trait + one middleware
stack covers Anthropic, OpenAI, Gemini, vLLM, MLX, llama.cpp, and three
CLI-based subscription providers (`claude_cli`, `gemini_cli`,
`codex_cli`). Python bindings ship as a wheel; you can also use it
directly from Rust.

What you get without writing it yourself:

- **Provider abstraction** — swap models without touching call sites
- **Middleware pipeline** — telemetry, cache, retry, output validation,
  pipeline event store, all engaged automatically by the default
  `Pipeline`
- **Capability pre-flight** — verify a provider supports your request
  shape (tools, thinking, structured output, context window) before
  burning a network call
- **Multi-turn `Session`** — history accumulation + tool dispatch loop
  + atomic per-turn rollback
- **Per-call observability** — `cache_hit`, `retry_count`,
  `validation_summary`, layer trace, latency, all on every response

## Hello, tars (5-minute path)

If you just want to confirm tars works on your machine, these three
commands are the shortest route to a successful LLM call. No Python,
no Rust code — just shell.

```bash
# 1. Build + write a starter config to ~/.tars/config.toml.
cargo run -p tars-cli -- init

# 2. Set the credential the starter config references.
export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY / GOOGLE_API_KEY

# 3. Send one prompt.
cargo run -p tars-cli -- run -p anthropic "Say hi in 5 words."
```

Expected: the model's reply on stdout, a one-line `usage:` summary on
stderr. If you see `error in tars run:` instead, double-check the env
var name matches what `~/.tars/config.toml` declares for that provider.

Want a different provider? Replace `-p anthropic` with one of the ids
in `~/.tars/config.toml` (`-p openai_main`, `-p claude_cli`, etc.).
For the long-lived subscription path, see
[`providers/claude-cli.md`](./providers/claude-cli.md).

Built-in providers (available without writing any config — just export
the key): `openai`, `anthropic`, `gemini`, `deepseek`, plus the local /
subscription backends (`claude_cli`, `gemini_cli`, `mlx`, `llamacpp`,
`vllm`). DeepSeek is reached via its OpenAI-compatible API
(`DEEPSEEK_API_KEY`, default model `deepseek-v4-flash`); request
`deepseek-v4-pro` for the reasoning model — its chain-of-thought arrives
on the thinking channel automatically:

```bash
export DEEPSEEK_API_KEY=sk-...
cargo run -p tars-cli -- run -p deepseek "Say hi in 5 words."
```

Once that works, the rest of this guide covers calling tars from
**Python** and **Rust**.

## Install

### Python

```bash
git clone https://github.com/leocaolab/tars.git
cd tars/crates/tars-py
maturin develop --release
```

(Maturin produces a wheel that installs into the current Python
environment. Requires Rust 1.85+ and Python 3.10+.)

### Rust

Add to `Cargo.toml`:

```toml
[dependencies]
tars-pipeline = { git = "https://github.com/leocaolab/tars.git", tag = "v0.2.0" }
tars-provider = { git = "https://github.com/leocaolab/tars.git", tag = "v0.2.0" }
tars-types    = { git = "https://github.com/leocaolab/tars.git", tag = "v0.2.0" }
```

(Pre-1.0: pin to a specific tag. Each minor version may break.)

## Bootstrap config

```bash
cargo run -p tars-cli -- init
# writes ~/.tars/config.toml with starter providers
```

Then `export ANTHROPIC_API_KEY=...` (and/or `OPENAI_API_KEY`,
`GOOGLE_API_KEY`) — the config references env vars by name; secrets
don't go into the file.

See [`.env.example`](../.env.example) for the full env-var list.

## Three call shapes

### 1. Single completion

**Python**

```python
import tars

p = tars.Pipeline.from_default("anthropic")
resp = p.complete(
    model="claude-sonnet-4-5",
    system="You are a precise reviewer.",
    user="Find race conditions in this Rust function: ...",
    max_output_tokens=2000,
)

print(resp.text)
print(resp.usage)        # input/output/cached/thinking tokens
print(resp.telemetry)    # cache_hit, retry_count, layers, latency
```

`Pipeline.from_default` wraps the provider in the default middleware
stack (telemetry, cache, retry, optional validation, optional event
emitter). The raw `Provider` is also available if you want to manage
those concerns yourself:

```python
p = tars.Provider.from_default("anthropic")  # no middleware
```

**Rust**

The shortest path loads the same `~/.tars/config.toml` and goes through
`ProviderRegistry::from_config`. No `Pipeline.from_default` analogue
exists in Rust today — you stack middleware explicitly.

```rust
use std::sync::Arc;
use tars_config::Config;
use tars_pipeline::{Pipeline, TelemetryMiddleware, RetryMiddleware, LlmService};
use tars_provider::ProviderRegistry;
use tars_types::{ChatRequest, ModelHint, ProviderId, RequestContext};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load ~/.tars/config.toml and build providers from it.
    let cfg = Config::load_default()?;
    let registry = ProviderRegistry::from_config(&cfg.providers, /* … */)?;

    let provider = registry.get(&ProviderId::new("anthropic")).unwrap();

    // Wrap in the default middleware stack. Outermost layer is added first.
    let pipeline = Arc::new(
        Pipeline::builder(provider)
            .layer(TelemetryMiddleware::new())
            .layer(RetryMiddleware::default())
            .build()
    );

    let req = ChatRequest::user(
        ModelHint::Explicit("claude-sonnet-4-5".into()),
        "Find race conditions in this Rust function: ...",
    );
    let resp = pipeline.complete(req, RequestContext::test_default()).await?;

    println!("{}", resp.text);
    println!("{:?}", resp.usage);
    println!("{:?}", resp.telemetry);
    Ok(())
}
```

`RequestContext::test_default()` is a dev convenience — production code
constructs one carrying the real `tenant_id` / `principal_id` /
`trace_id` so the IAM and audit middleware have something to work with.

### 2. Multi-turn conversation

**Python**

```python
import tars

session = tars.Session.from_default(
    "anthropic",
    system="You are a code reviewer.",
)

r1 = session.send("Look at foo.py")
r2 = session.send("What's the worst issue?")  # remembers r1
r3 = session.send("How would you fix it?")    # remembers r1 + r2

print(session.history_version)  # bumps on each successful send
```

`Session` enforces conversation invariants (alternating user/assistant
messages, no orphan tool calls), trims history when it exceeds the
budget, and rolls back atomically if any send fails mid-turn.

**Rust**

```rust
use tars_runtime::Session;
use tars_types::{ModelHint, RequestContext};

let mut session = Session::new(
    pipeline.clone(),                          // from §1
    ModelHint::Explicit("claude-sonnet-4-5".into()),
    Some("You are a code reviewer.".into()),
);

let r1 = session.send("Look at foo.py", RequestContext::test_default()).await?;
let r2 = session.send("What's the worst issue?", RequestContext::test_default()).await?;
let r3 = session.send("How would you fix it?", RequestContext::test_default()).await?;

println!("history_version = {}", session.history_version());
```

Same invariants as the Python `Session` — alternating roles, no orphan
tool calls, atomic rollback on mid-turn failure.

### 3. Tool dispatch (auto-loop)

**Python**

```python
import tars

def fs_read_file(args):
    """Tool callable — receives parsed args, returns a JSON-able value."""
    with open(args["path"]) as f:
        return f.read()

session = tars.Session.from_default(
    "anthropic",
    system="Use the read_file tool to fetch source before reviewing.",
    tools=[
        tars.Tool(name="read_file", description="...", schema={...},
                  callable=fs_read_file),
    ],
)

resp = session.send("Review main.py")
# tars dispatches read_file → feeds result back to model → final reply
```

Tool registration is by `(name, callable, schema)`. Parallel tool
calls are batched into one `tool_result` message per protocol
requirements.

**Rust**

Rust tools implement the `Tool` trait from `tars-tools`. The built-in
`ReadFileTool` is a good template; for a custom tool you implement
`fn name() / fn spec() / async fn invoke()`.

```rust
use std::sync::Arc;
use tars_tools::builtins::ReadFileTool;

let mut session = Session::new(pipeline.clone(), model_hint, Some(system.into()));
session.register_tool(Arc::new(ReadFileTool::new(tempdir.path())));

let resp = session.send("Review main.py", RequestContext::test_default()).await?;
```

For a complete worked example covering Worker / Critic / Orchestrator
with real filesystem tools, see
[`crates/tars-runtime/examples/multi_step_with_tools.rs`](../crates/tars-runtime/examples/multi_step_with_tools.rs):

```bash
cargo run -p tars-runtime --example multi_step_with_tools
```

## Output validators

Attach Python callbacks that run after the model reply, before the
response reaches your code. Validators chain in order; each sees the
previous one's filtered output.

```python
def must_be_json(req, resp):
    try:
        json.loads(resp["text"])
        return tars.Pass()
    except ValueError as e:
        return tars.Reject(reason=str(e))

p = tars.Pipeline.from_default("anthropic", validators=[
    ("must_be_json", must_be_json),
])
```

Four outcome shapes:

- `tars.Pass()` — response unchanged, validator chain continues
- `tars.Reject(reason)` — response unacceptable, surfaces as
  `TarsProviderError(kind="validation_failed")`
- `tars.FilterText(text, dropped=[...])` — replace the response text
  (subsequent validators see the filtered version)
- `tars.Annotate(metrics={...})` — record per-call metrics for the
  validation summary

## Pre-flight capability check

Verify a role's configured provider supports its request shape *at
startup*, instead of failing on the first model call:

```python
roles = {
    "planner":  tars.CapabilityRequirements(requires_thinking=True),
    "executor": tars.CapabilityRequirements(requires_tools=True,
                                             estimated_max_output_tokens=8000),
}

for role, reqs in roles.items():
    p = tars.Pipeline.from_default(provider_for(role))
    r = p.check_capabilities(reqs)
    if not r:
        print(f"{role!r} can't satisfy: {[x.kind for x in r.reasons]}")
```

When routing has multiple candidates, incompatibility surfaces as
`TarsRoutingExhaustedError` with the full list of skipped candidates +
typed reasons, not a string-mashed error.

## Typed errors

```python
try:
    p.complete(model="...", user="...")
except tars.TarsRoutingExhaustedError as e:
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

Error classes branch on `e.kind`:

| `kind`                | Meaning                                       |
|-----------------------|-----------------------------------------------|
| `auth`                | API key invalid or missing                    |
| `rate_limited`        | Provider 429; check `e.retry_after`           |
| `network`             | Transient connectivity failure                |
| `parse`               | Provider returned malformed response          |
| `unknown_tool`        | Model called a tool that isn't registered     |
| `validation_failed`   | Output validator rejected (Permanent)         |
| `no_compatible_candidate` | All routing candidates failed pre-flight  |
| `context_too_long`    | Prompt exceeds model's context window         |
| ... (see Doc 01 for full list) ||

## Per-call observability

Every `Response` carries a `telemetry` block:

```python
print(r.telemetry.cache_hit)         # bool
print(r.telemetry.retry_count)       # 0 = first attempt succeeded
print(r.telemetry.layers)            # ["telemetry", "cache_lookup", ...]
print(r.telemetry.provider_latency_ms)
print(r.telemetry.pipeline_total_ms)
```

And, if validators ran, a `validation_summary`:

```python
print(r.validation_summary.validators_run)  # ["snippet_grounded"]
print(r.validation_summary.outcomes)         # {"snippet_grounded": {"outcome": "filter", "dropped": [...]}}
print(r.validation_summary.total_wall_ms)
```

For longer-term cross-call analysis, point the Pipeline at an event
store directory:

```python
p = tars.Pipeline.from_default(
    "anthropic",
    event_store_dir="~/.tars/events/",
)
```

Each call lands a `LlmCallFinished` row in the event store; full
request and response bodies go into a tenant-scoped CAS body store.
Inspect with the CLI:

```bash
tars events list --since 1d --tag dogfood
tars events show <event_id> --with-bodies
```

For trajectory inspection, live stderr streaming, JSON-mode logging,
and the layered "I want to debug X → look at Y" mapping, see
[`observability.md`](./observability.md).

For per-call cost caps, per-tenant budgets, provider fallback, and
rate-limit handling, see
[`recipes/cost-and-reliability.md`](./recipes/cost-and-reliability.md).

For offline batch processing (~50% pricing, 24h SLA) on Anthropic /
OpenAI, see [`recipes/batch.md`](./recipes/batch.md).

## When NOT to use tars

- **You only call one provider, one model, one prompt shape.** A
  thirty-line `requests.post(...)` is fine; tars's value compounds with
  scale (multiple providers, retries, cache, observability,
  multi-tenant). Below that, it's overhead.
- **You need a hosted dashboard / UI today.** tars is a runtime
  library; it gives you the data via the event store, but no UI.
  Pair it with a lightweight dashboard you build yourself, or wait
  for the eval framework + dashboard work in M9+.
- **You need streaming chat UI in the browser.** The `Pipeline.call`
  stream API works, but you're on your own for SSE proxying.
  v1.0 will ship an HTTP/SSE gateway (Doc 12); not before.
- **You want LangChain's ecosystem of pre-built chains.** tars is
  primitives, not a chain library. If you're adding "another LangChain
  example" you don't need tars.

## Where to go next

- **For deeper architecture** — [`architecture/00-overview.md`](./architecture/00-overview.md)
- **For API details by layer** — pick the relevant `architecture/NN-*.md`
- **For competitive comparison** — [`comparison.md`](./comparison.md)
- **For "what was the thinking behind X"** — [`audit-stories/`](./audit-stories/)
