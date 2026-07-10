# Design — one provider definition: `data/provider.toml`, `InterfaceKind`, and the end of hand-written `Capabilities`

**Date:** 2026-07-10
**Status:** proposed — development-ready.
**Grounding:** every `file:line` was read at `tars@main` (`d55a5d2`). Two read-only audits back this:
the Capabilities-vs-KB audit and the KB-structure audit.

---

## 1. Overview & goal

### The one sentence

**A provider's static facts are data, written by tars authors, and there must be exactly one source for
them.** Today they are split three ways: hand-written `Capabilities` literals in 15 backend
constructors, a per-model `data/models.toml` KB, and a hardcoded builtin-provider table in
`builtin.rs`. The three disagree, silently.

### The proof they disagree

Every HTTP/CLI backend hardcodes `max_context_tokens` / `max_output_tokens` for its *default* model,
and every one is stale for every *other* model. Measured:

| provider | hand-written `max_output` | KB `max_output` for its default model | off by |
|---|---|---|---|
| anthropic (`anthropic/provider.rs:85`) | `8_192` | `128_000` (`models.toml:112`) | **16×** |
| openai (`openai/provider.rs:127`) | `16_384` | `128_000` (`models.toml:49`) | ~8× |
| claude_sdk (`claude_sdk.rs:132`) | `8_192` | `128_000` | 16× |
| claude_cli (`claude_cli/provider.rs:125`) | `64_000` | `128_000` | 2× |
| codex_cli (`codex_cli.rs:114`) | `64_000` | `128_000` (`models.toml:85`) | 2× |

`max_context_tokens` is worse — openai reports `128_000` where the KB model is `1_050_000` (8×), anthropic
`200_000` vs `1_000_000` (5×). `openai/provider.rs:131` even ships `supports_vision: false` with the
comment `// gpt-4o supports vision; per-model override expected` — an admission that a per-provider
constant is the wrong shape for a per-model fact.

**Exactly one backend is correct: gemini (http)** — because it alone reads `MODEL_KB` at build time
(`gemini/provider.rs:78-92`). This design generalizes that one backend to all of them.

### Goal

- One data file, `data/provider.toml`, holding every provider's static definition **and** its models —
  provider-level facts and model-level facts in one place, authored by tars, not users.
- `Capabilities` (the runtime struct) stops being hand-written and is **assembled at build time** from
  the provider's block ∪ the selected model's row.
- `InterfaceKind { Cli, Http, Api, Mock }` becomes a first-class provider-level field — the named,
  compiler-checked answer to "how does tars reach and drive this provider", replacing arc's shadow
  `ProviderType` + `is_native_agent` + the `Other` sentinel.

### Non-goals

- **Not** changing the user's `~/.tars/config.toml`. Users still write `type = "claude_cli"`, a model,
  a key. `data/provider.toml` is tars-authored, shipped in the binary via `include_str!`, invisible to
  users. Two files, two authors — see §2.
- **Not** removing the `Capabilities` struct. `LlmProvider::capabilities()` still returns it; only its
  *source* moves from code to data.
- **Not** touching `auth` / `base_url`. The audit confirms **no Capabilities field is derived from auth
  or base_url** in any backend — the only runtime inputs are the KB (gemini) and per-instance
  `CapabilitiesOverrides`. So nothing is blocked from migration by credential coupling.
- **Not** a user-facing capability-override expansion. `CapabilitiesOverrides` (§7) is a separate,
  per-instance concern that survives.

---

## 2. The two config files, and why they are two

| | who writes it | what it answers | where |
|---|---|---|---|
| `~/.tars/config.toml` | **the user** | which providers I use, what model, where my key is | `$TARS_HOME` |
| `data/provider.toml` | **tars authors** | what each provider *is* — interface, models, prices, capabilities | shipped in the binary (`include_str!`) |

The provider *definition* is not the user's to write, the same way the user does not write
`data/models.toml` today. It changes when a provider ships a model or a price, faster than tars
releases — but it is authored by us, verified against official docs, and carried as data so a stale
number is a one-line file edit, not a recompile. (This is verbatim the rationale already at the top of
`data/models.toml`.) `data/provider.toml` **is** `data/models.toml`, widened to carry the
provider-level facts the KB is missing.

---

## 3. The provider/model split — derived from the audit, not chosen

The audit classified all 13 `Capabilities` fields by whether they vary *within* a backend across its
models. The split is not a judgment call; the evidence forced it.

### Per-provider (a protocol/transport/interface fact — constant across every model on the backend)

| field | evidence it is per-provider |
|---|---|
| `interface` (**new**) | one interface per provider by construction |
| `supports_tool_use` | wire-protocol fact; all HTTP `true`, all CLI/SDK `false` (the CLI runs its own loop, never surfaces `ToolCall` — `codex_cli.rs:115`) |
| `supports_parallel_tool_calls` | per-backend constant (openai/anthropic/gemini/bedrock `true`; vllm/mlx/llamacpp `false`, `vllm.rs:111`) |
| `supports_structured_output` | endpoint capability; `StrictSchema`/`JsonObjectMode`/`None`/`ToolUseEmulation` per backend |
| `supports_cancel` | transport fact — can we abort the body / is it spawn-per-call (`claude_cli/provider.rs:131`) |
| `prompt_cache` | provider protocol — `ImplicitPrefix`/`ExplicitMarker`/`ExplicitObject`/`Delegated`/`None` |
| `streaming` | transport — `true` HTTP, `false` for buffered CLI/SDK |
| `pricing.cache_creation_per_million`, `pricing.thinking_per_million` | **provider-behavioral, deliberately NOT per-model** — the KB omits both (`model_kb.rs:142-143`); gemini patches `thinking = output` (`gemini/provider.rs:89`), anthropic has a cache-creation surcharge |

### Per-model (genuinely varies by model — already in the KB, and the hand-written backend value is a bug)

| field | KB source | today's bug |
|---|---|---|
| `max_context_tokens` | `ModelEntry.context` | stale per-backend literal (see §1) |
| `max_output_tokens` | `ModelEntry.max_output` | stale per-backend literal |
| `pricing.input/output/cached_input` | `ModelEntry.{input,output,cached_input}` | backends ship `Pricing::default()` = **all zeros** (silent $0 billing on every non-gemini provider) |
| `modalities_in/out` | `ModelEntry.modalities` | per-backend `{Text}` guess |
| `supports_vision` | derived: `modalities` contains vision | the `gpt-4o` admission above |
| `supports_thinking` | derived: `ModelEntry.thinking != none` | bedrock ships `false` while its Claude models think (`client.rs:186`) |

**Consequence for `Capabilities`:** it becomes `provider-level fields ∪ f(selected model)`. The runtime
struct keeps all 13 fields; six of them are now filled from the model row, seven from the provider block.

---

## 4. The file shape

```toml
# data/provider.toml — tars-authored provider definitions. DATA, not code.
# Supersedes data/models.toml. Every row verified against official docs on `verified`.
schema_version = 2
verified = "2026-07-..."

# ── a fully-defined HTTP provider ──────────────────────────────────────
[providers.anthropic]
interface = "http"                    # InterfaceKind — how tars reaches/drives it
default   = "claude-opus-4-8"
coding_default = "..."

  [providers.anthropic.capabilities]  # provider-level: constant across models
  structured_output = "tool_use_emulation"
  tool_use          = true
  parallel_tool_calls = true
  cancel            = true
  streaming         = true
  prompt_cache      = "explicit_marker"
  # provider-behavioral pricing the KB can't carry per-model:
  cache_creation_per_million = 3.75

  [[providers.anthropic.models]]      # per-model: exactly today's ModelEntry
  id = "claude-opus-4-8"
  tier = "flagship"
  input = 15.00
  output = 75.00
  cached_input = 1.50
  context = 1_000_000
  max_output = 128_000
  thinking = "optional"
  modalities = ["text", "vision"]
  status = "ga"

# ── a CLI-delegate provider with NO models in the KB (local/agent) ─────
[providers.claude_cli]
interface = "cli"
default   = "claude-sonnet-4-5"

  [providers.claude_cli.capabilities]
  structured_output = "none"          # CLI -p exposes no schema/tool surface
  tool_use          = false
  cancel            = false           # spawn-per-call
  streaming         = false
  prompt_cache      = "delegated"

  # claude_cli fronts Anthropic models but through the CLI. Two choices (§6 Open-2):
  #   (a) reference the anthropic models by a `models_from = "anthropic"` alias, or
  #   (b) carry its own rows. Its max_output (64_000) already disagrees with
  #       anthropic's 128_000 — so they are NOT the same model facts; (b) is likely.
  [[providers.claude_cli.models]]
  id = "claude-sonnet-4-5"
  context = 200_000
  max_output = 64_000
  thinking = "optional"
  modalities = ["text"]
  status = "ga"
```

`interface` and the `[capabilities]` block are the only additions over today's `models.toml`. The
model rows are `ModelEntry` **unchanged**.

---

## 5. Types & assembly

### `InterfaceKind` (new) — `tars-types/src/capabilities.rs`

```rust
/// How tars reaches and DRIVES a provider. A closed set; the compiler forces a
/// classification for every ProviderConfig variant (no catch-all). Two axes are
/// folded here — who runs the agent loop (Cli = the vendor binary; the rest =
/// tars) and what the wire is (Http = tars-constructed HTTP; Api = a vendor SDK
/// / daemon). Neither axis is observable at runtime, so this is DECLARED, never
/// detected. Contrast arc's deleted `ProviderType::from_wire`, whose `_ => Other`
/// silently misfiled opencode/antigravity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    /// Vendor agent binary: runs its OWN loop with its OWN tools, edits the
    /// worktree directly. tars hands a prompt, gets final text. A caller MUST NOT
    /// hand it a tool registry. {claude_cli, gemini_cli, codex_cli, opencode, antigravity}
    Cli,
    /// tars drives the loop and supplies the tools, over HTTP it constructs.
    /// {openai, openai_compat, anthropic, gemini, vllm, mlx, llamacpp}
    Http,
    /// tars drives the loop, but the wire is a vendor SDK / long-lived daemon,
    /// not raw HTTP. {claude_sdk (Node NDJSON), bedrock (aws_sdk)}
    Api,
    /// No call is placed. {mock, cassette}
    Mock,
}
```

Add `pub interface: InterfaceKind` to `Capabilities` (`capabilities.rs:12-31`). The struct already
derives `Serialize/Deserialize` and already round-trips through a file (the cassette writer proves it,
`cassette.rs:222`), so this is mechanically free.

### The projection — `tars-config/src/providers.rs`, next to `ProviderConfig`

`InterfaceKind` is **not** read from the file per-provider — it is a total function of the variant, so it
cannot disagree with the variant. But the file also *carries* it (§4) for the provider blocks that have
no variant (`deepseek`, `xai` — see §6 Open-1). The invariant that ties them: a build-time assertion
that `toml.interface == cfg.interface()` for every provider that has both a variant and a file block.

```rust
impl ProviderConfig {
    pub fn interface(&self) -> InterfaceKind {
        match self {
            Self::ClaudeCli{..} | Self::GeminiCli{..} | Self::CodexCli{..}
          | Self::Opencode{..}  | Self::Antigravity{..}          => InterfaceKind::Cli,
            Self::Openai{..} | Self::OpenaiCompat{..} | Self::Anthropic{..}
          | Self::Gemini{..} | Self::Vllm{..} | Self::Mlx{..}
          | Self::Llamacpp{..}                                    => InterfaceKind::Http,
            Self::ClaudeSdk{..} | Self::Bedrock{..}               => InterfaceKind::Api,
            Self::Mock{..} | Self::Cassette{..}                   => InterfaceKind::Mock,
        }
    }
}
```

No `_ =>`. A 17th variant fails to compile until its interface is declared.

### `capabilities_for(provider_name, model_id) -> Capabilities`

The one assembler, replacing all 15 hand-written constructors:

```
block  := PROVIDER_DB.providers[provider_name]           // provider-level facts
model   := block.models.find(model_id) ?? block.default   // per-model row
Capabilities {
    interface:                    block.interface,
    supports_tool_use:            block.capabilities.tool_use,
    supports_parallel_tool_calls: block.capabilities.parallel_tool_calls,
    supports_structured_output:   block.capabilities.structured_output,
    supports_cancel:              block.capabilities.cancel,
    streaming:                    block.capabilities.streaming,
    prompt_cache:                 block.capabilities.prompt_cache,

    max_context_tokens:           model.context,
    max_output_tokens:            model.max_output,
    supports_vision:              model.modalities.contains(Vision),
    supports_thinking:            model.thinking != Thinking::None,
    modalities_in:                model.modalities.map(Kb→Modality),
    modalities_out:               {Text},                  // no backend varies this today

    pricing: Pricing {
        input/output/cached_input: model.{input,output,cached_input},
        cache_creation_per_million: block.capabilities.cache_creation_per_million ?? 0,
        thinking_per_million:       block.capabilities.thinking_per_million ?? 0,
    },
}
```

This is `gemini/provider.rs:78-92` generalized. Every backend's `default_capabilities()` becomes one
call to `capabilities_for(name, model)`.

---

## 6. The hard seams (the audit's warnings, answered)

### The awkward one: KB is keyed by name, `Capabilities` is built per variant, and they don't line up

This is the load-bearing complication. Facts from the KB audit:

- `deepseek` is a KB key with **no `Deepseek` variant** — served by an `OpenaiCompat` builtin.
- `xai` is a KB key with **no `ProviderConfig` and no consumer** — data-only, unreachable.
- 11 variants (Bedrock/ClaudeSdk/CodexCli/Opencode/Antigravity/OpenaiCompat/Vllm/Mlx/Llamacpp/Mock/
  Cassette) have **no KB entry**.
- The only bridges today are hardcoded string literals (`builtin.rs:70/90/101/119`,
  `gemini/provider.rs:79`).

**Resolution:** the file is keyed by a **provider-definition name**, which is a superset of both spaces.
The bridge from a running `ProviderConfig` instance to its definition name is explicit, not inferred:

- **Open-1** — a builtin/HTTP provider names its definition. `default_deepseek` already passes the literal
  `"deepseek"` to `kb_default`; that literal becomes the definition key. `openai_compat` instances that
  are *not* a known definition (a user's local vLLM) have **no** definition block and fall back to
  `text_only_baseline` + `CapabilitiesOverrides` (§7) — exactly as today. So the file defines the
  *named, tars-blessed* providers; anonymous user instances keep the override path.
- `xai` stays in the file as a definition with no variant — harmless, and ready the day a consumer wants
  it. (Today it is already an orphan KB key; the redesign doesn't create the orphan, it inherits it.)

### Open-2 — `claude_cli` fronts Anthropic models but through the CLI

`claude_cli`'s `max_output = 64_000` disagrees with `anthropic`'s `128_000` (the CLI clamps). So they are
**not** the same model facts, and `claude_cli` must carry its own model rows, not alias anthropic's. The
file makes that explicit instead of burying it in a backend literal.

### Open-3 — the `openai_compat` StrictSchema→JsonObjectMode downgrade

`registry.rs:283-285` clones openai's caps then rewrites `structured_output` to `JsonObjectMode`, because
compat endpoints reject strict `json_schema`. Under the file, this is not a patch: `openai` and
`openai_compat` are **separate definition blocks**, each stating its own `structured_output`
(`strict_schema` vs `json_object`). The "openai base minus strict" relationship becomes two honest rows,
not a mutation. `CapabilitiesOverrides` still lets a specific vLLM instance say "actually I do strict".

### Open-4 — provider-behavioral pricing the KB drops

`cache_creation_per_million` and `thinking_per_million` are per-*provider* (gemini's `thinking = output`
hack, anthropic's cache surcharge), and the KB omits them by design (`model_kb.rs:142`). They move to the
provider `[capabilities]` block, not the model row. A naive "pricing = KB pricing" migration would drop
them — the assembler in §5 keeps them provider-level.

---

## 7. `CapabilitiesOverrides` — survives, narrowed in purpose

`providers.rs:549-576` lets a user's `[providers.X.capabilities]` in `~/.tars/config.toml` override
`max_context_tokens` / `max_output_tokens` (only those two). This is **per-instance user config**, a
different thing from the tars-authored definition. It survives unchanged: after `capabilities_for`
assembles the definition, `overrides.apply_to(&mut caps)` runs last, so a user's self-hosted vLLM can
still correct a number tars couldn't know. The file is the default; the user override still wins. (Do
NOT widen it to all 13 fields in this change — that is a separate decision, `providers.rs:682` owns its
validation.)

---

## 8. Migration & invariants to preserve

1. `data/models.toml` → `data/provider.toml`, `schema_version` 1 → 2, add `interface` + `[capabilities]`
   to each provider block. The model rows are byte-identical `ModelEntry`.
2. `ModelKb` grows a `providers.<name>.interface` + `.capabilities` — a new `ProviderDef` wrapping the
   existing `ProviderModels`.
3. Delete the 15 hand-written `Capabilities` constructors; each backend's `default_capabilities()` →
   `capabilities_for(definition_name, model)`. gemini's bespoke KB read (`gemini/provider.rs:78-92`)
   is the reference and collapses into the shared assembler.
4. **Invariants that MUST survive** (the audit's guardrails, all pinned by existing tests):
   - `every_ga_model_carries_input_and_output_price` (`model_kb.rs:292`) — no silent $0 billing.
   - `kb_parses_and_every_default_exists_in_its_models` (`model_kb.rs:213`).
   - `Capabilities::validate()` (`capabilities.rs:50`) — non-empty modalities, `ToolUseEmulation ⇒
     tool_use`.
   - `registry.rs:668-674` conformance — CLI/delegate advertise `!streaming && !supports_cancel`.
   - `builtin.rs:31` panic on a missing KB default stays (fail-loud authoring guard).
5. **New invariant:** `toml.providers[name].interface == cfg.interface()` for every provider with both a
   variant and a file block — the one line that keeps the file and the projection from drifting.

## 9. arc side (a separate PR, after tars ships this)

- delete `arc_types::ProviderType`, `is_native_agent`, `ProviderType::from_wire`'s `Other` sentinel.
- `fix/prompt.rs:135` (the sole reader) → `Config::get().providers[id]` → `capabilities().interface ==
  InterfaceKind::Cli`.
- This fixes the two live bugs from `none-is-not-none`: opencode/antigravity are now `Cli` (no more
  tool-registry flail), and the `Other` sentinel is gone.
- Note `claude_cli` + `tools = "disabled"` is still `Cli` yet brings no tools — `interface` answers "how
  to reach it", not "does it carry tools". That second axis (agency) is deliberately NOT folded in; if a
  consumer ever needs it, it is a separate capability bit, not an `InterfaceKind` variant.

## 10. Success criteria

- `rg 'Capabilities \{' crates/tars-provider crates/tars-bedrock` → only the assembler + tests; zero
  hand-written literals in backends.
- Every provider's `max_context`/`max_output`/`pricing` matches its KB row for every model (the §1
  disagreements are gone — measurable: assert `capabilities_for(p, m).max_output_tokens ==
  kb.find(m).max_output` for all rows).
- No provider ships `Pricing::default()` (all-zero) for a GA model.
- `InterfaceKind` has no `_ =>`; adding a variant fails to compile until classified.
- arc's `ProviderType`/`is_native_agent`/`Other` deleted; `rg 'ProviderType|is_native_agent' arc/crates`
  empty.
