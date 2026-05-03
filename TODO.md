# TODO

Living list. Each entry: **what** to do, **why** it's deferred (not "shouldn't", just "not now"), and a **trigger** for when to revisit.

---

## Overengineering — defer-and-revisit list

These were called out in the self-review on 2026-05-03. Decision: **keep** for now (rip-them-out cost > carry cost in the short term), but each has a trigger condition. When the trigger fires we either commit to the abstraction or delete it.

### O-1. `HttpTransport` trait + `OutboundRequest` / `HttpResponse` / `StreamResponse` wrappers
- **Where**: `crates/tars-provider/src/transport.rs`
- **Why deferred**: Borrowed from codex-rs but currently has zero call sites. All providers go straight through `HttpProviderBase.client`. wiremock + the existing integration tests cover everything we need today.
- **Trigger to commit**: A second test that genuinely benefits from a non-HTTP fake transport (e.g. a unit test that wants to assert "the adapter built this exact OutboundRequest" without spinning up wiremock).
- **Trigger to delete**: We hit `tars-pipeline` MVP without anyone needing it.

### O-2. `HttpProviderExtras` — `http_headers` / `env_http_headers` / `query_params`
- **Where**: `crates/tars-types/src/http_extras.rs`, embedded via `#[serde(flatten)]` in 5 ProviderConfig variants
- **Why deferred**: Borrowed from codex-rs's `ModelProviderInfo`. No user has asked for any of these fields. None of our tests use them.
- **Trigger to commit**: First user request — most likely "I need `OpenAI-Organization` header set from env" or "Azure deployment ID in query string."
- **Trigger to delete**: 6 months without a user request → the `#[serde(deny_unknown_fields)]` interaction noise outweighs the latent capability.

### O-3. `Pricing` as a configurable struct (5 × f64 fields)
- **Where**: `crates/tars-types/src/usage.rs`
- **Why deferred**: Designed as if users will customize per-deployment. In practice we have ~5 providers × ~3 models = 15 const data points.
- **Better shape**: `const PRICING: &[(provider_id, model_pattern, Pricing)]` table with helper lookup. Users override only when they have private deployments with negotiated rates.
- **Trigger**: When we add a real cost-display feature (admin dashboard / billing export). The cost table will need to live somewhere — that's the moment to switch from "field on Capabilities" to a proper pricing module.

### O-4. `Capabilities` 12-field struct
- **Where**: `crates/tars-types/src/capabilities.rs`
- **Why deferred**: Currently 0 readers — no routing layer, no pipeline middleware. Completely speculative.
- **Trigger to commit**: First Routing policy that actually filters by capability (e.g. `RequiresVision` model selection).
- **Trigger to slim**: At the moment we build Routing, audit what fields it really reads and drop the rest. Likely we end up with 5 fields (streaming / tool_use / structured_output / max_context / pricing).

### O-5. `Auth::Secret { secret: SecretRef }` nested enum variant
- **Where**: `crates/tars-types/src/auth.rs`
- **Why deferred**: Cosmetic. `Auth::Env { var }` would read better than `Auth::Secret { secret: SecretRef::Env { var } }`.
- **Trigger to flatten**: If we add a second auth-class concept (e.g. mTLS client cert) that's NOT a "secret reference" — at that point the enum reshuffle is forced anyway.

### O-6. `ToolCallBuffer::take_started` flag
- **Where**: `crates/tars-provider/src/tool_buffer.rs`, used by `crates/tars-provider/src/backends/openai.rs`
- **Why deferred**: Functionally correct, just placed wrong. Stream-level state shoved into a struct named for tool calls.
- **Cleaner**: Either let `Started` events repeat (consumer dedupes — `ChatResponseBuilder` already handles it) and drop the flag, OR introduce a proper `StreamState { tool_buf, started_emitted, … }`.
- **Trigger**: Next time we add a third per-stream flag → tipping point for the rename.
- **2026-05-03 update (commit 67de40d)**: We added `pending_stop_reason` for the openai-7/22 fix. That's now **2** stream-level flags on `ToolCallBuffer` — one short of the trigger. Next added flag means "rename now".

### O-7. `SecretString` is theatre, not protection
- **Where**: `crates/tars-types/src/secret.rs`
- **Why deferred**: The Display/Debug redaction does prevent accidental log leaks (real value). Memory-level protection (zeroize on drop, locked pages) is genuinely missing — but writing real secret-protection without a clear threat model is its own overengineering trap.
- **Trigger to harden**: First customer with a security review that asks "are secrets zeroized in memory?" Add `zeroize` crate at that point.
- **Rename consideration**: `RedactedDisplay<T>` would be more honest about scope. Defer to first non-secret use case (PII strings, etc.).

### O-8. `ProviderRegistry::{ids, len, is_empty}` "complete API"
- **Where**: `crates/tars-provider/src/registry.rs`
- **Why deferred**: Trivial. Either get used by routing or are never read.
- **Trigger to delete**: After Pipeline lands, grep `.ids()` `.len()` `.is_empty()` against the Registry. Anything with 0 callers — gone.

### O-9. `tars-config::builtin` 5-provider default table
- **Where**: `crates/tars-config/src/builtin.rs`
- **Why deferred**: Useful for "zero-config first-run" UX, but we have no first-run UX (no `tars init` command yet).
- **Trigger to keep**: When we ship a CLI binary with a `tars init` flow that scaffolds a minimal config — the defaults make user TOML one-liner short.
- **Trigger to delete**: If we never ship that CLI flow, the defaults are unused fixtures.

### O-10. Speculative documentation
- **Doc 06 §8** (Tenant provision/suspend/delete 7-step cascade) — designed for a system with no second tenant
- **Doc 09** (Storage Schema) — full Postgres schemas for tables that don't exist
- **Doc 13** (entire Operational Runbook) — 12 incident playbooks for incidents that haven't happened
- **Doc 12 §5** (gRPC), **§8** (WASM) — speculative API surfaces
- **Why deferred**: Already written. Reading them costs nothing. Implementing them prematurely WOULD cost something.
- **Trigger to revise**: When the actual subsystem ships, audit the doc for what we got wrong vs. right. Don't try to keep them current ahead of code.

---

## Audit follow-up — non-critical findings to revisit

The 2026-05-03 A.R.C. review (`3ab2b7fa`) flagged 208 issues; commit `9683ce8` fixed the 1 critical and 9 of the 51 errors. The rest are deferred:

### A-1. Test quality (148 warnings, 8 info)
Most warnings are `happy-path-only-enumeration` or `assertion-strength-mismatch` — tests cover the main path but not edge cases.
- **Trigger**: Dedicated test-hardening pass, or whenever we touch the relevant module for another reason.

### ~~A-2. `Capabilities` invariant gaps (audit:capabilities-{2,3})~~ ✅ 2026-05-03 (67de40d)
- Resolved by adding `Capabilities::validate()` (rejects empty modalities + ToolUseEmulation without supports_tool_use). `CapabilityError` enum at `tars-types/src/capabilities.rs`.

### ~~A-3. `usage.rs` `saturating_sub` masking invalid usage data~~ ✅ 2026-05-03 (67de40d)
- Resolved with `debug_assert!(cached + creation <= input)` in `Pricing::cost_for`. Saturating-sub stays as a release-build safety net.

### A-4. `events.rs` `ToolCallArgsDelta` lacks `id` field
- Correlation relies on `index` alone. If a provider ever reuses an index across calls in the same stream, args get cross-contaminated.
- **Trigger**: First time a provider's streaming protocol surfaces this. Anthropic and OpenAI both use stable index per stream today; not a real bug yet.

### ~~A-5. `cache.rs` `SystemTime` serialization~~ ✅ 2026-05-03 (67de40d)
- Resolved early — switched to portable epoch-millis i64 via custom `systemtime_millis` serde module (no chrono dep needed). Round-trip test `use_explicit_directive_round_trips_with_handle` proves it.

---

## Real backlog (not overengineering)

### B-1. CLI providers: long-lived stream-json mode (Doc 01 §6.2.1)
- Current `claude_cli` / `gemini_cli` spawn a fresh subprocess per call (cold start 200-500ms).
- **Goal**: Long-lived process pool with `--output-format stream-json` for low-latency interactive use.
- **Cost**: ~1 week of careful work (cancel guards, session pool lifecycle, JSONL bidi protocol).

### B-2. `tars-pipeline` skeleton (Doc 02) — ⏳ partially done
- Tower-style middleware framework with the Doc 02 onion layers.
- **Order**: Telemetry → Auth → IAM → Budget → Cache → Guard → Routing → CircuitBreaker → Retry.
- **MVP shipped (bdaf3b5 + 15600a2)**: `LlmService` trait, `Middleware` + `Pipeline` builder, `ProviderService` (bottom adapter), `TelemetryMiddleware`, `RetryMiddleware` (open-time only, error-class driven, cancel-aware backoff). 13 tests (10 unit + 3 wiremock integration).
- **Still missing**: Routing (= D-8), CacheLookup (needs `tars-cache`), CircuitBreaker, Auth/IAM (needs `tars-security`), Budget (needs `tars-storage` for token-bucket), PromptGuard (needs `tars-tools` ONNX classifier).

### B-3. Hot reload for `ConfigManager` (Doc 06 §6)
- Currently load-once. Real-world: change `~/.config/tars/config.toml` and have it pick up without restart.
- **Trigger**: First user demo where "I want to switch providers without restarting" matters.

### B-4. SQLite event store + Trajectory tree (Doc 04 §3, Doc 09 §4)
- The actual M3 Agent Runtime work.
- **Blockers**: Pipeline (B-2) needs to exist first.

### B-5. `tars-cli` binary
- Wires up CLI Mode adapter (Doc 07 §5).
- **Trigger**: After Pipeline + at least one Skill is testable end-to-end.

### B-6. PyO3 + napi-rs bindings (Doc 12 §6, §7)
- **Trigger**: First Python or Node user.

---

## Doc 01 — LLM Provider gap items

Audit run 2026-05-03 against `docs/01-llm-provider.md`. Code currently implements ~85% of the doc surface (HTTP + CLI + capability + tool-call + structured-output + cache directive + error model + registry are all in). What's still missing:

> **Vocabulary in this section** — borrowed from `defer > delete > implement`:
>
> - **Deferred** = "haven't built yet, but the trigger is plausible — likely to revisit." Default classification.
> - **Frozen** = "haven't built, and don't expect to. Documented for completeness; reads more like an option closed than a TODO." Has its own meta-entry at D-11. Strikethrough on commit, not on freeze.

### D-1. `ExplicitCacheProvider` sub-trait (Doc 01 §10)
- `create_cache(content, ttl) -> ProviderCacheHandle`, `delete_cache(&handle)`, `extend_ttl(&handle, additional)`. Anthropic + Gemini implement; OpenAI never (auto-cache only).
- **Why deferred**: Caller-side has no Janitor / Cache Registry yet to issue creates and track handles. Adding the trait without consumers means dead code per the O-prefix decision rule.
- **Trigger**: When `tars-cache` lands and needs to reach into provider-side caches.
- **Blocker for**: Real cost control on Anthropic-heavy workloads (Doc 01 §10.1 "must actively delete").

### D-2. `Auth::SecretManager` + `Auth::GoogleAdc` + `per_tenant_home` flag
- **Where**: `crates/tars-types/src/auth.rs`. Doc 01 §7 lists 6 Auth variants; we have 3 (None / Delegate / Secret{SecretRef}).
- **Missing**:
  - `Auth::SecretManager { backend: Vault|Aws|Gcp|Azure, key }` — pluggable secret backends
  - `Auth::GoogleAdc { scope: Vec<String> }` — Application Default Credentials for Vertex / Gemini
  - `per_tenant_home` flag on `Auth::Delegate` — multi-tenant CLI HOME isolation (Doc 01 §6.2 + §7)
- **Why deferred**: All three live in the future `tars-security` crate (Doc 14 M6). The `BasicAuthResolver` in `tars-provider` is documented as "test/personal-mode"; production resolvers swap in.
- **Trigger**: M6 (Multi-tenant + Postgres + 安全) per Doc 14.

### D-3. mistral.rs embedded backend (Doc 01 §6.3) — ❄️ Frozen (see D-11)
- In-process LLM inference via `mistral.rs` crate. Apple Silicon Metal backend especially useful for the Mac Pro node (covers same posture as MLX but Rust-native, no `mlx_lm.server` subprocess).
- **Why deferred**: Adds a heavy native dep + GPU-toolchain CI pain. The `mlx`/`llamacpp` HTTP-server backends already cover the same hardware via subprocess. No call-path benefit until someone needs in-process inference (e.g. embedded scenarios with no network stack).
- **Trigger**: First user with "I want zero-process, Rust-only inference" — likely an embedded / WASM-adjacent use case.

### D-4. ONNX `ClassifierProvider` trait (Doc 01 §6.3) — ❄️ Frozen (see D-11)
- Separate trait — **not** `LlmProvider`. Used by PromptGuard middleware's slow lane (DeBERTa injection classifier).
- **Why deferred**: PromptGuard middleware itself doesn't exist (B-2 list). Trait without consumers = dead code.
- **Trigger**: When PromptGuard slow-lane is implemented (Doc 14 M4).

### D-5. Real tokenizer for `count_tokens` (Doc 01 §3 + §15.1) — ❄️ Frozen (see D-11)
- `LlmProvider::count_tokens(req, fast=false)` is supposed to load the real tokenizer (`tiktoken-rs` for OpenAI, `tokenizers` for HF-tokenized models). Current default impl ignores `fast` and always returns `chars/4`.
- **Why deferred**: Budget middleware (the only consumer that needs real counts) doesn't exist. Doc 01 §15.1 is explicit: "don't do precise token counting on the request path; estimate with chars/4, get truth from `response.usage`."
- **Trigger**: When BudgetMiddleware needs pre-flight precision to reject requests over the per-tenant cap *before* incurring provider cost.

### D-6. `capabilities_override` config field (Doc 01 §13) — ❄️ Frozen (see D-11)
- Per-provider TOML can override the built-in capability profile (e.g. local llama.cpp deployment with `supports_thinking = false, prompt_cache = "none"`). Currently capabilities are hardcoded per backend builder.
- **Why deferred**: We can already achieve this in code via `OpenAiProviderBuilder::capabilities(...)`; just not from config. Adding the TOML deserialization is small (~30 lines) but low-value until users have heterogeneous local deployments.
- **Trigger**: First user TOML-only deployment that needs to flag a capability off (e.g. "this vLLM doesn't actually do strict JSON, please don't route strict-output requests here").

### D-7. `ContextTooLong { limit, requested }` populated from error message (Doc 01 §11.1)
- All HTTP adapters currently classify context overflow as `ProviderError::ContextTooLong { limit: 0, requested: 0 }` — typed correctly but with placeholder numbers. Doc 01 §11.1 says these fields enable "上层有明确处理路径（截断 / 摘要）". Without the numbers, callers can't make the truncation decision intelligently.
- **Where**: `crates/tars-provider/src/backends/{openai,anthropic,gemini}.rs` — `classify_error` paths.
- **Why deferred**: Each provider's error message format is different and changes without notice. Real fix is "regex over the message body" — hacky but unavoidable.
- **Trigger**: First time the agent loop hits a long-context request and the truncation policy needs the actual numbers (not just the error class). Until then `0/0` is honest about "we know it overflowed, we don't know by how much".

### D-8. Routing layer (Doc 01 §12) — split out from B-2
- `RoutingPolicy` trait + 5 policies: `ExplicitPolicy`, `TierPolicy`, `CostPolicy`, `LatencyPolicy`, `EnsemblePolicy`. Plus `FallbackChain<P>`. Plus `RoutingMiddleware` that consumes the policy.
- **Where it'll live**: `tars-pipeline` (the policy lives in the pipeline crate that already holds the middleware framework).
- **Why deferred**: Useless without > 1 candidate provider in a single deployment AND a `ModelHint::Tier` use case. Today's call sites all use `ModelHint::Explicit` and pick one provider by id.
- **Trigger**: First time a user wants `gpt-4o-mini → claude-haiku → local-qwen` fallback ordering — the integration test for this lives in TODO already.
- **Tied to**: O-4 (Capabilities slimming); routing's actual reads decide which fields stay.

### D-9. Conformance test suite (Doc 01 §14)
- One test body, run as a generic over `Arc<dyn LlmProvider>`. Each provider impl runs the same conformance set: tool use, structured output, streaming, cancel, error classification, parallel tool-call interleave.
- **Where**: New `crates/tars-provider/tests/conformance.rs`, parameterised over a `provider_factory: fn() -> Arc<dyn LlmProvider>`.
- **Why deferred**: Each backend currently has its own `*_integration.rs` covering its specific wire format. The duplication will become annoying once we have 3+ providers behaving subtly differently — at that point we extract the shared body. Today the duplication is bearable.
- **Trigger**: When we add a 4th HTTP-shape provider (xAI? Mistral La Plateforme?) and copy-paste fatigue hits.
- **Doc 01 §14 also calls for**: nightly CI hitting real APIs for ~$0.01/run. Defer until budget for that exists.

### D-10. Doc 01 §17 open questions
Verbatim from the doc; tracked here so they don't get lost:
- mistral.rs Metal-backend verification on Apple Silicon (blocked by D-3)
- Claude Code CLI `interrupt` JSONL command spec confirmation across versions (blocked by B-1)
- Gemini CLI stream-protocol maturity assessment (blocked by B-1)
- OAuth token auto-refresh for Anthropic + Google (blocked by D-2)
- ONNX classifier multi-thread inference scheduling (blocked by D-4)

### D-11. ❄️ Frozen Doc 01 items — explicit "don't pursue" decisions
Recorded 2026-05-03 after a `defer > delete > implement` review. These are **not** "haven't gotten to yet" — they're "looked at, decided no, here's why". The trigger conditions still apply and could thaw an item, but absent the trigger we don't read these as work backlog.

| ID | Frozen because | Thaw trigger |
|----|----------------|--------------|
| D-3 mistral.rs embedded | MLX (subprocess) + llama.cpp cover the same hardware via the existing OpenAI-compat adapter. In-process FFI adds a heavy native dep + GPU-toolchain CI pain for zero current user benefit. | A user with a no-network embedded / WASM-adjacent posture, OR a measured perf gap where subprocess overhead actually matters. |
| D-4 ONNX `ClassifierProvider` | PromptGuard middleware (the only consumer) is M4 work. Trait without consumers = dead code we'd have to maintain. | When PromptGuard slow-lane gets implemented (M4 per Doc 14). |
| D-5 Real tokenizer | Doc 01 §15.1 explicitly says "don't precise-count on the request path". `chars/4` is correct for our only consumer (a future BudgetMiddleware) until pre-flight rejection precision actually matters. tiktoken-rs is ~30 MB binary bloat per provider family. | When BudgetMiddleware needs to reject pre-flight (i.e. a tenant is hitting their cap often enough that the wasted provider call has measurable cost). |
| D-6 `capabilities_override` config field | The escape hatch already exists in code (`OpenAiProviderBuilder::capabilities(...)`). Adding TOML deserialization is ~30 lines of pure plumbing for a feature we can't recall a single concrete user of. | First TOML-only deployment that needs to flag a capability off (e.g. "this self-hosted vLLM doesn't actually do strict JSON, please don't route strict-output requests here"). |

**What is *not* frozen** — explicitly listed so future-me doesn't conflate "deferred" with "frozen":
- **Tool calling** stays a first-class feature. Already wired across 5 backends (OpenAI, Anthropic, Gemini, vLLM, MLX/llamacpp). Capability flags (`supports_tool_use`, `supports_parallel_tool_calls`) handle the per-model variation cleanly; we don't try to "fix" Llama-3 quantized model tool-call format chaos in the universal layer — that's exactly D-4's domain (PromptGuard ≠ tool-call adapter, but the same "don't pollute the universal type with provider-quirk handling" principle).
- **D-1 ExplicitCacheProvider**, **D-2 Auth::SecretManager / GoogleAdc**, **D-7 ContextTooLong numbers**, **D-8 Routing**, **D-9 conformance suite** — all still expected to land; the trigger conditions are concrete and likely to fire within v1.0 timeline (Doc 14 M3-M6).

---

## Process notes

- **Do not delete from this list silently** — strikethrough with date instead, so we keep the institutional memory of "we considered this and decided X".
- **Trigger conditions are real** — when one fires, open the corresponding work, don't just shuffle the line.
- When in doubt: `defer > delete > implement`. Premature deletion is also overengineering.
- **Deferred ≠ Frozen**. Deferred = expected to ship. Frozen = explicit "no" with thaw conditions. See D-11 for the current frozen list.
