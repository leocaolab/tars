# Claude Code CLI provider — Builder API guide

`claude_cli` is the subscription-authenticated provider that delegates
to your locally installed `claude` binary. Same Anthropic auth as your
interactive Claude Code session — we never see your credentials — but
called as a one-shot subprocess via `claude -p`.

This is the user-facing walkthrough for getting the Builder right.

---

## 1. The surprise: `claude_cli` is not a pure inference channel by default

If you call `claude -p` with the flags that the SDK historically passed,
the CLI does **far more than one inference**. Concrete data from
production `pipeline_events.db` (16 real `llm_call_finished` records,
May 2026):

| reported `output_tokens` | actual response body | response `text` chars | latency |
|---:|---:|---:|---:|
| 14,547 | 5,217 B | 4,587 (≈1.1k tok) | 270 s |
| 14,531 | **509 B** | 175 (≈40 tok) | 277 s |
| 6,733 | 10,284 B | 9,490 (≈2.4k tok) | 131 s |

Token counts diverge from response size by 3–360×, and `input_tokens`
typically reports as a single-digit number even for a 99 KB request.
That's not a measurement bug — those tokens are real, the model really
burns 270 seconds on a single call, but **most of that work is invisible
to the caller**.

What's actually happening: when you run `claude -p` with defaults, the
CLI is a mini-agent runtime. Around every request it auto-loads the
`CLAUDE.md` hierarchy from your cwd, splices in auto-memory, runs hooks,
loads plugins, and can take additional internal turns with tool access
(Read / Bash / Grep / etc.) before emitting a final response. From the
caller side this looks like one LLM call; on the inside it's an opaque
multi-turn session that you didn't ask for.

This matters because the whole point of a `Provider` trait is
interchangeability: `ChatRequest` in, `ChatEvent` stream out. A
`claude_cli` provider shipping defaults breaks that — same request, very
different work done, vs. the Anthropic HTTP API backend (see
[`anthropic.md`](./anthropic.md)).

---

## 2. Flag mapping — the real knobs

The first instinct (mine, and probably yours) is to reach for
`--max-turns`. **It does not exist in the Claude Code CLI.** That flag
lives in some Anthropic SDKs and got cross-wired into people's mental
model. `claude --help` is the source of truth; the actual levers:

| What you want | Real flag |
|---|---|
| Disable all tools → no agent loop possible | `--tools ""` (literal empty string) |
| Skip auto-memory, `CLAUDE.md` auto-discovery, hooks, plugin sync, keychain reads | `--bare` |
| Lower reasoning effort (Sonnet 4.5+) | `--effort low\|medium\|high\|xhigh\|max` |
| Improve cross-tenant prompt-cache reuse | `--exclude-dynamic-system-prompt-sections` |
| Allow a specific tool whitelist (advanced) | `--allowedTools "Bash(git *) Read"` |

The mechanism that replaces `--max-turns`: **if no tools are available,
no agent loop is possible**. The CLI can't take internal turns to read
files or run commands when the only thing it can do is answer. So
`--tools ""` is the kill switch.

### Why `--bare` is the bigger lever

`--bare` is documented in `claude --help` but easy to miss. It skips:

- **auto-memory** — the per-project memory file (`~/.claude/projects/.../memory/MEMORY.md` etc.) that the CLI splices into every system prompt
- **CLAUDE.md auto-discovery** — walks from cwd up looking for `CLAUDE.md` files and appends all of them
- **hooks** — `settings.json` hooks that fire on tool calls / lifecycle events
- **plugin sync** — pulls plugins from configured registries
- **keychain reads** — for credentials beyond `ANTHROPIC_API_KEY`

Auto-memory is the likely main culprit behind the token-bloat data
above. The CLI was silently stuffing your memory + every auto-found
`CLAUDE.md` into the system prompt, then running its built-in agent
framework around that whole inflated context. **`--bare` + `--tools ""`
together are the actual fix.** `--max-turns` would have been wrong
anyway, even if it existed.

---

## 3. Builder API

The library API mirrors the flag list, with defaults that make
`claude_cli` behave as a pure inference channel — same trait contract
as the HTTP-API backends. To get the full Claude Code interactive
experience, you opt in.

> ⚠️ **Critical auth caveat for `--bare`**: `claude --help` documents
> that `--bare` forces auth strictly through `ANTHROPIC_API_KEY` or
> `apiKeyHelper` — **OAuth tokens and keychain are never read** under
> `--bare`. The whole point of picking `claude_cli` over the
> [`anthropic`](./anthropic.md) HTTP backend is usually that you
> authenticated via `claude login` (which uses OAuth + keychain), so
> **the Builder defaults `bare` to `false`**. Setting `bare = true`
> while relying on subscription auth will make every call fail with
> "missing credentials." Only turn `bare = true` on if you have
> `ANTHROPIC_API_KEY` exported in the environment **and** you've
> decided to pay for the cleaner prompt over your subscription quota.

```rust
pub enum ClaudeToolAccess {
    Disabled,                   // → --tools ""
    Default,                    // → omit --tools entirely (CLI default = full access)
    Allow(Vec<String>),         // → --tools "Read,Bash"
}

pub enum ClaudeCliEffort { Low, Medium, High, Xhigh, Max }

impl ClaudeCliProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self;

    // existing
    pub fn executable(self, e: impl Into<String>) -> Self;
    pub fn timeout(self, t: Duration) -> Self;
    pub fn capabilities(self, c: ProviderProfile) -> Self;

    // new — see §17 of the architecture doc for why these defaults
    pub fn tools(self, t: ClaudeToolAccess) -> Self;          // default: Disabled
    pub fn bare(self, b: bool) -> Self;                     // default: false — see auth caveat above
    pub fn effort(self, e: Option<ClaudeCliEffort>) -> Self;// default: None (CLI default)
    pub fn exclude_dynamic_sections(self, b: bool) -> Self; // default: true (cache-friendly)
    pub fn extra_args(self, a: Vec<String>) -> Self;        // escape hatch — see §6

    pub fn build(self) -> Arc<ClaudeCliProvider>;
}
```

### Minimal call site

```rust
use tars_llm::ClaudeCliProviderBuilder;

// All defaults: tools disabled (no agent loop), cache-friendly system prompt.
// bare is OFF so your `claude login` OAuth/keychain auth still works.
let provider = ClaudeCliProviderBuilder::new(ProviderId::new("claude")).build();
```

This is the **subscription-friendly inference** configuration —
fastest setup that still authenticates through your existing
`claude login` session. Equivalent argv:

```
claude -p - --model <model> --output-format json \
       --disable-slash-commands --tools "" \
       --exclude-dynamic-system-prompt-sections
```

For users with `ANTHROPIC_API_KEY` exported who want the cleanest
possible prompt (no auto-loaded `CLAUDE.md`, no auto-memory), opt into
bare mode explicitly:

```rust
// API-key users only — see the auth caveat above.
let provider = ClaudeCliProviderBuilder::new(ProviderId::new("claude"))
    .bare(true)
    .build();
```

Equivalent argv adds `--bare` to the line above.

### Letting the CLI run as an agent (opt-in)

```rust
let provider = ClaudeCliProviderBuilder::new(ProviderId::new("claude"))
    .tools(ClaudeToolAccess::Allow(vec![
        "Read".into(), "Bash(git *)".into(),
    ]))
    .effort(Some(ClaudeCliEffort::High))
    .build();
```

Use this when you genuinely want Claude Code's agent runtime —
exploratory debugging from a Rust host, for instance — and accept that
the call is no longer a single inference. `bare` stays at the default
`false` so auto-memory and `CLAUDE.md` discovery still load the project
context the agent needs.

---

## 4. TOML configuration

Every Builder method has a corresponding TOML field. The
`tars-llm::registry::from_config` plumbing wires them through
identically — there is no config-only or builder-only setting.

```toml
[providers.claude_cli]
type = "claude_cli"
default_model = "claude-sonnet-4-5"

# All optional — values shown are defaults.
executable = "claude"
timeout_secs = 300
tools = "disabled"                  # "disabled" | "default" | ["Read","Bash"]
bare = false                        # see auth caveat in §3 before flipping
exclude_dynamic_sections = true

# Optional opt-in.
# effort = "medium"

# Escape hatch (see §6).
# extra_args = []
```

### A note on TOML schema for `tools`

The deserializer accepts three shapes:

```toml
tools = "disabled"           # → ClaudeToolAccess::Disabled
tools = "default"            # → ClaudeToolAccess::Default
tools = ["Read", "Bash"]     # → ClaudeToolAccess::Allow(...)
```

This is intentional polymorphism: the common cases get a short string,
the advanced case gets the full list. The Rust enum and the TOML schema
intentionally diverge in surface (enum vs string|array) but never in
semantics.

---

## Model — accepted ids

`default_model` is handed verbatim to `claude -p --model <id>`
(`crates/tars-llm/src/provider/backends/cli/dialects/claude.rs:59-63`, via
`build_argv_with`). Set it with the config field `default_model`
(`crates/tars-config/src/providers.rs:255`) or TOML
`[providers.claude_cli] default_model = "…"` (§4). A per-request model, if
supplied, overrides it — tars does not validate the id against your
account.

Known model ids (`crates/tars-config/data/provider.toml:685-701`, both
`billed = "subscription"`):

| id | tier |
|---|---|
| `claude-sonnet-4-5` | mid — **default** (`provider.toml:673`) |
| `claude-opus-4-8` | flagship |

`max_output` in the DB is the **CLI clamp**
(`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, env default ~32k, commonly 64k), not the
model ceiling (`provider.toml:683`).

> ⚠️ **Gotcha — your `claude login` account must recognize the id.** The
> id is passed straight through; if the account no longer serves it (a
> stale id from an older config), `claude` itself rejects the call and its
> error is carried out verbatim — no tars sentinel. Same failure class as
> codex's stale `gpt-5.x` (see [`codex-cli.md §2`](./codex-cli.md)). Fix by
> setting `default_model` to a current id.

---

## Auth / credentials — subscription via `claude login`

`claude_cli` self-authenticates through your existing `claude login`
session (OAuth + keychain) — tars threads it **no** provider credential.
The provider is wired with `Auth::Delegate`, which resolves to
`ResolvedAuth::None`: "the spawned tool/CLI carries its own credential
(e.g. `claude login`); tars injects nothing"
(`crates/tars-llm/src/provider/auth.rs:137-139`; `Auth::Delegate` doc at
`crates/tars-types/src/providers/auth.rs:17-20`). CLI providers consume no
`Auth` (`registry.rs:511-513`).

**Stripped env** — these keys are removed from the child process
(case-insensitive match) so an exported API key or a Bedrock/Vertex/Foundry
toggle can't divert the call off your subscription onto API billing
(`crates/tars-llm/src/provider/backends/cli/claude_argv.rs`):

- `ANTHROPIC_API_KEY`
- `CLAUDE_CODE_USE_BEDROCK`
- `CLAUDE_CODE_USE_VERTEX`
- `CLAUDE_CODE_USE_FOUNDRY`

**Passed env** — claude gets only the system variables (`PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, the temp directories, `TERM`, `TZ`, the locale (`LANG`, `LANGUAGE`, `LC_*`), the `XDG_*` directories, proxies (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`) and CA bundles (`SSL_CERT_FILE`, `SSL_CERT_DIR`, `NODE_EXTRA_CA_CERTS`)),
`CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_OAUTH_TOKEN`; no other variable of the
tars process reaches it (`passes_env` in
`crates/tars-llm/src/provider/backends/cli/subprocess.rs`). The
`claude_sdk` daemon is not jailed and inherits the environment, but loses
the four stripped keys above.

So exporting `ANTHROPIC_API_KEY` does **not** feed the default (non-`bare`)
call a key — the subscription path is what runs. The one exception is
`bare = true`: `claude --help` documents that `--bare` forces auth strictly
through `ANTHROPIC_API_KEY` / `apiKeyHelper` and never reads OAuth/keychain,
so it is the wrong mode for subscription auth — hence the Builder defaults
`bare` to `false` (see the auth caveat in §3 and the decision table in §8).

The `~/.claude` state dir (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) is
granted into the write-jail so login/session state survives the sandbox
(`claude_state_dirs` in `crates/tars-llm/src/provider/backends/cli/dialects/claude.rs`).

A delegated run does not write your global claude config, `~/.claude.json`, which
sits outside that state dir. claude tries to save it during a run; the sandbox
refuses the write, the run and its reply go on as usual, and only that config is not
saved. That is intended: one delegated call should not change settings every later
claude session reads. tars does not open the file to the delegate and does not move
it with `CLAUDE_CONFIG_DIR`. The rule belongs to the delegate boundary, not to
Seatbelt or bubblewrap, so any sandbox tars runs delegates under keeps it.

---

## 5. Composing a Builder-built provider with an LlmService

A common second question: **once I've built a custom provider, can I
still use telemetry / retry / cache / validation?** Yes — an
[`LlmService`] wraps any `Arc<dyn LlmProvider>` and doesn't care where the
provider came from. Three patterns, depending on how many providers you
have and how you load config.

### Pattern A — direct: Builder → LlmService, no Registry

The simplest case. You build one provider, bind it to a model, and wrap it
in the standard middleware stack manually.

```rust
use std::sync::Arc;
use tars_llm::ClaudeCliProviderBuilder;
use tars_llm::{LlmService, TelemetryMiddleware, RetryMiddleware};

let provider = ClaudeCliProviderBuilder::new(ProviderId::new("claude"))
    .tools(ClaudeToolAccess::Disabled)
    .bare(true)
    .build();

// LlmService = provider + one bound model + an ordered middleware chain.
let svc = LlmService::builder(provider, "claude-sonnet-4-5")  // Arc<dyn LlmProvider>
    .layer(TelemetryMiddleware::new())                        // outermost
    .layer(RetryMiddleware::default())                        // innermost
    .build();
```

Layer order matches call order: the first `.layer(...)` runs first
inbound and last outbound. So Telemetry sees every retry attempt,
Retry sees only the inner provider call. That's why Telemetry goes
outside Retry — you want failed retries logged, not silently swallowed.

### Pattern B — Registry: name a hand-built provider

When you have multiple providers and want id-based lookup (multi-tenant
dispatch, or a caller-built fallback across several services), wrap them in
a Registry.

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tars_llm::{ProviderRegistry, LlmProvider};
use tars_types::ProviderId;

let custom: Arc<dyn LlmProvider> = ClaudeCliProviderBuilder::new(ProviderId::new("my-claude"))
    .bare(true)
    .build();

let mut m = HashMap::new();
m.insert(ProviderId::new("my-claude"), custom);
let registry = ProviderRegistry::from_providers(m);

// Downstream:
let p = registry.get(&ProviderId::new("my-claude")).unwrap();
let svc = LlmService::builder(p, "claude-sonnet-4-5")
    .layer(TelemetryMiddleware::new())
    .build();
```

**Important**: `ProviderRegistry::from_providers` does **not** auto-wrap
middleware. The Registry is a naming namespace; the `LlmService` is the
middleware stack. They compose orthogonally. If you want middleware on
every Registry entry, do it in a single `map_providers` pass
(see Pattern C).

### Pattern C — override one entry of a config-loaded Registry

The most common real-world case: you load TOML config that defines five
providers, but for one specific provider you want different settings
than the config produced.

```rust
let registry = ProviderRegistry::from_config(&cfg, ...)?
    .map_providers(|id, default_p| {
        if id.as_str() == "claude_cli" {
            // override with a custom Builder config
            ClaudeCliProviderBuilder::new(id.clone())
                .bare(true)
                .tools(ClaudeToolAccess::Allow(vec!["Read".into()]))
                .build() as Arc<dyn LlmProvider>
        } else {
            default_p
        }
    });
```

`map_providers` (`registry.rs:97`) exists for exactly this — surgical
override without re-loading all of config. Returns a new Registry; the
original is unchanged.

### Why this all works cleanly

```text
LlmService ─wraps→ Arc<dyn LlmProvider>
                       ↑
        trait object — any concrete impl satisfies it
                       │
   ┌───────────────────┼────────────────────┐
   │                   │                    │
ClaudeCliProvider  AnthropicProvider   MockProvider
(Builder-built)    (from config)       (test fixture)
```

`ClaudeCliProvider` implements `LlmProvider` (`claude_cli.rs:134`).
Telemetry / Retry / Cache / Validation see only the trait and have no
idea you're spawning a subprocess. Whatever you pass to `.tools()` /
`.bare()` / etc. is entirely contained inside the provider's argv
construction — invisible to middleware, exactly as it should be.

---

## 6. Escape hatch: `extra_args`

`.extra_args(Vec<String>)` appends raw flags to the `claude -p` argv,
after everything the Builder constructs. Use when the CLI ships a flag
that the Builder doesn't yet model.

```rust
let provider = ClaudeCliProviderBuilder::new(ProviderId::new("claude"))
    .extra_args(vec!["--betas".into(), "experimental-feature".into()])
    .build();
```

**Don't** use this to override flags the Builder already sets — argv
order matters for some flags, and the Builder's value will win on
others. If you find yourself reaching for `extra_args` for a flag the
Builder *should* model, file an issue.

---

## 7. Version drift — how we stay safe

We hardcode the spellings `--tools`, `--bare`, `--effort` etc. If
Anthropic renames a flag in a future CLI version, our calls will fail
loudly (subprocess exits non-zero with `unknown option`).

The defensive layer:

- **argv-shape tests** — `MockSubprocessRunner` captures the
  constructed argv and the test asserts on flag tokens. The moment a
  rename ships, CI breaks before users do.
- **Pinned CLI version in conformance suite** — nightly runs against a
  known-good CLI build to catch silent behavior changes.

If you're upgrading the CLI in production and a Builder method stops
having the expected effect, check the argv-shape test first.

---

## 8. When to use `claude_cli` vs the HTTP API

| Reason | Pick |
|---|---|
| You have a Claude Pro/Max subscription, no API key | `claude_cli` |
| Production server with no interactive login | `anthropic` HTTP — see [`anthropic.md`](./anthropic.md) |
| You want the Claude Code agent loop (file edits, bash, etc.) | `claude_cli` with `bare = false` and `tools = "default"` |
| You want a clean, predictable, single inference | either — both look identical from the trait |
| You need OpenAI-style streaming with tool use | `anthropic` HTTP (CLI streaming is JSONL-framed, different shape) |

Both implement `LlmProvider`, so swapping is a one-line change at the
Builder site. Plan for that.

---

## 9. Implementation notes

Everything in this doc is wired end-to-end and covered by tests in the
`tars-llm` + `tars-config` crates:

- ✅ Builder methods `.executable() / .timeout() / .capabilities() /
  .tools() / .bare() / .effort() / .exclude_dynamic_sections() /
  .extra_args()`
- ✅ `pub use ClaudeCliProviderBuilder`, `ClaudeToolAccess`,
  `ClaudeCliEffort` at the crate root (`tars-llm/src/lib.rs`)
- ✅ TOML `ProviderConfig::ClaudeCli` accepts every Builder field plus
  the three `tools` shapes (`"disabled"` / `"default"` / `["…"]`)
- ✅ `default_claude_cli()` in `tars-config::builtin` emits the
  subscription-friendly defaults
- ✅ `registry.rs` translates `ProviderConfig::ClaudeCli` → Builder
  methods 1:1, with a roundtrip test guarding against silent no-op fields
- ✅ argv-shape tests cover every Builder permutation — they will fail
  loudly if Anthropic renames a CLI flag

The token-bloat behavior described in §1 was the **old**
default-argv-shape. With these defaults landed, the same workload now
produces a pure-inference single LLM call without the agent loop.

