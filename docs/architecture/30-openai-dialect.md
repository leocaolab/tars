# Doc 30 — OpenAI Dialect (behavior-driven provider quirks)

Status: **design** (not built). Refactors how the shared `openai` backend handles
per-provider quirks — from hardcoded `if provider == deepseek` special-cases in
shared code to a behavior-driven `OpenAiDialect` trait (one impl per variant).
Touches `crates/tars-provider/src/backends/openai/`.

## 1. Overview & goal

Every OpenAI-protocol variant (DeepSeek, Groq, Together, xAI, LM Studio, Ollama,
vLLM, MLX, Azure, …) speaks the *same* wire protocol with small **dialect**
differences. Today those differences are **special-cased inside the shared
adapter/mapping** (`if provider == deepseek-ish`), which:
- fragments the shared parser (grows an `if/match` per quirk),
- lets one provider's quirk break another (the shipped bug:
  `mapping.rs` had `thinking_tokens` hardcoded 0, so DeepSeek thinking mode
  reported 0 despite returning `reasoning_content`),
- leaks the quirk into the functional core.

**Goal:** move each variant's quirks into **its own `impl OpenAiDialect`** — the
shared core becomes the *default* impl, a variant overrides only what differs, and
the functional core only ever sees the canonical `ChatRequest`/`ChatResponse`.

**Why behavior (trait), not data (a `Profile` struct) — Open-Closed.** A data
profile can only express quirks its *schema* anticipated; a quirk that exceeds the
schema forces you to reopen the shared struct **and** the shared interpreter. The
profile also accretes into a union-of-all-quirks `Option`-field god-struct + a
shared `if let Some(..)` ladder — the same fragmentation, relocated. A trait is
open for extension (new impl), closed for modification (shared core untouched),
expresses arbitrary logic, keeps each variant's behavior in one file, and the
compiler forces every impl to cover a new axis. (Full comparison: this doc §9.)

**Non-goals.** Not a runtime/config-only provider-plugin system (that's MCP-shaped;
providers are compile-time registered). Not for genuinely-different protocols
(Anthropic / Gemini / **Bedrock** / Cohere stay their own `LlmProvider` impls).
Routing-relevant capability (structured-output support) stays on `Capabilities`.

## 2. CUJs

- **CUJ-1 — Add an OpenAI-dialect provider.** A dev adds Groq/xAI/LM Studio by
  writing one small `impl OpenAiDialect` (overriding only the quirks) + a config
  `type`; the shared backend + parser are untouched.
- **CUJ-2 — A variant's quirk stays isolated.** DeepSeek's `reasoning_content`
  handling lives entirely in `DeepSeekDialect`; a bug there cannot affect Groq or
  standard OpenAI.
- **CUJ-3 — The core stays pure.** A pipeline/agent consuming a completion always
  gets a canonical `ChatResponse`; it never learns which provider produced it.
- **CUJ-4 — Add a local model.** LM Studio / Ollama / vLLM / MLX = `type` +
  `base_url` + (only if it has a quirk) a dialect. No new backend.
- **CUJ-5 — A dialect failure tells the truth.** When `parse_response` can't
  interpret a reply, the error carries the raw payload (not a sentinel).

## 3. Feature list

| # | Feature | CUJ |
|---|---|---|
| F1 | `OpenAiDialect` trait — `build_request` / `parse_event` / `parse_usage` / `parse_response`, default = standard OpenAI | 1,2,3 |
| F2 | Shared adapter/mapping refactored to call `dialect.*` instead of `if provider==` | 2 |
| F3 | Per-variant impls: `StandardDialect`, `DeepSeekDialect`, `LmStudioDialect` (first three) | 1,4 |
| F4 | Config `type` → dialect selection at construction | 1,4 |
| F5 | Typed `parse_response` error carrying the raw payload | 5 |

## 4. Requirements

**Functional**

| FR | Requirement | Feature |
|---|---|---|
| FR-1 | The shared `openai` adapter/mapping contains **no** provider-name branching; all variance goes through `dialect` methods | F2 |
| FR-2 | `OpenAiDialect` methods have **default impls = standard OpenAI**; a variant overrides only what differs | F1 |
| FR-3 | `build_request(adapter, &ChatRequest) -> Result<Value, ProviderError>` and `parse_response(&Value) -> Result<ChatResponse, ProviderError>` are the boundary; the core sees only canonical types | F1,F3 |
| FR-4 | Streaming: a `parse_event(adapter, &SseEvent, &mut ToolCallBuffer) -> Result<Vec<ChatEvent>, ProviderError>` default handles standard SSE deltas; variants override | F1 |
| FR-5 | `parse_response` failure returns a typed error **carrying the raw** (CLAUDE.md #1) — never a sentinel/`unknown`. As-built: `ProviderError::Parse(String)` with the truncated raw embedded in the message | F5 |
| FR-6 | Default behavior (standard OpenAI + existing DeepSeek reasoning handling) is **byte-for-byte preserved** — all current openai tests pass | F2,F3 |
| FR-7 | Adding a variant requires **zero edits** to the shared adapter/mapping | F1 |

**Non-functional**

| NFR | Requirement |
|---|---|
| NFR-1 (OCP) | Shared core closed for modification; variants added by extension only |
| NFR-2 (locality) | A variant's full wire behavior is readable in one file |
| NFR-3 (typed) | No magic strings for finish-reason/quirks; enums + typed errors |
| NFR-4 (perf) | `Arc<dyn OpenAiDialect>` (shared between adapter + provider) — one vtable hop per call, off the hot byte path |
| NFR-5 (compat) | Existing configs unchanged; `type = openai/openai_compat/vllm/mlx/llamacpp` keep working |

## 5. Infra

| Infra | New/exists |
|---|---|
| `OpenAiAdapter` (`backends/openai/adapter.rs`) — ChatRequest↔JSON, SSE→ChatEvent | exists → refactor to delegate to `dialect` |
| `mapping.rs` (`parse_openai_usage`, finish-reason, reasoning) | exists → move variance into dialect |
| `provider.rs` (`OpenAiProvider`, builder) | exists → hold `Arc<dyn OpenAiDialect>`; builder takes a dialect |
| `registry.rs` construction (openai/compat/vllm/mlx/llamacpp) | exists → pick dialect from config `type` |
| `ProviderError::Parse(String)` (raw embedded) | exists (`tars-types`) — reuse for FR-5 |
| `OpenAiDialect` trait + impls | **new** (`backends/openai/dialect/`) |

## 6. Components

### C1 — `OpenAiDialect` trait (`backends/openai/dialect/mod.rs`, new)

```rust
pub trait OpenAiDialect: Send + Sync {
    /// Canonical request → provider wire JSON. Default delegates to
    /// `OpenAiAdapter::build_request_default` (standard chat/completions body).
    fn build_request(&self, adapter: &OpenAiAdapter, req: &ChatRequest)
        -> Result<Value, ProviderError> { /* adapter.build_request_default(req) */ }

    /// One streaming SSE `data:` line → 0..N canonical events. Default delegates
    /// to `OpenAiAdapter::parse_event_default` (standard delta/tool-call/finish),
    /// threading the shared `ToolCallBuffer` for cross-chunk tool-call assembly.
    fn parse_event(&self, adapter: &OpenAiAdapter, raw: &SseEvent, buf: &mut ToolCallBuffer)
        -> Result<Vec<ChatEvent>, ProviderError> { /* adapter.parse_event_default(raw, buf) */ }

    /// Provider `usage` object → canonical `Usage`. Default = `parse_openai_usage`
    /// (reads prompt/completion tokens, nested cached + reasoning tokens). Used by
    /// the streaming finish path so a dialect can reinterpret token accounting.
    fn parse_usage(&self, usage: &serde_json::Map<String, Value>) -> Usage { /* parse_openai_usage(usage) */ }

    /// Non-streaming chat-completion body → canonical response (batch/one-shot).
    /// Default = `openai_chat_completion_to_chat_response`. Failure returns
    /// `ProviderError::Parse` with the truncated raw embedded (FR-5).
    fn parse_response(&self, raw: &Value) -> Result<ChatResponse, ProviderError> { /* standard */ }
}
```
Reuses: the *current* adapter/mapping bodies become these defaults (no logic lost).
Note: the real trait threads `adapter: &OpenAiAdapter` and a `&mut ToolCallBuffer`
through the streaming path, and splits usage parsing into its own `parse_usage`
hook (not in the original sketch) — the SSE loop stays monomorphic while a dialect
overrides only token accounting.

### C2 — Variant impls (`backends/openai/dialect/{standard,deepseek,lmstudio}.rs`, new)

- `StandardDialect` — empty (all defaults). The `openai`/`openai_compat`/vLLM/MLX/
  llama.cpp presets use this unless they need a quirk.
- `DeepSeekDialect` — overrides `parse_response`/`parse_chunk` to pull
  `reasoning_content` (text) + `completion_tokens_details.reasoning_tokens`
  (usage) — the logic currently baked in `mapping.rs`, now isolated.
- `LmStudioDialect` — overrides `build_request` to emit the structured-output
  form LM Studio accepts (`response_format: json_schema`, never `json_object` —
  the error hit earlier this cycle).

### C3 — Backend wiring (`provider.rs`, `registry.rs`)

`OpenAiProvider` holds `dialect: Arc<dyn OpenAiDialect>`; the adapter calls
`self.dialect.build_request(..)` / `.parse_response(..)` instead of inline logic.
`registry.rs` maps config → dialect: `openai/openai_compat/vllm/mlx/llamacpp →
StandardDialect` (default), and a `dialect = "deepseek"|"lmstudio"` config knob (or
inferred) selects an override.

## 7. Interfaces

- **→ functional core**: unchanged — `LlmProvider::stream` still yields canonical
  `ChatEvent`/`ChatResponse`. The dialect is entirely inside the openai backend.
- **← config**: `registry.rs` picks the dialect; a new optional `dialect` field on
  the openai-family `ProviderConfig` (default inferred from `type`/base_url).
- **↔ ProviderError**: `parse_response` returns `Result<_, ProviderError>`; a parse
  failure is `ProviderError::Parse(String)` with the truncated raw embedded in the
  message (raw-carrying, no sentinel).

## 8. Algorithms

**Response parse (per call):**
```
raw = http_json
dialect.parse_response(raw):
    default: standard OpenAI (choices[0].message.content, usage, finish_reason map)
    DeepSeek: default + reasoning_content → thinking; reasoning_tokens → usage
    on any shape it can't read → Err(ProviderError::Parse("… <truncated raw> …"))   # FR-5
```
Invariant: the shared adapter never inspects a provider name; all variance is a
`dialect` method call. Adding a variant touches only its own impl (FR-7).

## 9. Why this beats a data `Profile` (the decision, expanded)

| Axis | data `Profile` | `OpenAiDialect` trait |
|---|---|---|
| Open-Closed | quirk beyond schema → reopen struct + shared interpreter | new quirk → new impl code; shared core untouched |
| Accretion | union-of-all-quirks `Option` god-struct + `if let Some` ladder (fragmentation relocated) | each variant carries only its own logic |
| Expressiveness | only what the schema foresaw | arbitrary logic in a method |
| Locality | behavior split: profile data + shared interpreter | one file = one variant's full behavior |
| New axis | silent `Option` default → wrong behavior | compiler forces every impl to address it |
| The shipped bug class | shared interpreter still one place | variant logic isolated; can't touch others |

Data's only wins: trivial-provider ceremony (mitigated by the default impl —
standard providers write no impl) and config-only extensibility (narrow; that's
MCP territory, and providers are compile-time registered anyway).

## 10. E2E tests

- **E2E-1 (CUJ-2, FR-6)**: `DeepSeekDialect::parse_response(fixture with
  reasoning_content + reasoning_tokens)` → `ChatResponse.thinking` set,
  `usage.thinking_tokens == N` (the shipped-bug regression, now per-dialect).
- **E2E-2 (CUJ-3)**: standard OpenAI fixture through `StandardDialect` → identical
  to today's parse (byte-for-byte).
- **E2E-3 (CUJ-4/1)**: `LmStudioDialect::build_request` with a response schema →
  emits `response_format: {type: json_schema}`, never `json_object`.
- **E2E-4 (FR-5)**: a malformed reply → `parse_response` `Err` carrying the raw
  (assert the raw substring is in the error).
- **E2E-5 (FR-1/7)**: grep the shared adapter/mapping for provider-name branching →
  none remains; adding a dialect touches only its file.

## 11–13. Perf / reliability / security

- Perf: one `dyn` hop per request (NFR-4); byte-level SSE parsing stays monomorphic.
- Reliability: default impls preserve current behavior (FR-6); typed raw-carrying
  errors (FR-5) — fail loud with the truth.
- Security: dialects are internal code (no dynamic loading); no new trust boundary.

## 14. Abstraction & reuse

**Reuse map**: `OpenAiAdapter` build/parse bodies → the trait defaults
(`adapter.rs:248-280` request; `mapping.rs:196` finish-reason, `:217-259` reasoning
→ `DeepSeekDialect`); `ProviderError::Parse` (raw embedded) for FR-5;
`OpenAiProviderBuilder` (`provider.rs:33`) gains a `dialect`. **New**: the
`OpenAiDialect` trait + `dialect/` module.

## Roadmap

- **M0** — `OpenAiDialect` trait + `StandardDialect` (defaults = today's adapter/
  mapping, extracted verbatim). Backend holds `Arc<dyn OpenAiDialect>`; wire it.
  Verify: **E2E-2** (standard byte-identical), FR-6 (all openai tests green).
- **M1** — `DeepSeekDialect` (move reasoning handling out of shared mapping).
  Verify: **E2E-1** (the shipped-bug regression, now isolated).
- **M2** — `LmStudioDialect` (`build_request` structured-output). Verify: **E2E-3**.
- **M3** — config `dialect` selection + docs; grep-clean the shared core of
  provider-name branching. Verify: **E2E-5**.
- **M4 (breadth)** — add Groq/Together/xAI/Ollama as `StandardDialect` (or tiny
  overrides) + presets. Cheap once M0–M3 land.

Providers that are NOT this design (separate `LlmProvider` impls): **Bedrock
(SigV4)** — first, ties to the cloud-security line — and **Cohere**.
