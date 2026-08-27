# tars User Guide

## What tars is

A Rust library for **calling a language model**, with the parts you would
otherwise write yourself: a dozen providers behind one trait, a middleware
pipeline, typed errors, and a record of every call that you can read afterwards.

It is a caller, not a framework. There is no agent loop, no planner, no
orchestrator. **You write the control flow.** When the model asks for a tool,
you dispatch it and hand the result back — because the loop is where your
program's logic lives, and a library that owns it owns your program.

What that buys you is that everything below is a plain function call you can
put a breakpoint in.

## Hello, tars (5-minute path)

```bash
# 1. Build + write a starter config to $TARS_HOME/config.toml (default ~/.tars).
cargo run -p tars-cli -- init

# 2. Set the credential the starter config references.
export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY / GEMINI_API_KEY

# 3. Check that it resolved — key found, model library loaded.
cargo run -p tars-cli -- providers
```

`tars providers` is the first thing to run and the first thing to check when
something is wrong: it says which providers are configured, whether each one's
credential actually resolves, and what its default model is. A missing env var
shows up here rather than as a 401 three layers down.

Then send a prompt from Rust or Python — see **Three call shapes** below.

There is no `tars run`. The CLI's job is to *inspect* (providers, models,
events, trajectories, eval); sending a prompt is what the library is for, and
if you only want one completion from a shell, `curl` is already installed.

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

Two, now — a completion, and a completion with a tool in the middle.


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

The shortest path installs the global config from the same
`$TARS_HOME/config.toml` (default `~/.tars`) via
`tars_handle::init_from_home`, then pulls the process-wide
`ProviderRegistry`. There's no Rust `Pipeline.from_default` — you stack
middleware explicitly with `LlmService::builder`, or assemble the canonical
onion with `LlmService::default_chain`.

```rust
use futures::StreamExt;
use tars_pipeline::{LlmService, RetryMiddleware, TelemetryMiddleware};
use tars_provider::ProviderRegistry;
use tars_types::{ChatRequest, ChatResponseBuilder, ProviderId, RequestContext};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Composition root: install the global config from $TARS_HOME/config.toml
    // (default ~/.tars) and eagerly build the one provider registry.
    tars_handle::init_from_home(None)?;
    let registry = ProviderRegistry::global()?;

    let provider = registry.get(&ProviderId::new("anthropic")).unwrap();

    // LlmService = provider + one bound model + a middleware chain.
    // Outermost layer is added first.
    let svc = LlmService::builder(provider, "claude-sonnet-4-5")
        .layer(TelemetryMiddleware::new())
        .layer(RetryMiddleware::default())
        .build();

    // The request is pure content — the model already lives on the service.
    let req = ChatRequest::user("Find race conditions in this Rust function: ...");

    // `LlmService::call` streams events; aggregate them into a ChatResponse.
    let ctx = RequestContext::test_default();
    let mut stream = svc.call(req, ctx.clone()).await?;
    let mut acc = ChatResponseBuilder::new();
    while let Some(ev) = stream.next().await {
        acc.apply(ev?);
    }
    let resp = acc.finish();

    println!("{}", resp.text);
    println!("{:?}", resp.usage);
    // Per-call telemetry (cache_hit, retry_count, layer trace, latency)
    // accumulates on `ctx.telemetry`.
    Ok(())
}
```

`RequestContext::test_default()` is a dev convenience — production code
constructs one carrying the real `tenant_id` / `principal_id` /
`trace_id` so the IAM and audit middleware have something to work with.

### 2. Tools — the model asks, you dispatch

There is no auto-loop. `ToolRegistry::dispatch` runs one tool call and hands
back a `Message` you append and send again. Three lines, and the branch where
you decide whether to run it at all is yours:

```rust
let mut msgs = vec![Message::user("Review src/main.rs")];
loop {
    let resp = pipeline.complete(ChatRequest::new(msgs.clone()).with_tools(specs.clone()), ctx.clone()).await?;
    let Some(call) = resp.tool_calls.first() else { break resp };
    msgs.push(resp.into_assistant_message());
    msgs.push(registry.dispatch(call, tool_ctx.clone()).await);
}
```

Every built-in that touches the machine — `bash.run`, `write_file`,
`edit_file`, `web.fetch` — goes through an **approval sink** first, and the
CLI-delegate providers run inside an OS write-jail (macOS Seatbelt, Linux
bubblewrap): the worktree, `$TMPDIR` and the delegate's own state dir are
writable, everything else including `.git` is read-only. There is no unconfined
path; `--sandbox danger-full-access` is the explicit opt-out and it is spelled
that way on purpose.

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
the `tars-eval` crate — its judge carries an anti-incest guard, and
McNemar is the test for "did this change help" on paired items.

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

When a caller composes several candidate services (its own fallback /
ensemble) and every one fails its pre-flight, that exhaustion surfaces as
`TarsRoutingExhaustedError` (mapped from `ProviderError::NoCompatibleCandidate`)
with the full list of skipped candidates + typed reasons, not a
string-mashed error.

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
| `no_compatible_candidate` | All caller-composed candidates failed pre-flight |
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

- **One provider, one model, one prompt shape.** A thirty-line
  `requests.post(...)` is fine. The value here compounds with scale —
  several providers, retries, a cache, a record you can read afterwards —
  and below that it is overhead you are paying for nothing.
- **You want an agent framework.** There is no loop here, and that is on
  purpose. If you want something that plans and decides, you want a
  different library, or you want to write the loop yourself and call this
  one from inside it.
- **You want a hosted dashboard.** The event stores are SQLite files on
  your disk and the CLI reads them; there is no UI and none is planned in
  this repo.
- **You want a chain library.** These are primitives. If what you are
  writing is "another LangChain example", you do not need tars.
- **You want streaming into a browser.** `LlmService::call` gives you the
  stream; SSE proxying is yours.

## Where to go next

- **One page per provider** — [`providers/`](./providers/): auth, models,
  and the quirks each one actually has.
- **Batching, retries, budgets** — [`recipes/`](./recipes/).
- **Seeing what a run did** — [`observability.md`](./observability.md).
- **Where does this code belong?** — each crate's `README.md` is a
  placement contract: its role, its effect budget, the dependencies it may
  and may not take, and the one reason it should ever change.
- **What actually shipped** — [CHANGELOG.md](../CHANGELOG.md). If a doc
  here disagrees with the code, the code is right and the doc is a bug.
