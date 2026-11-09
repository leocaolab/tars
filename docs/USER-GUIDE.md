# tars User Guide

How to call tars from your own code: set it up, check the setup, make a call
from Rust or Node, decode the reply, and see what happened. For the crate
layout, see the [README](../README.md).

> **Stability**: tars follows SemVer — breaking changes land on major
> versions, not minors. [CHANGELOG.md](../CHANGELOG.md) lists them.

---

## What tars is

A Rust library for calling language models: one trait and one middleware
stack cover Anthropic, OpenAI, Gemini, DeepSeek, vLLM, MLX, llama.cpp, any
OpenAI-compatible endpoint, and the subscription CLIs (`claude_cli`,
`codex_cli`, `opencode`, `antigravity`). Node/TypeScript bindings ship as a
native addon.

What you get without writing it yourself:

- **Provider abstraction** — swap models without touching call sites
- **Middleware pipeline** — telemetry and retry in the default chain, plus
  output validation when you give it validators and the call log when you
  give it a sink
- **Capability pre-flight** — every request is checked against the
  provider's profile (tools, thinking, vision, structured output, context
  window) before any network call; you can also check at startup
- **Typed errors and a decode seam** — branch on an error's kind, decode a
  reply into your own type
- **A readable record** — one Parquet `llm_call` row per call, read back with
  `tars events`

tars is a caller, not an agent framework: the loop that decides what to call
next is yours.

## Hello, tars (5-minute path)

If you just want to confirm tars works on your machine, these three
commands are the shortest route to a successful LLM call. No code — just
shell.

```bash
# 1. Build + write a starter config to $TARS_HOME/config.toml (default ~/.tars).
cargo run -p tars-cli --bin tars -- init

# 2. Set the credential the starter config references.
export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY / GOOGLE_API_KEY

# 3. Send one prompt.
cargo run -p tars-cli --bin tars -- probe anthropic --tenant me --prompt "Say hi in 5 words."
```

Expected: every event the provider streams back, the reply among them. If
it fails with a credential error instead, double-check the env
var name matches what `$TARS_HOME/config.toml` (default `~/.tars`) declares
for that provider.

Want a different provider? Replace `anthropic` with one of the ids
in `$TARS_HOME/config.toml` (default `~/.tars`), e.g. `probe openai_main …` or
`probe claude_cli …`; the provider id is the positional argument, and `-p` is
short for `--prompt`.
For the long-lived subscription path, see
[`providers/claude-cli.md`](./providers/claude-cli.md).

Built-in providers (available without writing any config — just export
the key): `openai`, `anthropic`, `gemini`, `deepseek`, plus the local /
subscription backends (`claude_cli`, `codex_cli`,
`opencode`, `antigravity`, `mlx`, `llamacpp`, `vllm`). Any
OpenAI-compatible endpoint (Groq, xAI, OpenRouter, LM Studio, Ollama, …)
works via `type = "openai_compat"` + `base_url` + `gen_ai_provider_name` (who
serves it, as tracing names it: `groq`, `vllm`). The CLI delegates
(`claude_cli`/`codex_cli`/`opencode`/`antigravity`) run the
vendor's own agent as a black box, each write-jailed to the worktree by
the tars OS sandbox. DeepSeek is reached via its OpenAI-compatible API
(`DEEPSEEK_API_KEY`, default model `deepseek-flash`); request
`deepseek-v4-pro` for the reasoning model — its chain-of-thought arrives
on the thinking channel automatically:

```bash
export DEEPSEEK_API_KEY=sk-...
cargo run -p tars-cli --bin tars -- probe deepseek --tenant me --prompt "Say hi in 5 words."
```

Once that works, the rest of this guide covers calling tars from
**Rust** and **Node**.

## Install

### Rust

Add to `Cargo.toml`, pinned to a release tag:

```toml
[dependencies]
tars-llm    = { git = "https://github.com/leocaolab/tars.git", tag = "v2.0.0" }
tars-types  = { git = "https://github.com/leocaolab/tars.git", tag = "v2.0.0" }
tars-config = { git = "https://github.com/leocaolab/tars.git", tag = "v2.0.0" }
```

Requires the Rust toolchain in `rust-toolchain.toml` (1.99+).

### Node

Build the addon from a checkout (see [`crates/tars-node/README.md`](../crates/tars-node/README.md)):

```bash
cd crates/tars-node
npm install
npm run build      # → tars-node.<platform>.node + index.js + index.d.ts
```

### The CLI

```bash
cargo install --path crates/tars-cli
```

## Bootstrap config

```bash
cargo run -p tars-cli --bin tars -- init
# writes $TARS_HOME/config.toml (default ~/.tars) with starter providers
```

A provider's `default_model` can name a concrete model or follow the newest
one in a series — `default_model = "flash@latest"`; see
[Model specs and output limits](#model-specs-and-output-limits).

tars reads its global config from **`$TARS_HOME/config.toml`** —
the home resolves as an explicit home (`tars_config::load_tars(Some(home))`) > the `$TARS_HOME` env var >
`~/.tars` (the default); an empty or blank home, passed or in `$TARS_HOME`,
counts as not given. The providers declared there are global: shared by
every tars consumer/tool. Each provider's API key is read from the env var
its `auth` names (`auth = { kind = "secret", secret = { source = "env", var = "…" } }`)
and is never stored in the config file itself. Every `tars` command that reads the config
(`probe`, `bench`, `eval run` / `judge`, `providers`, `models`) also reads
`$TARS_HOME/.env`: a variable the shell sets wins over the file's, and the process
environment is not changed. A line of `.env` that does not parse is an error naming the
file and the line number (exit 1), never the line's text, which may hold a key. A key in a
file (`source = "file"`) or inline is sent by `tars models` / `tars providers --check` too.

Then `export ANTHROPIC_API_KEY=...` (and/or `OPENAI_API_KEY`,
`GOOGLE_API_KEY`) — the config references env vars by name; secrets
don't go into the file.

See [`.env.example`](../.env.example) for the full env-var list.

## Checking your setup — `tars providers` / `tars models`

Two read-only commands answer "is my config wired up, and which models
can I actually ask for?" Both resolve config from
**`$TARS_HOME/config.toml`** (override with `--config <path>`) and
read `$TARS_HOME/.env`, so env-var-backed keys resolve
without pre-exporting them. Neither ever prints a secret. The model-list request follows no
redirect, so a key never goes to a host the provider's `base_url` does not name; a 3xx is
reported as that HTTP status. A `gemini`, `openai` or `anthropic` provider with its own
`base_url` (a gateway) keeps its list under its own id: it does not decide the vendor's
`@latest` or limits.

### `tars providers` — configured providers + key health

```bash
tars providers            # name, type, default_model, key-env health
tars providers --check    # + a fast reachability probe per HTTP provider
tars providers --json     # machine-readable envelope
```

For every provider in your config it prints the `type`, the configured
`default_model`, and how its auth resolves — for an env-backed key,
**which** env var and whether it is currently **set** (`(set)` /
`(UNSET)`), never the value. A provider configured with `auth = "none"`
shows `auth: none (keyless)`; one configured with `auth = "delegate"` shows
`auth: delegated to tool login`; a CLI provider, which has no `auth` setting
and logs in on its own, shows `auth: the tool's own login` (`"kind":
"tool_login"` in `--json`). With `--check`
it also fires the same list-models GET as `tars models --live` and
reports `reachable`, or what went wrong with what the server said:
`auth failed: HTTP 401: <body>`, `HTTP error: HTTP 429: <body>`, `unreachable: <cause>`,
`answered, but the list did not parse: <why>`, `key did not resolve: <why>` (a key file
that cannot be read), `key invalid` (a key holding a newline, refused before anything is
sent), `no key (skipped probe)`, `no list API: <why>`, `server not running (could not
connect): <cause>` (a vllm, mlx or llama.cpp server that is not started) — bounded by a short timeout so a dead
local server can't hang the command. `--json` gives the verdict word as `reachability`
(`reachable`, `auth_failed`, `http_error`, `unreachable`, `unparsable`, `key_unresolved`,
`key_invalid`, `no_key`, `not_applicable`, `not_running`) and the rest as `detail`. A provider never
refreshed shows `not in library` (`"status": "not_in_library"` in `tars models --json`).

### The model library — `tars models`

The **model library** is a JSON file at **`$TARS_HOME/models.json`**
recording, per provider, what that provider's list-models API last
reported: each model id, plus its context and output limits when the API
states them (Gemini does; OpenAI-shaped lists — OpenAI, DeepSeek,
Anthropic, local servers — carry ids only). It's tars-owned state alongside
`config.toml`, and it is the **live layer of the model catalog the runtime
reads** — see [Model specs and output limits](#model-specs-and-output-limits).

```bash
tars models                 # QUERY the library (fast, offline) for every provider
tars models gemini_flash    # just one provider
tars models --live          # bypass the library, hit the provider APIs now
tars models --json          # machine-readable envelope

tars models update          # UPDATE the library from the live APIs, for all providers
tars models update openai   # refresh one provider
tars models --json update   # --json works on either side of `update`
```

- **`tars models`** reads the library — fast and offline. Each provider
  row shows its default as configured and as resolved
  (`[default: flash@latest → gemini-3.8-flash]`), marks it (`← default`),
  lists each model's limits when known, and flags a default missing from
  the last-seen list (`⚠ default not in list (stale config?)`). A library
  more than 7 days old gets a "run `tars models update`" note.
- **`tars models update`** queries every selected provider live,
  persists the result, and reports:
  - what **changed** since last time — `+ added`, `- removed
    (deprecated/retired)`, `~ <model> max output: 8192 → 65536`;
  - where each `@latest` default resolves now, and what it moved from;
  - a concrete `default_model` that is **no longer listed**, or that a
    **newer model in its series** has overtaken (with the `@latest` spec
    that would follow it);
  - where tars's shipped `provider.toml` is **behind** the API: the newest
    model of a series has no row there (limits are known from the API, but
    its price and thinking mode aren't), a row the API no longer lists, or
    a limit the API contradicts.

  It never edits your config or `provider.toml` — it only reports. A
  single-provider update merges into the existing library without dropping
  the other providers' rows. A provider whose query fails (no network, an
  expired key) keeps the list it last returned: the runtime goes on reading
  it, `tars models` shows the failure above it, and `--json` gives the
  list's time as `listed_at`.

**Keep it fresh.** A refresh reaches every tars process that starts after
it — no tars release needed — so run it on a schedule. A daily cron entry:

```cron
17 6 * * * /path/to/tars models update >> ~/.tars/models-update.log 2>&1
```

(`tars models update` reads `$TARS_HOME/.env` itself, so the keys don't
need to be in cron's environment.)

### Exit codes

Every `tars` command reports how it ended in its exit code, so cron and CI
can tell a failure without reading the text:

| Code | Meaning |
|---|---|
| 0 | The command passed. |
| 1 | It did not run: a bad config, an unreadable file, a failed call. |
| 2 | A usage error, refused before anything ran (an unknown flag, an empty id, `bench --repeat 0`, `eval run -o` on a directory that is not empty). |
| 3 | It ran, and what it found is a failure. |

Exit 3 is `tars eval run` with a failed case, `tars eval diff` with a
regression (more failed cases, a check's violation rate up, judge
precision down), `tars models update` with a provider that answered with
no list, `tars providers --check` with a provider that is unreachable,
refused, or answered a list that does not parse, `tars bench` whose every
iteration generated nothing, `tars probe` whose provider failed the probe, and
`tars config check` that found a problem in the files it was given.
A key the config names that does not resolve (a key file that cannot be read)
or cannot be sent (a newline in it) is a failure too. A provider whose key
variable is not set, with no list API, or whose local server (vllm, mlx,
llama.cpp) is not running, is never asked, so it does not count.
A `bench` iteration whose stream ends without `Finished` stops the bench
(exit 1), as it fails `probe`. The built-in local
servers (`mlx`, `llamacpp`, `vllm`) are in every config, so a refresh or a
check of every provider exits 3 while one of them is not running; name the
provider (`tars models update openai`) to refresh just that one.

### Operator settings — `tars config explain` / `tars config check`

Retry, circuit breaker, HTTP timeouts and the output-token reserve are set in
`[llm.retry]`, `[llm.circuit_breaker]`, `[llm.http]` and `[llm.reserve]`. A key
left out keeps its default, which lives with the code that uses it. They can
be written in the user file and in a system file the platform names with
`--system-config` or `$TARS_SYSTEM_CONFIG`; the user file wins key by key.

```toml
[llm.retry]
max_attempts = 5          # also initial_backoff_ms, max_backoff_ms, multiplier,
                          # respect_retry_after, max_wait_ms, jitter_ms
[llm.circuit_breaker]
cooldown_secs = 60        # also failure_threshold
[llm.http]
connect_timeout_ms = 5000 # also stream_idle_timeout_ms
[llm.reserve]
output_tokens = 4096      # assumed for a request that states no output limit
```

```sh
tars config explain                 # every setting, its value and where it came from
tars config explain llm.retry       # one section, or one key
tars config check --system system.toml [--user config.toml]   # exit 3 on any problem
```

A misspelt section or key is an error, not a silent default; so is a
section that the process reading the file does not use.

### Model specs and output limits

Wherever a model id goes — a provider's `default_model`, a consumer's
per-role model — you can write either a **concrete id**
(`gemini-3.8-flash`) or a **series spec**, `<series>@latest`:

```toml
[providers.gemini_flash]
type = "gemini"
default_model = "flash@latest"     # the newest Gemini flash
auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }

[providers.gemini_pro]
type = "gemini"
default_model = "pro@latest"       # the newest Gemini pro (preview included)
auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
```

- Series are defined per provider in `data/provider.toml`
  (`[providers.gemini.series]`: `flash`, `flash-lite`, `pro`). A series is an
  id pattern with a version slot (`gemini-{version}-flash`); `latest` is
  the highest version, a GA model beating a `-preview` of the same version.
  A provider with no series (DeepSeek, local servers) takes concrete ids
  only; `flash@latest` there is an error that says so.
- `@latest` picks from the **live list** in `models.json` when one was
  refreshed, else from the models `provider.toml` ships. So a new Gemini
  flash is picked up by the next process after `tars models update`.
- The spec is resolved **once, when a model is bound to a provider** (the
  provider registry resolves `default_model` at build; a consumer resolves a
  role's model with `ProviderRegistry::resolve_model`). Events and
  cassette fingerprints all carry the concrete id that ran.

**Output limit.** A request that sets no `max_output_tokens` does not mean
"unlimited": the provider's own default applies, and it can be far below the
model's ceiling (DeepSeek: 8K against 384K). So when an `LlmService` binds a
provider and model, it asks the provider for that model's **output limit**,
and a call with no `max_output_tokens` is sent with it. The limit comes from,
in order:

1. the provider's config — `max_output_tokens` on an `openai_compat` /
   `vllm` / `mlx` / `llamacpp` provider;
2. the model's limit in `models.json` (what the API reported);
3. the model's `max_output` in `provider.toml`;
4. none known (a local model, a model no table describes) — the request goes
   out without one and the provider's default applies.

A `max_output_tokens` the caller sets is always sent as set. Each call's event
records both the value sent (`max_output_tokens`) and the limit with its
source (`output_limit`: `configured`, `model` with `from: live | shipped`, or
`unknown`).

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
| `claude_cli` / `codex_cli` / `opencode` / `antigravity` | — | models via the tool's own login |
| `mock` / `cassette` | — | internal test providers |

A non-queryable provider is listed with the reason, not silently
dropped. When a provider needs a key whose env var is unset, the row
carries the **var name to export** (e.g. `no key: set $GEMINI_API_KEY`),
never a sentinel.

## Calling tars

### Rust — one completion

Read the config, build the provider registry from it, stack the middleware
you want, and call. There is no hidden global pipeline: `LlmService::builder`
takes the layers you name, and `LlmService::default_chain` assembles the
standard ones.

```rust
use std::sync::Arc;
use tars_config::{LoadedHome, Section};
use tars_llm::{
    HttpClientConfig, LlmService, OutputReserve, ProcessEnv, ProviderRegistry, RetryConfig,
    RetryMiddleware, TelemetryMiddleware, complete_async,
};
use tars_types::{Caller, ChatRequest, ProviderId, TenantId, TraceId};

// Read $TARS_HOME/config.toml (default ~/.tars), under the file $TARS_SYSTEM_CONFIG names if
// any. Take the [llm.*] sections this program uses: `finish` fails when a file sets one it did
// not take. Build the provider registry, and install the config only once all of that
// succeeded, so a failed start can be retried.
let LoadedHome { config, mut operator, catalog } = tars_config::load_tars(None)?;
let retry = operator.take::<RetryConfig>(Section::LlmRetry)?.into_value();
let http = operator.take::<HttpClientConfig>(Section::LlmHttp)?.into_value();
let reserve = operator.take::<OutputReserve>(Section::LlmReserve)?.into_value();
operator.finish()?;
let registry = ProviderRegistry::from_config_with_env(
    config.providers(),
    &catalog,
    http,
    reserve,
    Arc::new(ProcessEnv),
    None,
)?;
tars_config::install(config, catalog)?;
let provider = registry.get(&ProviderId::new("anthropic"))?;

// An LlmService is one provider, one model and a middleware chain; the first layer is outermost.
let svc = LlmService::builder(provider, "claude-sonnet-5")
    .layer(TelemetryMiddleware::new())
    .layer(RetryMiddleware::new(retry))
    .build();

let mut req = ChatRequest::user("Find race conditions in this Rust function: ...")
    .with_system("You are a precise reviewer.");
req.max_output_tokens = Some(2000);

let caller = Caller::new(TenantId::new("acme"), TraceId::new("race-review"));
let resp = complete_async(svc, req, caller).await?;
println!("{}", resp.text);
println!("{:?}", resp.usage);
```

A `Caller` always names its tenant — whose work the call is, which budgets
and the call log partition by. There is no default tenant. `TenantId::new` and
`TraceId::new` take a literal; text from outside the program goes through
`str::parse`.

### Rust — streaming

`LlmService::call` returns the stream of events and the call-log ids of every
attempt, retries included:

```rust
use futures::StreamExt;
use tars_types::ChatEvent;

let call = svc.call(ChatRequest::user("Say hi in five words"), caller).await;
println!("call log ids: {:?}", call.call_ids);
let mut events = call.result?;
while let Some(event) = events.next().await {
    if let ChatEvent::Delta { text } = event? {
        print!("{text}");
    }
}
```

### Node

```ts
import { Pipeline } from '@leocaolab/tars-node';

// A file in the tars config schema; `tenant` is required, there is no default.
const pipeline = Pipeline.fromConfigPath('tars.toml', 'anthropic', { tenant: 'acme' });

const resp = await pipeline.complete({
    model: 'claude-sonnet-5',
    system: 'You are a precise technical reviewer.',
    user: 'Review this Rust function for race conditions: ...',
    maxOutputTokens: 2000,
});

console.log(resp.text);
console.log(resp.usage?.inputTokens, resp.usage?.outputTokens); // absent when the provider reported none
console.log(resp.stopReason);
```

Roles (`init()`, `pipeline(role)`, `roleModel(role)`), JSON Schema output and
cassette replay from Node are in
[`crates/tars-node/README.md`](../crates/tars-node/README.md).

### Tools — the loop is yours

`tars-tools` gives you the `Tool` trait, a `ToolRegistry` that dispatches one
tool call into one tool message, and built-ins (`fs.*`, `bash.run`,
`web.fetch`, `web.search`) behind an approval sink. tars does not run the
call → tool → call loop for you: you offer the tools on the request, dispatch
what the model asks for, append the results and call again.
[`examples/examples/loop_react`](../examples/examples/loop_react/) is a
complete loop in one file (`cargo run -p tars-examples --example loop_react`).

### Testing without a model

`MockProvider` answers with what you script, so the logic around a call —
prompt assembly, parsing, branching, error handling — is tested with no
network and no key.
[`examples/examples/testing`](../examples/examples/testing/main.rs) shows it
(`cargo run -p tars-examples --example testing`). To pin a *real* reply, use a
cassette (below).

## Decoding a structured response

When you asked the model for JSON, `resp.text` is a string you still have
to parse — and *how* you parse it depends on how the provider produced it.
tars gives you one seam that gets this right: `tars_utils::json_decode`
(`decode` / `decode_json` / `ResponseJsonExt::json` on a `ChatResponse`). It handles the two
failure modes that bite hand-rolled `serde_json::from_str`: providers that
wrap JSON in a ```` ```json ```` fence or chatty prose, and models that
emit an out-of-range integer.

**The strong type is yours; tars is a generic engine — you hand it a `T`,
it hands you back a `T`.** tars never learns your type or your envelope
tag; those live only in your crate. And because it returns *either* a
valid `T` *or* a typed `DecodeJsonError`, you cannot end up holding an
ill-formed `T`: the type is the contract (*parse, don't validate*).

The strategy is keyed off the `StructuredOutputMode` the request used
(from the provider's `ProviderProfile`), so the layer that knows how the
response was produced tells the decoder how to read it:

| Mode | Meaning | Decode strategy |
|------|---------|-----------------|
| `StrictSchema` / `JsonObjectMode` | provider guarantees a clean JSON document | parse `text` directly; a fenced/chatty body is a *broken promise* → `InvalidJson`, never a silent scrape |
| `None` / `ToolUseEmulation` | `text` may be chatty prose with JSON embedded | strip the code fence, scan for the first balanced `{…}` / `[…]`, parse that |

**`resp.json` — the common case:**

```rust
use serde::Deserialize;
use tars_types::StructuredOutputMode;
use tars_utils::ResponseJsonExt;

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
use tars_utils::{decode, DecodeOpts, JsonAgentResponse};

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

**Error taxonomy** (`DecodeJsonError`) — the failure tells you *which* stage
broke, so you branch on the variant, not a substring:

| Variant | Meaning |
|---------|---------|
| `EmptyStream` | no assistant text to decode (e.g. a tool-only turn) |
| `MissingBlock { tried }` | declared envelope tags, none found in the text |
| `NoJsonObject { attempts }` | chatty scan found no balanced JSON value |
| `InvalidJson` | text wasn't valid JSON (in strict mode: a violated "clean JSON" promise) |
| `Schema` | valid JSON, but the wrong shape for `T` |

`JsonValueType` (`Null` / `Bool` / `Integer` / `Float` / `String` / `Array` /
`Object`) is there if you want to write your own "expected an object, got an
array" message.

**Consuming the seam from another repo.** Depend on `tars-utils` (pinned to a
tag, as in [Install](#install)), and use a local `[patch]` while you hack on
tars itself — edit and verify without pushing each time:

```toml
[dependencies]
tars-utils = { git = "https://github.com/leocaolab/tars.git", tag = "v2.0.0" }

# while iterating on tars locally, redirect to your checkout:
[patch."https://github.com/leocaolab/tars.git"]
tars-utils = { path = "../tars/crates/tars-utils" }
```

In one line: a local strong type is *your* serde type + a three-line `impl
JsonAgentResponse` + one `decode::<T>` call. The type is yours; the
mechanism is tars's.

## Output validators

A validator runs after the reply arrives and before it reaches your code.
Validators chain in order; each sees the previous one's filtered response.
Write one by implementing `OutputValidator`, or use the built-ins
(`JsonShapeValidator`, `NotEmptyValidator`, `MaxLengthValidator`):

```rust
use std::sync::Arc;
use tars_llm::{ChainOptions, JsonShapeValidator, LlmService, OutputValidator};
use tars_types::{ChatRequest, ChatResponse, ValidationOutcome, ValidationReason};

struct NoTodo;

impl OutputValidator for NoTodo {
    fn name(&self) -> &str {
        "no_todo"
    }

    fn validate(&self, _req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome {
        if resp.text.contains("TODO") {
            ValidationOutcome::Reject {
                reason: ValidationReason::Custom {
                    kind: "no_todo".into(),
                    message: "the reply left a TODO".into(),
                    detail: None,
                },
            }
        } else {
            ValidationOutcome::Pass
        }
    }
}

let mut opts = ChainOptions::default();
opts.validators = vec![Arc::new(JsonShapeValidator::new()), Arc::new(NoTodo)];
let svc = LlmService::default_chain(provider, "claude-sonnet-5", opts);
```

`ValidationOutcome` has four shapes: `Pass` leaves the reply alone; `Reject`
fails the call with `LlmError::ValidationFailed`, naming the validator;
`Filter` replaces the reply and lists what it dropped; `Annotate` records
per-call metrics. The Node binding has no validators yet: parse and check the
shape in your own code.

## Pre-flight capability check

Every request is checked against the provider's profile before any layer
runs: a request the provider cannot serve fails with
`LlmErrorKind::Incompatible` and is never sent. To find out at startup
instead of on the first call, declare what a role will need:

```rust
use tars_types::{CompatibilityCheck, ProviderNeeds};

let needs = ProviderNeeds {
    requires_tools: true,
    estimated_max_output_tokens: 8000,
    ..ProviderNeeds::default()
};
match svc.provider().capabilities().check_requirements(&needs, svc.output_limit()) {
    CompatibilityCheck::Compatible => {}
    CompatibilityCheck::Degraded { unsent } => eprintln!("served without {unsent:?}"),
    CompatibilityCheck::Incompatible { reasons } => {
        eprintln!("cannot serve: {:?}", reasons.iter().map(|r| r.kind()).collect::<Vec<_>>())
    }
}
```

A capability the profile has no data on (a local server the data file does
not describe) is unknown, not unsupported, and a request that needs it is
still sent.

## Typed errors

`LlmError::kind()` says what went wrong and `class()` whether waiting can fix
it; branch on those, never on the message:

```rust
use tars_llm::LlmError;
use tars_types::{ErrorClass, LlmErrorKind};

fn handle(err: &LlmError) {
    match err.kind() {
        LlmErrorKind::RateLimited => eprintln!("rate limited: back off"),
        LlmErrorKind::Incompatible => eprintln!("this provider cannot serve the request: {err}"),
        _ if err.class() == ErrorClass::Transient => eprintln!("transient, already retried: {err}"),
        _ => eprintln!("permanent: {err}"),
    }
}
```

The kinds (`LlmErrorKind` in `tars-types` lists them all):

| `kind`                | Meaning                                       |
|-----------------------|-----------------------------------------------|
| `auth`                | API key invalid or missing                    |
| `rate_limited`        | Provider 429; check the error's `unavailable_for`           |
| `network`             | Transient connectivity failure                |
| `parse`               | Provider returned malformed response          |
| `malformed_tool_call` | Model sent tool arguments that are not a JSON object (Permanent); the tool name and the raw arguments |
| `truncated_tool_call` | Output hit `max_output_tokens` inside a tool call (Permanent; raise the limit); the tool name and the raw arguments holds the part received |
| `incomplete_tool_call` | The stream's terminator came without a stop reason while a tool call's arguments were unparsed (Permanent); the tool name and the raw arguments holds the part received |
| `stream_cut`          | The response body ended before the end of the stream: the connection was closed under it (Transient); when it was inside a tool call, the tool name and the raw arguments holds the part received |
| `unfinished`          | The stream ended, without an error, before saying why the turn stopped, or with a tool call started and never ended; the reply may be cut short (Permanent) |
| `stream_violation`    | The stream broke the tool-call order: a second start for an open call, arguments or an end for a call that never started, an end whose id is not its start's (Permanent) |
| `server_error`        | The provider failed on its side: a 5xx other than 503 (504 included), or a server error inside an open stream (Transient) |
| `model_overloaded`    | The provider said it is overloaded: 503, Anthropic 529 (Transient) |
| `request_timeout`     | Provider 408: it waited too long for the request (Transient) |
| `provider_timed_out`  | The provider's own time limit for the request ran out before it answered, e.g. Anthropic's 500 `timeout_error` (Permanent) |
| `incompatible`        | Provider's profile cannot serve the request; not sent (Permanent). The call's `llm_call` row lists every reason in `refusal_reasons` |
| `validation_failed`   | Output validator rejected (Permanent)         |
| `context_too_long`    | Prompt exceeds model's context window         |
| `daemon_failed`       | The `claude_sdk` daemon answered with an error in the SDK's words (Transient) |
| `spawn`, `io`         | A CLI or daemon child did not start, or an I/O to it failed; Transient only when the OS error passes on another try (`EAGAIN`, a broken pipe) |
| `batch_item_expired`, `batch_item_canceled`, `batch_item_failed` | A batch item the provider did not run before the batch expired (Transient) or was cancelled, or failed with a code tars does not map (Permanent) |
| `encode`, `invalid_url`, `invalid_header`, `sandbox`, `internal` | tars could not build or send the request (Permanent) |
| ... (`LlmErrorKind` in `tars-types` lists them all) ||

When the provider states in an `x-should-retry` header whether the same request can succeed
later (Anthropic and OpenAI send it), `class()` follows the header; `kind()` stays what the status says.

## Per-call observability

Give the chain a call-log sink and every call — completed, failed or
cancelled — lands one `llm_call` row in a Parquet log, with the request and
response bodies inline. Open the sink inside the tokio runtime:

```rust
use std::sync::Arc;
use tars_llm::{ChainOptions, LlmService};
use tars_melt::event::call_log::{CALL_LOG_SINK, LlmCallTable};
use tars_melt::event::log::ParquetLog;   // tars-melt's `parquet` feature
use tokio::runtime::Handle;

let sink = ParquetLog::<LlmCallTable>::open("/var/lib/myapp/tars-events")
    .sink(CALL_LOG_SINK, None, &Handle::current());
let mut opts = ChainOptions::default();
opts.events = Some(Arc::new(sink));
let svc = LlmService::default_chain(provider, "claude-sonnet-5", opts);
```

Inspect with the CLI:

```bash
tars events list --tenant T --since 1d --tag dogfood
tars events show --tenant T <call_id> --with-bodies
```

For the live tracing stream, JSON logs, OpenTelemetry export and the "I want
to debug X → look at Y" mapping, see [`observability.md`](./observability.md).

For per-call cost caps, per-tenant budgets, provider fallback, and
rate-limit handling, see
[`recipes/cost-and-reliability.md`](./recipes/cost-and-reliability.md).

For offline batch processing (~50% pricing, 24h SLA) on Anthropic /
OpenAI, see [`recipes/batch.md`](./recipes/batch.md).

## A/B testing — two axes, and pinning the LLM

tars frames A/B on **two axes**; getting a strong-typed,
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

```rust
// Both arms replay the SAME pinned completion (the `cassette_schema` provider in
// examples/tars.toml), so the difference is pure code — the regression question.
let review: Review = tars_utils::decode_json(&resp.text, StructuredOutputMode::None)?;

let a = bucket_v_a(review.severity);   // old code
let b = bucket_v_b(review.severity);   // refactored code
assert_eq!(a, b, "behavior changed");  // a regression a text diff won't show
```

**LLM-change axis — vary the prompt/model, diff behavior statistically.**
Here the code is fixed and you compare two configs over a fixed corpus.
Because outcomes are **paired** (same corpus through both), the correct
test is **McNemar** on the discordant cells — *not* two overlapping
confidence intervals. Tag each cohort so the event store can split them
(`Caller::tags` in Rust; the Node binding writes no call log), then compare with
`tars eval diff`. The full methodology (McNemar, paired bootstrap,
precision/recall) lives in [eval-methodology.md](eval-methodology.md).

### Reading a run + diff — and where the tooling stops

`tars eval run` writes a run directory: a `manifest.json` (per-case status,
tokens, latency, check outcomes) plus per-case `output.txt` / `report.json`.
The run owns the directory: `-o` must name a new or empty one, and one with
anything in it is refused (exit 2) so an earlier run's files, its
`judge_report.json` included, are never read as this run's. Pick another
directory or empty it first.
`tars eval diff <baseline> <candidate>` then reports, in tiers:

```
operational:
  cases / errors / tokens in-out        plain deltas (=, +N, -N)
  latency p50            34420ms → 0ms  (-100%)
checks (violation rate):
  valid-json                   0.0% → 12.5%  (+12.5%)   ← a check got worse
trajectory (--trajectory):
  divergence   30.8% (12/39 cases differ)   diverging: case_003, case_011, …
  McNemar (trajectory-match): regressed b=2 improved c=7 exact p=0.180 → NOT significant at α=0.05
quality (if you ran `tars eval judge`):
  precision / recall deltas with Wilson CIs
```

`--json` emits the same facts as one machine-readable object: a check only
the baseline ran has `"dropped": true` and a null candidate rate, and
`quality` (absent when not both runs were judged) has `"state": "compared"`
with both precisions, the paired regressed / improved counts and McNemar's
test, or `"state": "judges_differ"` naming the two judges.
`--trajectory-mode` (default `ordered`) needs `--trajectory`; given alone
it is a usage error. `tars eval run --case-timeout <SECS>` bounds each case,
its model call and any arg-judge calls together; a case past it is recorded
as failed and the run goes on.

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

### Freeze it as a test, not a CLI run

Once a cassette is recorded, the comparison is just a deterministic function
call — so pin it in your normal test runner instead of the `tars eval` CLI.
Point a test at a cassette provider (committed cassette = the fixture) and
assert; no live model, so it runs in CI. The request **fingerprint is
binding-agnostic** — one cassette replays byte-identically in Rust and Node:

```rust
// #[tokio::test] — crates/tars-llm/tests/ab_cassette.rs
let provider = CassetteProvider::replay_from_file(ProviderId::new("cassette_schema"), &cassette_path())?;
let review: Review = tars_utils::decode_json(&replay_text().await, StructuredOutputMode::None)?;
assert_eq!(review.severity, 8);          // a refactor that changes this fails the test
```

Node mirrors this with `node --test` (`crates/tars-node/__test__/ab_cassette.test.mjs`).

### Accepting an intended change

When a diff or a cassette test goes red for an **intended** change, accept it
with a commit that a reviewer reads:

- **The model's reply changed** (new model/prompt, or you re-recorded):
  re-record — `TARS_CASSETTE_RECORD=1 …` — and commit the new
  `*.cassette.json`. Reviewers see exactly which replies moved.
- **Behavior/threshold changed** (a refactor you meant): update the expected
  value in the test's assertion, or promote the candidate run to the baseline
  dir (`benchmarks/baselines/eval/<model>/` — a manual `cp`, the convention
  `tars eval diff` compares against).

Either way the change is a **reviewable commit**, never a silent overwrite, so
an unintended drift cannot slip through as accepted.

## Built-in web tools

Two built-in tools give an agent live web access. Both are thin adapters over
[`sisurf-core`](https://github.com/leocaolab/sisurf) — the browsing engine
(fetch, browser escalation, distillation, result parsing) lives there; tars only
validates the args, calls the one sisurf primitive, and maps its typed result
into a `ToolResult`.

| Tool | In → out |
|---|---|
| **`web.fetch`** | `url` → the page's main content as clean **Markdown**, plus a provenance header (final URL + which **tier** served it: `static` reqwest fetch vs. `browser` Chromium render). Use to READ a page you already have a URL for. If a page needs JavaScript and no headless browser can be launched, you get a legible, actionable `NoBrowser` error ("install Chrome, or fetch a URL that serves without client-side JS"), not an opaque failure. |
| **`web.search`** | `query` → a numbered `title / url / snippet` list. Use to DISCOVER URLs, then follow up with `web.fetch`. Backend: DuckDuckGo unless the embedder configures another (see below). |

Both are network, long-running `web.*` ops, so they route through the same
approval gate as `bash.run`: a policy that marks them `Ask` / `Deny` sends them
through human approval by tool name — no extra wiring.

### Choosing the `web.search` backend

`web.search` defaults to **DuckDuckGo** (`ddg`), which needs no key. The tars
config has no section for it: a `[web_search]` section never took effect (no
command read it) and now fails to load as an unknown field. An embedder that
wants a keyed backend builds the tool itself with
`WebSearchTool::with_config(config)`, where `config` is a sisurf `SearchConfig`
whose API key the embedder has already filled in.

## When NOT to use tars

- **You only call one provider, one model, one prompt shape.** A
  thirty-line HTTP call is fine; tars's value compounds with scale
  (multiple providers, retries, observability, multi-tenant). Below that,
  it's overhead.
- **You need a hosted dashboard / UI today.** tars is a library; it gives
  you the data in the call log, but no UI.
- **You need streaming chat UI in the browser.** Rust's `LlmService::call`
  streams events, but the Node binding returns whole responses, and tars
  ships no HTTP/SSE gateway; SSE proxying is yours to build.
- **You want an agent framework or a library of pre-built chains.** tars
  makes the call; the control flow is yours.

## Where to go next

- [`providers/`](./providers/) — the providers that need more than an API key
- [`recipes/`](./recipes/) — cost, reliability and batch
- [`observability.md`](./observability.md) — what a call left behind
- [`eval-methodology.md`](./eval-methodology.md) — comparing two runs
