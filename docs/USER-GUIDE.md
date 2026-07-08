# tars User Guide

A 5-minute orientation for developers who want to call tars from their
own code. Covers the three common call shapes + when tars is the wrong
tool. For *why* tars is shaped this way, jump to
[`architecture/`](./architecture/) — most callers don't need to.

> **Stability**: as of **v1.0** the public API follows SemVer — breaking
> changes land on major bumps, not minors. The shapes shown here are
> current; track the [Releases page](../../../releases) for changes.

---

## What tars is

A Rust-first multi-provider LLM runtime: one trait + one middleware
stack covers Anthropic, OpenAI, Gemini, DeepSeek, vLLM, MLX, llama.cpp,
and three CLI-based subscription providers (`claude_cli`, `gemini_cli`,
`codex_cli`). Python bindings ship as a wheel and Node/TypeScript
bindings as a native addon; you can also use it directly from Rust.

What you get without writing it yourself:

- **Provider abstraction** — swap models without touching call sites
- **Middleware pipeline** — telemetry, cache, retry, output validation,
  pipeline event store, all engaged automatically by the default
  `Pipeline`
- **Agent abstraction** — hand a `Task` to a capability set (`TarsAgent`
  drives the tool loop; `EnsembleAgent` hedges one task across N agents)
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
# 1. Build + write a starter config to $TARS_HOME/config.toml (default ~/.tars).
cargo run -p tars-cli -- init

# 2. Set the credential the starter config references.
export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY / GOOGLE_API_KEY

# 3. Send one prompt.
cargo run -p tars-cli -- run -p anthropic "Say hi in 5 words."
```

Expected: the model's reply on stdout, a one-line `usage:` summary on
stderr. If you see `error in tars run:` instead, double-check the env
var name matches what `$TARS_HOME/config.toml` (default `~/.tars`) declares
for that provider.

Want a different provider? Replace `-p anthropic` with one of the ids
in `$TARS_HOME/config.toml` (default `~/.tars`) (`-p openai_main`, `-p claude_cli`, etc.).
For the long-lived subscription path, see
[`providers/claude-cli.md`](./providers/claude-cli.md).

Built-in providers (available without writing any config — just export
the key): `openai`, `anthropic`, `gemini`, `deepseek`, plus the local /
subscription backends (`claude_cli`, `gemini_cli`, `codex_cli`,
`opencode`, `antigravity`, `mlx`, `llamacpp`, `vllm`). Any
OpenAI-compatible endpoint (Groq, xAI, OpenRouter, LM Studio, Ollama, …)
works via `type = "openai_compat"` + `base_url`. For keyless cloud, build
with `--features bedrock` and use `type = "bedrock"` (region + model, no
key — authed via the AWS credential chain). The CLI delegates
(`claude_cli`/`gemini_cli`/`codex_cli`/`opencode`/`antigravity`) run the
vendor's own agent as a black box, each write-jailed to the worktree by
the tars OS sandbox. DeepSeek is reached via its OpenAI-compatible API
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
tars-pipeline = { git = "https://github.com/leocaolab/tars.git", tag = "v0.4.0" }
tars-provider = { git = "https://github.com/leocaolab/tars.git", tag = "v0.4.0" }
tars-types    = { git = "https://github.com/leocaolab/tars.git", tag = "v0.4.0" }
```

(Pin to a tag for reproducibility; the API follows SemVer from v1.0.)

## Bootstrap config

```bash
cargo run -p tars-cli -- init
# writes $TARS_HOME/config.toml (default ~/.tars) with starter providers
```

tars reads its global config from **`$TARS_HOME/config.toml`** —
`$TARS_HOME` resolves as `--tars_home` flag > `$TARS_HOME` env var >
`~/.tars` (the default). The providers declared there are global: shared by
every tars consumer/tool. Each provider's API key is read from the env var
its `api_key_env` names — optionally loaded from `$TARS_HOME/.env` — and is
never stored in the config file itself.

Then `export ANTHROPIC_API_KEY=...` (and/or `OPENAI_API_KEY`,
`GOOGLE_API_KEY`) — the config references env vars by name; secrets
don't go into the file.

See [`.env.example`](../.env.example) for the full env-var list.

## Checking your setup — `tars providers` / `tars models`

Two read-only commands answer "is my config wired up, and which models
can I actually ask for?" Both resolve config from
**`$TARS_HOME/config.toml`** (override with `--config <path>`) and
best-effort load `$TARS_HOME/.env` first, so env-var-backed keys resolve
without pre-exporting them. Neither ever prints a secret.

### `tars providers` — configured providers + key health

```bash
tars providers            # name, type, default_model, key-env health
tars providers --check    # + a fast reachability probe per HTTP provider
tars providers --json     # machine-readable envelope
```

For every provider in your config it prints the `type`, the configured
`default_model`, and how its auth resolves — for an env-backed key,
**which** env var and whether it is currently **set** (`(set)` /
`(UNSET)`), never the value. Keyless local servers show `auth: none`;
subscription CLIs show `auth: delegated to tool login`. With `--check`
it also fires the same list-models GET as `tars models --live` and
reports `reachable` / `auth failed (HTTP 401)` / `unreachable` /
`no list API (CLI/bedrock/mock)` — bounded by a short timeout so a dead
local server can't hang the command.

### The model library — `tars models`

The **model library** is a JSON catalog at **`$TARS_HOME/models.json`**
recording, per provider, the model ids that provider's API last
reported. It's tars-owned state alongside `config.toml`.

```bash
tars models                 # QUERY the library (fast, offline) for every provider
tars models gemini_flash    # just one provider
tars models --live          # bypass the library, hit the provider APIs now
tars models --json          # machine-readable envelope

tars models update          # UPDATE the library from the live APIs, for all providers
tars models update openai   # refresh one provider
```

- **`tars models`** reads the library — fast and offline. Each provider
  row marks the configured `default_model` (`← default`), and flags it
  with `⚠ default not in list (stale config?)` if that default is not in
  the last-seen live list. If the library is empty/missing it tells you
  to run `tars models update`. `--live` skips the cache and queries the
  APIs directly.
- **`tars models update`** queries every selected provider live,
  persists the result, and reports what **changed** since last time
  (`+ added` / `- removed (deprecated/retired)`). If a configured
  `default_model` is no longer in the provider's live list it prints a
  **stale-config warning** — it never edits your config, only reports.
  A single-provider update merges into the existing library without
  dropping the other providers' rows.

### Which provider types are queryable

Model discovery is an HTTP list-models call, so it only works for
providers that expose one:

| Provider type | Queryable? | Endpoint / note |
|---|---|---|
| `gemini` | ✅ | `…/v1beta/models` (`?key=`) |
| `openai` | ✅ | `…/models` (Bearer) |
| `openai_compat` | ✅ | `{base_url}/models` (Bearer, key optional) |
| `vllm` / `mlx` / `llamacpp` | ✅ | local `…/models` (keyless OK) |
| `anthropic` | ✅ | `…/v1/models` (`x-api-key` + `anthropic-version`) |
| `bedrock` | — | model list is an AWS SDK (SigV4) call, not queried here |
| `claude_cli` / `gemini_cli` / `codex_cli` / `opencode` / `antigravity` | — | models via the tool's own login |
| `mock` / `cassette` | — | internal test providers |

A non-queryable provider is listed with the reason, not silently
dropped. When a provider needs a key whose env var is unset, the row
carries the **var name to export** (e.g. `no key: set $GEMINI_API_KEY`),
never a sentinel.

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

The shortest path loads the same `$TARS_HOME/config.toml` (default `~/.tars`) and goes through
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
    // Load $TARS_HOME/config.toml (default ~/.tars) and build providers from it.
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

## Decoding a structured response

When you asked the model for JSON, `resp.text` is a string you still have
to parse — and *how* you parse it depends on how the provider produced it.
tars gives you one seam that gets this right: `tars-types::json_decode`
(`decode` / `decode_json` / `ChatResponse::json`). It handles the two
failure modes that bite hand-rolled `serde_json::from_str`: providers that
wrap JSON in a ```` ```json ```` fence or chatty prose, and models that
emit an out-of-range integer.

**The strong type is yours; tars is a generic engine — you hand it a `T`,
it hands you back a `T`.** tars never learns your type or your envelope
tag; those live only in your crate. And because it returns *either* a
valid `T` *or* a typed `TarsJsonError`, you cannot end up holding an
ill-formed `T`: the type is the contract (*parse, don't validate*).

The strategy is keyed off the `StructuredOutputMode` the request used
(from the provider's `Capabilities`), so the layer that knows how the
response was produced tells the decoder how to read it:

| Mode | Meaning | Decode strategy |
|------|---------|-----------------|
| `StrictSchema` / `JsonObjectMode` | provider guarantees a clean JSON document | parse `text` directly; a fenced/chatty body is a *broken promise* → `InvalidJson`, never a silent scrape |
| `None` / `ToolUseEmulation` | `text` may be chatty prose with JSON embedded | strip the code fence, scan for the first balanced `{…}` / `[…]`, parse that |

**`ChatResponse::json` — the common case:**

```rust
use serde::Deserialize;
use tars_types::StructuredOutputMode;

#[derive(Deserialize)]
struct Review { severity: u8, summary: String }

// `mode` is whatever the request/provider used.
let review: Review = resp.json::<Review>(StructuredOutputMode::JsonObjectMode)?;
```

`decode_json::<T>(text, mode)` is the same thing when you only have the
text; `resp.json` is a thin wrapper over it.

**`decode` — envelope tags + integer clamp.** Use the full `decode` when
the model wraps its JSON in a declared envelope tag, or when you need the
lossy integer-clamp recovery. A response type opts into unwrapping by
implementing `JsonAgentResponse` and listing its tags — tried in order,
first match wins; brackets optional (`"<report>"` ≡ `"report"`). List a
new tag first and legacy aliases after to accept both. Empty (the default)
means bare JSON.

```rust
use tars_types::{decode, DecodeOpts, JsonAgentResponse};

#[derive(Deserialize)]
struct FixReport { id: i64, changed: Vec<String> }

impl JsonAgentResponse for FixReport {
    fn wrapper_tags() -> &'static [&'static str] { &["<fix_report>", "<report>"] }
}

// Extracts the <fix_report>…</fix_report> block, then decodes.
// DecodeOpts::clamping() opts into clamping any integer above i64::MAX
// down to i64::MAX (off by default — a lossy recovery for a bogus id).
let report: FixReport = decode(&resp.text, mode, DecodeOpts::clamping())?;
```

**Then stage two — strong type → domain.** `decode` gives you the *wire
mirror* (a serde type shaped like what the model emits). Your own transform
turns it into domain values (filter to known ids, split tags, fold …) —
plain code over a type you already trust, no more JSON in sight:

```rust
let replies: HashMap<String, FixReply> = parse_fix_report_domain(report, known_ids)?;
```

**A different agent is a different type — the call doesn't change.** Each
consumer response is its own serde type + a three-line `impl
JsonAgentResponse` naming its tags; the `decode::<T>` call site is
identical:

```rust
// critic: reply may arrive as an array, a dict, or a flat object
let wire: CriticWire = decode(&resp.text, mode, DecodeOpts::clamping())?;
let findings = wire.into_flat_findings();               // stage two
```

**Error taxonomy** (`TarsJsonError`) — the failure tells you *which* stage
broke, so you branch on the variant, not a substring:

| Variant | Meaning |
|---------|---------|
| `EmptyStream` | no assistant text to decode (e.g. a tool-only turn) |
| `MissingBlock { tried }` | declared envelope tags, none found in the text |
| `NoJsonObject { attempts }` | chatty scan found no balanced JSON value |
| `InvalidJson` | text wasn't valid JSON (in strict mode: a violated "clean JSON" promise) |
| `Schema` | valid JSON, but the wrong shape for `T` |

`JsonValueType` is a Python-named JSON type tag (`dict` / `list` / `int` /
…) if you want to write your own "expected an object, got a list" message.

**Consuming the seam from another repo.** Point your `tars-types`
dependency at the branch (don't pin a rev), and use a local `[patch]` while
you hack on tars itself — edit and verify without pushing each time:

```toml
[dependencies]
tars-types = { git = "https://github.com/leocaolab/tars", branch = "result-side-json-decode" }

# while iterating on tars locally, redirect to your checkout:
[patch."https://github.com/leocaolab/tars"]
tars-types = { path = "../tars/crates/tars-types" }
```

In one line: a local strong type is *your* serde type + a three-line `impl
JsonAgentResponse` + one `decode::<T>` call. The type is yours; the
mechanism is tars's.

### Validating a schema from Python

The `decode::<T>` seam above is **Rust-side only** — it's parametric over a
Rust type and has no cross-FFI analogue. Python's strong typing is a
*runtime* concern, and there are two current, complementary ways to get it:

1. **Enforce at decode time (first choice).** Pass the JSON Schema as the
   `response_schema=` kwarg on `complete`. For a strict-capable provider the
   model is *forced* to emit conforming JSON (`StrictSchema` mode), so
   `resp.text` is clean by construction. `response_schema_strict=False`
   makes the schema a hint rather than a hard constraint.

   ```python
   resp = p.complete(
       model="claude-sonnet-4-5",
       user="Rate this diff.",
       response_schema={
           "type": "object",
           "properties": {"severity": {"type": "integer"}, "summary": {"type": "string"}},
           "required": ["severity", "summary"],
       },
   )
   data = json.loads(resp.text)          # clean JSON — parse straight through
   review = Review.model_validate(data)  # …into your pydantic model, if you use one
   ```

2. **Validate post-hoc with an output validator (defense in depth).** Attach
   a Python callable via the `validators=` kwarg; it runs inside the
   pipeline and can `Reject` a bad response (which `RetryMiddleware` will
   retry) or `Annotate` it. There is **no** built-in schema validator on the
   Python side — you write the check with the `jsonschema` PyPI package (or
   pydantic) and return a typed outcome:

   ```python
   import json, jsonschema, tars

   SCHEMA = {"type": "object", "required": ["severity", "summary"]}

   def validate_schema(req, resp):
       try:
           data = json.loads(resp.text)
           jsonschema.validate(data, SCHEMA)
       except (json.JSONDecodeError, jsonschema.ValidationError) as e:
           return tars.Reject(reason=str(e))   # raw error carried out, not a sentinel
       return tars.Pass()

   p = tars.Pipeline.from_default("anthropic", validators=[("schema", validate_schema)])
   ```

   See [Output validators](#output-validators) below for the full outcome
   vocabulary (`Pass` / `Reject` / `FilterText` / `Annotate`).

Node/`tars-node` follows the same shape: `responseSchema` on the completion
options for decode-time enforcement, then `JSON.parse(result.text)` into
your own TS type. The mode-aware fence-scrape of `decode` isn't bound to
either runtime yet — see the CHANGELOG's v0.8.0 entry for the rationale.

## A/B testing — two axes, and pinning the LLM

tars frames A/B on **two axes** (Doc 18 §5a); getting a strong-typed,
schema-valid result (above) is *not* one of them — it's the input you then
A/B over:

| Axis | What varies | What's pinned | Diff | Samples |
|------|-------------|---------------|------|---------|
| **LLM-change** | prompt / model / dataset | the code | behavioral, **statistical** | many (for significance) |
| **Code-change** | the code (refactor, rewrite) | **the LLM** | **exact / deterministic** | **one** |

**Code-change axis — pin the LLM with a cassette.** "Did this refactor
change observable behavior?" is unanswerable if the LLM is stochastic —
model noise swamps the code delta. So pin it: record a cassette once, then
run code variant A vs B against the *same replayed responses*. The only
thing that moved is your code, so the diff is exact and one sample suffices.

```python
# Both arms replay the SAME pinned completion (examples/tars.toml cassette),
# so the difference is pure code — the regression question.
pipe = tars.Pipeline.from_config("examples/tars.toml", "cassette_schema")
review = json.loads(pipe.complete(model=MODEL, system=SYS, user=USER).text)

a = bucket_v_a(review["severity"])   # old code
b = bucket_v_b(review["severity"])   # refactored code
if a != b:
    print(f"behavior changed: {a!r} → {b!r}")   # a regression a text diff won't show
```

Runnable: [`examples/python/ab-testing/code_change_ab.py`](../examples/python/ab-testing/code_change_ab.py).

**LLM-change axis — vary the prompt/model, diff behavior statistically.**
Here the code is fixed and you compare two configs over a fixed corpus.
Because outcomes are **paired** (same corpus through both), the correct
test is **McNemar** on the discordant cells — *not* two overlapping
confidence intervals. Tag each cohort so the event store can split them
(`RequestContext::with_tags([...])` in Rust; the `tags=` kwarg on
`complete` in Python/Node), then compare with `tars eval diff`. The full
methodology (McNemar, paired bootstrap, precision/recall) lives in
[eval-methodology.md](eval-methodology.md) and [Doc 18](architecture/18-agent-testing.md).

### Reading a run + diff — and where the tooling stops

`tars eval run` writes a run directory: a `manifest.json` (per-case status,
tokens, latency, check outcomes) plus per-case `output.txt` / `report.json`.
`tars eval diff <baseline> <candidate>` then reports, in tiers:

```
operational:
  cases / errors / tokens in-out        plain deltas (=, +N, -N)
  latency p50            34420ms → 0ms  (-100%)
checks (violation rate):
  json_shape                   0.0% → 12.5%  (+12.5%)   ← a check got worse
trajectory (--trajectory):
  divergence   30.8% (12/39 cases differ)   diverging: case_003, case_011, …
  McNemar (trajectory-match): regressed b=2 improved c=7 χ²=2.78 → NOT significant at α=0.05
quality (if you ran `tars eval judge`):
  precision / recall deltas with Wilson CIs
```

`--json` emits the same as one machine-readable object.

**How to drill down.** The diff hands you the *coordinates*, not the cause:
1. a check-rate or divergence delta tells you **what** moved;
2. `diverging:` names **which** cases (paired by id);
3. open those cases' `report.json` — each failed check carries a required
   `reason`; compare the two runs' `output.txt` / `tool_trajectory`;
4. McNemar tells you whether the change is **signal or noise**.

**Where it stops — be clear-eyed.** `eval diff` is a *localizer + statistician*.
It does **not** write a narrative report, interpret the delta into a
conclusion, or find root cause — that last mile is human. (`tars eval judge`
adds per-case correctness verdicts *with* the judge's rationale, but that
explains whether an output is *right*, not *why the diff happened*.) An
automated "why did B regress on case_003" analysis would be a consumer-layer
LLM pass over `eval diff --json` + the diverging cases — a natural use of the
[decode seam](#decoding-a-structured-response), but it is **not built in**.

### Freeze it as a test (py / ts / rs), not a CLI run

Once a cassette is recorded, the comparison is just a deterministic function
call — so pin it in your normal test runner instead of the `tars eval` CLI.
Point a test at a cassette provider (committed cassette = the fixture) and
assert; no live model, so it runs in CI. The request **fingerprint is
binding-agnostic** — one cassette recorded from Python replays byte-identically
in Node *and* Rust:

```python
# pytest — crates/tars-py/python/tests/test_ab_cassette.py
def test_severity_bucket_snapshot():
    pipe = tars.Pipeline.from_config("examples/tars.toml", "cassette_schema")
    severity = json.loads(pipe.complete(model=MODEL, system=SYS, user=USER,
                                        max_output_tokens=200).text)["severity"]
    assert bucket(severity) == "high"   # refactor changes this → fails → you bless
```

```rust
// #[tokio::test] — crates/tars-provider/tests/ab_cassette.rs
let provider = CassetteProvider::replay_from_file("cassette_schema", &cassette_path())?;
let review: Review = tars_types::decode_json(&replay_text().await, StructuredOutputMode::None)?;
assert_eq!(review.severity, 8);          // replays the SAME cassette the py test uses
```

Node mirrors this with `node --test` (`crates/tars-node/__test__/ab_cassette.test.mjs`).
A Python test that isn't marked `requires_provider` runs everywhere (conftest
only skips the live ones) — the cassette test is exactly that.

### Blessing a change

When a diff or a snapshot test goes red for an **intended** change, you
"bless" it — accept the new output as the reference. As of **v0.9.0** a bless is
a first-class, committed file of field-level assertions (Doc 28):

```rust
// load a committed bless over the (cassette-pinned) decoded reply → pass/drift
let outcome = tars_types::Bless::load(&path)?.check(&value)?;   // rs
// or the approval assert: TARS_BLESS=1 captures/updates, else loads + checks
Bless::check_or_bless(&path, &value, &["$.severity"], None, do_bless)?;
```

```python
r = tars.bless_check("severity.bless.json", resp.text)   # py  → {"passed", "drifts"}
```
```ts
const r = blessCheck("severity.bless.json", result.text); // ts → {passed, drifts}
```

Over an eval run: `tars eval bless <run> --select '$.severity' --accept` captures
per-case references; `tars eval bless <run>` checks and bails on drift. Blessing
is still *regenerate the fixture + commit* — the git diff of the `.bless.json` is
the review surface, and a capture always stages a `.new` before it can clobber a
committed file:

- **The model's reply changed** (new model/prompt, or you re-recorded): bless
  by re-recording — `TARS_CASSETTE_RECORD=1 …` — and commit the new
  `*.cassette.json`. Reviewers see exactly which replies moved.
- **Behavior/threshold changed** (a refactor you meant): bless by updating the
  asserted expected value in the test, or promote the candidate run to the
  baseline dir (`benchmarks/baselines/eval/<model>/` — a manual `cp`, the
  convention `tars eval diff` compares against).

The discipline: a bless is a **reviewable commit**, never a silent overwrite —
so an unintended drift can't slip through as an "accepted" snapshot.

## Built-in web tools

Two built-in tools give an agent live web access. Both are thin adapters over
[`sisurf-core`](https://github.com/leocaolab/sisurf) — the browsing engine
(fetch, browser escalation, distillation, result parsing) lives there; tars only
validates the args, calls the one sisurf primitive, and maps its typed result
into a `ToolResult`.

| Tool | In → out |
|---|---|
| **`web.fetch`** | `url` → the page's main content as clean **Markdown**, plus a provenance header (final URL + which **tier** served it: `static` reqwest fetch vs. `browser` Chromium render). Use to READ a page you already have a URL for. If a page needs JavaScript and no headless browser can be launched, you get a legible, actionable `NoBrowser` error ("install Chrome, or fetch a URL that serves without client-side JS"), not an opaque failure. |
| **`web.search`** | `query` → a numbered `title / url / snippet` list. Use to DISCOVER URLs, then follow up with `web.fetch`. Backend is chosen by config (see below). |

Both are network, long-running `web.*` ops, so they route through the same
approval gate as `bash.run`: a policy that marks them `Ask` / `Deny` sends them
through human approval by tool name — no extra wiring.

### `[web_search]` config

`web.search` defaults to **DuckDuckGo** (`ddg`) — no key, works out of the box.
To use a keyed backend, add a `[web_search]` section to `$TARS_HOME/config.toml` (default `~/.tars`).
The schema is **owned by sisurf** (`SearchConfig`); tars deserializes the section
into it and injects the key — it does not redeclare the schema.

```toml
[web_search]
backend = "google_cse"            # ddg | google_cse | brave
google_cse = { cx = "your-cx-id" } # the programmable-search-engine id; NOT the secret
```

The **API key is never written to the config file.** tars resolves it from a
conventional environment variable — the same posture as a provider's
`api_key_env` — and injects it at load time:

| Backend | Config section | Key env var |
|---|---|---|
| `ddg` | *(none)* | *(none — keyless)* |
| `google_cse` | `[web_search] google_cse = { cx = "…" }` | `GOOGLE_CSE_KEY` |
| `brave` | `[web_search] brave = { }` | `BRAVE_API_KEY` |

If the env var is missing or blank, the key stays empty on purpose:
`SearchConfig::build()` then typed-fails with `MissingApiKey`, which `web.search`
surfaces as a legible tool error **before any network call** — it never silently
falls back to a different backend.

## Agents — hand a task to a capability set

The three shapes above are *calls*. An **Agent** is one level up: a set of
capabilities (skills) you give a **task** to, and it does the work — driving
its own multi-turn tool loop internally. The contract is `tars-model`'s
`trait Agent { id, role, skills, run(task) }`; see
[architecture/20-agent-abstraction.md](architecture/20-agent-abstraction.md).

A **native** agent is LLM-backed — it turns the task into prompts and runs a
tool loop over a *pure-inference* provider. Swap the provider and the same
agent is a "gemini agent" or a "claude_cli agent"; tars owns the loop +
tools + working dir (white box), not the CLI's internal black box.

```rust
use std::sync::Arc;
use tars_model::{Agent, AgentContext, Skill, SkillSet, Task, TaskId};
use tars_runtime::TarsAgent;
use tars_tools::{builtins::{EditFileTool, WriteFileTool, BashTool}, ToolRegistry};

// 1. The capabilities (concrete tools), jailed to the worktree.
let mut reg = ToolRegistry::new();
reg.register_owned(WriteFileTool::with_root(&worktree).unwrap()).unwrap();
reg.register_owned(EditFileTool::with_root(&worktree).unwrap()).unwrap();
reg.register_owned(BashTool::new()).unwrap();

// 2. Assemble the agent over a pure-inference provider (`llm`).
let agent = TarsAgent::new(
    "agent:fixer", "fix",
    SkillSet::new()
        .with(Skill::new("fs.write_file", "write files"))
        .with(Skill::new("fs.edit_file", "edit files"))
        .with(Skill::new("bash.run", "run commands")),
    "claude-sonnet-4-5", llm, Arc::new(reg),
);

// 3. Hand it a task. cwd scopes where its tools act.
let task = Task::new(TaskId::new("t1"), "fix the failing test in src/foo.rs");
let ctx = AgentContext::new().with_cwd(&worktree);
let out = agent.run(task, ctx).await?;
println!("{}", out.summary);
```

**Hedge across agents** — run one task on several agents, take the first
success (tail-latency hedge at *task* granularity):

```rust
use tars_runtime::EnsembleAgent;
use tars_model::AgentRole;

let ens = EnsembleAgent::new(
    "ens:fix", AgentRole::worker("fix"),
    vec![claude_cli_agent, gemini_agent, user_agent],
);
let out = ens.run(task, ctx).await?; // first to succeed wins; the rest are cancelled
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

## Durable agent jobs (`tars-durable`)

The default DAG runner (`tars_runtime::run_plan`) executes a plan of
agent steps to completion **in one `await`, entirely in memory** — great
for a short, side-effect-free run, but the whole frontier is lost if the
process dies mid-run. The new **`tars-durable`** crate adds a durable
execution layer for work that must survive a restart without re-paying
for what already finished.

The core idea: **the persisted step-result store IS the checkpoint.**

- **Durable async jobs.** An agent task's DAG runs as a job whose state
  and per-step answers are persisted as it goes, in the runtime's own
  always-on sqlite store (`answers` + an append-only `result_events`
  log + a `jobs` status table), each step checkpointed through **one**
  atomic transaction.
- **Resume = memoized re-run.** On restart the DAG is simply re-driven:
  a step whose answer is already in the store is skipped — **the LLM is
  never re-called** — and only un-done steps execute. Resume falls out
  of the model; there's no in-memory frontier to reconstruct.
- **Critical invariant — correctness never depends on observability.**
  The durability store is separate from, and independent of, the
  **off-able** event/telemetry store (`StoreScope::Off`,
  `ARC_TARS_EVENTS_OFF`, …). With observability events fully off, a
  job's answers and state still persist and it still resumes. Event
  logs stay a diagnosis convenience, never the status-of-record.

**Status: M0 + M1 shipped** — the durable checkpoint store (AnswerStore
+ atomic commit) and the DB-driven memoized-re-run scheduler
(`DurableScheduler`). **M2–M5 are pending**: the JobManager +
reconcile-on-open + persisted cancel, the at-least-once delivery outbox,
the ephemeral streaming event bus, and the end-to-end app wiring.

Full design (CUJs, the outbox two-bit rule, effect classification, the
reuse map, the roadmap): [`design/durable-agent-runtime.md`](./design/durable-agent-runtime.md).

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
