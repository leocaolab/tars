# TODO

Forward-looking list. Each entry: **what** to do, **why** it's deferred (not "shouldn't", just "not now"), and a **trigger** for when to revisit.

**For shipped items**, see [CHANGELOG.md](./CHANGELOG.md). For the day-to-day commit history, `git log`. This file is "what's NOT done and why".

---

## Roadmap status — at a glance (2026-05-08)

Per Doc 14's milestone breakdown. Not authoritative — CHANGELOG is —
but kept current enough that "is X done?" doesn't require a 2,000-line
TODO scan to answer.

| Milestone        | Status                       | Notes |
|------------------|------------------------------|---|
| M0 Foundation    | ✅ shipped                    | tars-types / config / storage / melt / cache (L1) |
| M1 Single path   | ✅ shipped                    | tars-provider (8 backends) + Telemetry/Retry/CacheLookup middleware |
| M2 Multi-prov + Routing | ✅ shipped (M2 scope)  | Routing (Static + Tier) + CircuitBreaker + B-31 capability pre-flight. CostPolicy / LatencyPolicy / Ensemble deferred to M5 (need metrics infra) — not M2 blockers. |
| M3 Agent Runtime | ✅ shipped                    | Session / Turn / TurnGuard / WorkerAgent + Critic + run_task — see B-4 for *enhancements* on top of working baseline. |
| M4 Tools         | ✅ shipped                    | Tool trait + ToolRegistry + fs.read_file / fs.list_dir + MCP integration. fs.write_file gated on Backtrack/Saga (B-4). |
| M5 CLI/MELT      | 🟡 partial                   | tars-cli (init / probe / bench / plan / run / run-task / trajectory) shipped. Per-provider runtime metrics infra (B-8) NOT shipped — blocks Cost/Latency/Ensemble routing. |
| M6 Multi-tenant + Server | ❌ not started        | tars-security / HTTP+gRPC server / Auth/IAM/Budget/Guard middleware all blocked here. |
| M7 Web frontends | ❌ not started                | tui-shape outlined as B-19 (build-our-own, not fork-codex). |
| M8 Python bindings (`tars-py`) | 🟢 in progress      | Stages 1-4 + B-31 + B-20 W1+W2+W4 + v3 shipped. Remaining: B-20 v2 (typed Reject reasons), Pipeline.builder Python surface (B-6c). |
| M9 Output Validation + Eval | 🟡 W1+W2+W4+v3 shipped, W3 enabler shipped, W3 main pending | W3 enabler (Doc 17 Phase 1 — pipeline event store + body store + EventEmitter + `tars events` CLI + cohort tags) shipped 2026-05-08. W3 main body re-scoped per downstream consumer (2026-05-08): original "Rust evaluator runner + trait + sampler + subscription" is overengineered for the consumer's batch-mode use cases; revised plan = 30-line `tars.eval.write_score` Python helper + caller writes evaluator scripts as cron / CI / notebook against the event store. Online monitoring (recall drop alerts, context-saturation correlation) belongs in a separate Doc 19 / B-21 OTel exporter, not the evaluator path. |

### `tars-pipeline` specifically

**M2 deliverables done.** What's missing in the Doc 02 10-layer onion is
NOT a pipeline-crate gap — every missing layer is blocked on a different
crate that hasn't shipped:

| Missing layer            | Blocked on                                       | Unlocks at |
|--------------------------|--------------------------------------------------|---|
| Auth / IAM middleware    | `tars-security` crate                            | M6 |
| Budget middleware        | `tars-storage` KVStore (B-7)                     | M0+ patchback |
| PromptGuard middleware   | `tars-tools` + ONNX classifier                   | D-4 (frozen — needs trigger) |
| L3 cache hooks (create/extend) | `ExplicitCacheProvider` (D-1)              | D-1 |
| CostPolicy / LatencyPolicy / EnsemblePolicy routing | per-provider metrics infra (B-8) | M5 |

Pipeline's trait + builder surface are stable; new layers ship as
`.layer(NewMiddleware)` one-liners on top. **No pipeline-internal
roadmap items pending.**

---

## Overengineering — defer-and-revisit list

These were called out in the self-review on 2026-05-03. Decision: **keep** for now (rip-them-out cost > carry cost in the short term), but each has a trigger condition. When the trigger fires we either commit to the abstraction or delete it.

### O-1. `HttpTransport` trait + `OutboundRequest` / `HttpResponse` / `StreamResponse` wrappers
- **Where**: `crates/tars-provider/src/transport.rs`
- **Why deferred**: Established pattern. All providers go straight through `HttpProviderBase.client`. wiremock + the existing integration tests cover everything we need today.
- **Trigger to commit**: A second test that genuinely benefits from a non-HTTP fake transport (e.g. a unit test that wants to assert "the adapter built this exact OutboundRequest" without spinning up wiremock).
- **Trigger to delete**: We hit `tars-pipeline` MVP without anyone needing it.

### O-2. `HttpProviderExtras` — `http_headers` / `env_http_headers` / `query_params`
- **Where**: `crates/tars-types/src/http_extras.rs`, embedded via `#[serde(flatten)]` in 5 ProviderConfig variants
- **Why deferred**: Established pattern. No user has asked for any of these fields. None of our tests use them.
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

The 2026-05-03 A.R.C. reviews (`3ab2b7fa`, `65be2621`, `71d49588`) flagged ~330 issues across three rounds. The critical + error tier was fixed across `9683ce8 / 67de40d / cf1605e / af2d8f1` — see CHANGELOG entries for the per-round details. The deferred residue:

### A-1. Test quality (148 warnings, 8 info)
Most warnings are `happy-path-only-enumeration` or `assertion-strength-mismatch` — tests cover the main path but not edge cases.
- **Trigger**: Dedicated test-hardening pass, or whenever we touch the relevant module for another reason.

### A-4. `events.rs` `ToolCallArgsDelta` lacks `id` field
- Correlation relies on `index` alone. If a provider ever reuses an index across calls in the same stream, args get cross-contaminated.
- **Trigger**: First time a provider's streaming protocol surfaces this. Anthropic and OpenAI both use stable index per stream today; not a real bug yet.

### A-6. `RequestContext` identity fields are `pub`-mut
- From audit `65be2621` (context-1). `tenant_id` / `principal_id` / `trace_id` are public + mutable, so production code holding `&mut RequestContext` could rotate them mid-request. Audit was disputed (test code in `tars-cache` legitimately mutates `tenant_id` to construct cross-tenant scenarios; locking down to private + setters touches 50+ call sites for marginal real safety) — but the dispute holds only **until M6 multi-tenant runtime exists**.
- **Trigger**: M6 — when there's a real security boundary the field mutability could cross.

---

## Real backlog (not overengineering)

### B-1. CLI providers: long-lived stream-json mode (Doc 01 §6.2.1)
- Current `claude_cli` / `gemini_cli` / `codex_cli` all spawn a fresh subprocess per call (cold start 200-500ms; codex's startup is heavier).
- **Goal**: Long-lived process pool with `--output-format stream-json` (claude/gemini) / sustained `codex exec --json` over a stdio session (codex). Low-latency interactive use.
- **Cost**: ~1 week of careful work (cancel guards, session pool lifecycle, JSONL bidi protocol). Per-CLI quirks compound — each one's session model differs.

### B-2. `tars-pipeline` middleware layers — remaining onion layers
- M2-tier middleware (Telemetry / Retry / CacheLookup / Routing / CircuitBreaker) is shipped — see CHANGELOG. **Still missing in the Doc 02 onion**:
  - **Auth / IAM** middleware: needs `tars-security` (Doc 14 M6).
  - **Budget** middleware: needs `tars-storage`'s `KVStore` (B-7's second half) for token-bucket state across restarts.
  - **PromptGuard** middleware: needs `tars-tools` + ONNX classifier (D-4 frozen).
  - **L3 cache hooks** (cache-create / cache-extend on existing CacheLookupMiddleware): depends on D-1 (`ExplicitCacheProvider`).
  - **CostPolicy / LatencyPolicy / EnsemblePolicy** for routing: need per-provider runtime metrics + (for Ensemble) a fan-out + merge primitive. All blocked on metrics infra (B-8 / M5).

### B-3. Hot reload for `ConfigManager` (Doc 06 §6)
- Currently load-once. Real-world: change `~/.config/tars/config.toml` and have it pick up without restart.
- **Trigger**: First user demo where "I want to switch providers without restarting" matters.

### B-4. M3 Agent Runtime — enhancements beyond the M3 baseline
- M3 is **fully shipped** — see CHANGELOG. Storage + runtime + agent primitive + AgentMessage envelope + all 3 default agents (Orchestrator + WorkerAgent in both stub and tool-using flavours + Critic) + multi-step `run_task` loop + `tars run-task` CLI + `tars-tools` crate (Tool trait + Registry + `fs.read_file`) + `PromptBuilder` extraction all live. The remaining items here are **enhancements** to a working baseline, not gates on M3 completion:
  - **`run_task` replan-on-Reject** — current MVP treats `VerdictKind::Reject` as task-failed. Doc 04 §4.2's full design has Reject trigger a fresh Orchestrator call with the rejection reason as feedback. Slot in when a real consumer hits "the Critic was right to reject but the task is still salvageable".
  - **Per-LLM-call observability inside Worker tool loops** — when WorkerAgent has tools, one `Agent::execute` drives N internal LLM calls + tool dispatches but the trajectory log only captures one StepStarted/LlmCallCaptured/StepCompleted triple (with summed usage + final-answer summary). New event variants `LlmSubcallCaptured` + `ToolCallExecuted` would expose the inner timeline. Trigger: lands alongside Backtrack/Saga (per-call replay needs the granularity anyway) OR when debugging-a-stuck-tool-loop becomes a regular pain point.
  - **`ContextStore` + `ContextCompactor`** (Doc 04 §3.3 / §5). Schema-aware history pruner so multi-step trajectories don't grow the prompt unboundedly. Sits between the Trajectory log and the next `AgentContext`. Trigger: when `run_task` traces start exceeding a model's context window in real use (especially likely once tool-using Workers chain reads).
  - **Block-composition `PromptBuilder`** (Doc 04 §6 full vision). Today's `PromptBuilder` (shipped `8fdeed1`) is fluent assembly of the request *recipe* — model + system + structured_output + temperature + tools. The Doc 04 §6 vision goes further: compose system prompts from typed *blocks* (persona + role + tool-doc + format-rules) so a tenant could rebrand the persona without touching role/format. No consumer needs the block variant today — wait for a second persona to ship (probably alongside multi-tenant work in M6).
  - **Backtrack + Saga compensation** (Doc 04 §6). Concrete `CompensationAction` types + `AgentEvent::CompensationExecuted` + the runtime hook that runs compensations in reverse on backtrack. Trigger: first Tool with externally-visible side effects (`fs.write_file`, `git push`, `web POST`) AND a real failure-recovery scenario where rolling them back matters. **Specifically blocks `fs.write_file` from shipping safely** — see B-9's "additional builtins" note.
  - **CLI: `tars trajectory replay <ID>`** — replays a trajectory's LLM/tool calls against the recorded inputs. Needed once Workers have real side effects (compensation interacts with replay). Trigger: lands with Backtrack.
- **Trigger / order**: ContextStore (when prompts grow) → Backtrack + Saga + replay together (when first side-effecting tool ships) → block-composition PromptBuilder (when multi-tenant rebranding needs it).

### B-5. `tars-cli` follow-on subcommands
- M1 / M2 / M3 surface is shipped (`tars run` + `tars plan` + `tars run-task` + `tars trajectory list/show`) — see CHANGELOG. Remaining CLI surface from Doc 07 §5:
  - `tars chat` — interactive REPL (long-lived process, multi-turn). Where the breaker / pipeline-cache cross-call value actually pays off; would build on the same agent triad `tars run-task` already exposes but with multi-turn context state.
  - `tars trajectory delete <ID>` — needs a retention policy decision (rolling window? size cap? both?). Today the file just grows.
  - `tars trajectory replay <ID>` — needs the multi-step Agent loop (B-4) to know what "replay" means at the action level.
  - `tars trajectory diff <ID-A> <ID-B>` — same prompt, two providers / two configs, what differed. Useful demo when EnsemblePolicy lands.
  - `tars dash` — launcher for the future web dashboard (M7).
  - Shell completions (bash / zsh / fish).
  - `--output json` / CI mode adapter (GitHub PR comment / junit-xml).
- **Trigger**: each item independent. `chat` is the most likely first since multi-turn proves out the runtime / cache / breaker cross-call value.

### B-6. PyO3 + napi-rs bindings (Doc 12 §6, §7)
- PyO3 wheel **shipped** — Stage 1+2+3 (Pipeline / Provider / Session / response_schema / `~/.tars/config.toml` / `tars init`). See CHANGELOG M8. Remaining items:
  - **B-6a. `Response.telemetry` per-call surface (Stage 4)** — see B-15.
  - **B-6b. napi-rs (Node)** — same trait surface, different binding crate. **Trigger**: first Node user. Design constraint: API shape (`Pipeline.from_default(id)` / `Session(pipeline, system, ...)` / `Response.telemetry`) must stay identical across Python / Node / future Go so consumers can switch languages without re-learning the model.
  - **B-6c. PyO3 `PipelineBuilder` for custom middleware** — currently `Pipeline.from_default()` uses a hardcoded layer order (telemetry → cache_lookup → retry → provider). Python can't inject a custom middleware. **Trigger**: first consumer that wants e.g. a custom rate-limit layer or auth-refresh layer specifically from the Python side. downstream consumer and other near-term consumers don't need this.

### B-15. Stage 4 — `Response.telemetry` per-call observability surface — ✅ shipped (`<unreleased>`)
- See CHANGELOG M8 for shipping detail. Surface: `Response.telemetry.{cache_hit, retry_count, retry_attempts, provider_latency_ms, pipeline_total_ms, layers}`. Plumbed via `RequestContext.telemetry: SharedTelemetry` so every middleware writes through the same Arc<Mutex<...>>. Session.send aggregates across the auto-loop's multiple model calls under one handle.
- **Out of scope (preserved as future)**: full OTel exporter (B-8); per-HTTP-attempt visibility (codex exposes `attempt: u64` — tars aggregates retries inside the middleware; revisit if real debugging need shows up); `CallObserver` push-trait (B-18).

### B-16. Session ↔ EventStore integration (durability + multi-agent blackboard)
- **Where**: `tars-runtime/src/{session,event}.rs`, plumbing into existing `tars-storage::EventStore`.
- **What**: Plug `Session` into the existing trajectory + EventStore pipeline rather than build a parallel `SessionStore`. Session optionally takes `Arc<dyn EventStore>` + `TrajectoryId`; emits `AgentEvent` variants for turn lifecycle (TurnOpened / TurnCommitted / TurnRolledBack / ToolCalled / HistoryTrimmed / HistoryReset). `Session::resume(store, trajectory_id)` reads back the trajectory and rebuilds the in-memory `Vec<Turn>`.
- **Why this shape**: tars already has the trajectory (event log, in `tars-runtime`) + blackboard (`EventStore`, in `tars-storage`) two-layer split — agents emit events, store persists. Session is just another agent-shaped thing emitting into the same stream. A new `SessionStore` trait would be parallel infrastructure for the same concern. Multi-agent scenarios (Orchestrator + Worker + Critic each owning a Session writing to the same store) become natural under this model. - **Note on Turn-as-data vs Turn-as-projection**: long-term `Vec<Turn>` should become a derived projection over the event log rather than the primary state, because true async conversations (multi-agent cross-writes, webhook event injection, long-running async tools) don't map cleanly onto strict turn boundaries. Current Turn-as-data is the right pragmatic call (downstream consumer's 80-line Session is turn-based; downstream consumer has none of the async patterns yet); after B-16 lands, the migration to "Turn = `fn turns(events) -> Vec<Turn>` view" is a small refactor since events are already primary. **Don't pre-build the projection now** — wait for the first async consumer.
- **Trigger**: First long-running downstream consumer review where mid-process restart loses 80% of work. Or first multi-agent shared-conversation scenario. Until then, in-memory-only Session is fine.

### B-17. Optional — LLM-summarize compaction (codex-style)
- **Where**: New module under `tars-runtime/src/session/compact.rs` + integration into `Session::trim_to_budget`.
- **What**: When trim would otherwise drop turns, instead invoke the model with a `SUMMARIZATION_PROMPT` to generate a summary of the dropping section, replace those turns with the summary turn. Preserves semantic intent at cost of an extra LLM call.
- **Why deferred**: Current chars-budget trim is "drop oldest whole turn" which is brutally simple but works fine for downstream consumer's profile (review batches don't hit 100k tokens of useful history anyway — at that scale you usually want a fresh Session per PR not a long-lived one). Compaction has real LLM-call cost + risk of summary losing key details.
- **Trigger**: First user complaint that "long agentic loop dropped a critical detail in trim". Or first product where 50+ turn conversations are normal (chat product, not batch reviewer).
- **Pattern reference**: production-tested compaction implementations include an `InitialContextInjection::BeforeLastUserMessage` mode for mid-turn invocation.

### B-18. Optional — `CallObserver` trait (rust-side push hook) — ❌ withdrawn
- ~~Original design: trait + push callback.~~
- **Reason for withdrawal**: B-20 (Evaluation Framework) solves the same class of cross-call aggregation problem via the EventStore stream, with cleaner decoupling (pipeline ↔ aggregator coupled through events rather than a trait callback). Writing CallObserver would create two homogeneous mechanisms alongside EvaluatorRunner.
- **If you need "aggregate metric across pipelines / across calls"** → use B-20's OnlineEvaluatorRunner, not CallObserver.

### B-20. Output Validation + Evaluation Framework — ⭐ highest priority (M9)
- **Design docs**: [Doc 15 — Output Validation](./docs/architecture/zh/15-output-validation.md) + [Doc 16 — Evaluation Framework](./docs/architecture/zh/16-evaluation-framework.md)
- **Breakdown** (adjusted after 2026-05-05 review; 3-wave split to reduce PyO3 single-point risk):
  - **Wave 1 (Rust-only Validator framework)** — ✅ shipped 2026-05-07. `OutputValidator` trait + `ValidationOutcome` enum + `ProviderError::ValidationFailed` + 3 built-in validators (JsonShape / NotEmpty / MaxLength) + `ValidationMiddleware` + `Response.validation_summary` field + `RequestContext.validation_outcome` side channel + 17 unit tests. See CHANGELOG B-20 W1 section for details.
  - **Wave 2 (PyO3 binding)** — ✅ shipped 2026-05-08. Python validators attach to `Pipeline.{from_default,from_config,from_str}` via `[(name, callable), ...]`. `PyValidatorAdapter` bridges Python callbacks into the Rust `OutputValidator` trait; 4 outcome pyclasses (`tars.Pass / Reject / FilterText / Annotate`). A buggy validator (raise / wrong return type) is auto-caught into a permanent `ValidationFailed` — workers don't get killed by user-side bugs. 17 pytests in `crates/tars-py/python/tests/test_validators.py`. See CHANGELOG B-20 W2 section for details.
  - **Wave 3 (downstream consumer integration + Evaluation framework Doc 16, ~7.5 days)** — full Doc 16 implementation (`Evaluator` / `AsyncEvaluator` traits + `LlmCallFinished` / `EvaluationScored` events + `OnlineEvaluatorRunner` / `OfflineEvaluatorRunner` + built-in evaluators + tars-py `tars.eval.Evaluator` base + `Pipeline.with_event_store` API + SQL templates); downstream consumer removes inline `_known_rule_ids` and switches to a Pipeline-attached validator + dogfood.
- **Key design decision (Cache × Validator interaction, locked in during W1 — ⚠️ implementation inconsistent with design, fixed in W4)**:
  - **Design intent**: Cache stores raw Response (pre-Filter). Cache hit still runs the validator chain. Validators are pure, so re-running = CPU local cost only, much cheaper than a wire round-trip. Multi-caller cache sharing is safe. Changing validators doesn't change the cache key. Validator failure does NOT bypass cache.
  - **Bug in W1 implementation** (found during the audit triggered by the downstream consumer (2026-05-08) dogfood flag): `ValidationMiddleware` re-emits the stream as post-Filter events when filtering (`validation.rs:225-232`), so cache sees the post-Filter stream. **Whenever a Filter validator + Cache coexist → cache stores something other than raw**. multi-caller with different validator chains → silent corruption; even with a single chain, cache never returns raw. The side channel `rec.filtered_response` already exists but had been made redundant.
  - **Fix → see B-20.W4**. Until then, consumer / any multi-role consumer must use a per-role independent Pipeline instance, and not reuse one Pipeline with different validator chains.
- **Why this ranks ahead of B-16 / B-17 / B-19**:
  - Both classes of pain points exposed by downstream consumer dogfood (2026-05-04 / 05) are solved here: (a) the model fabricating rule_id / dropping evidence tags → validation; (b) "metrics suddenly dropped, let's figure out what happened" → evaluation.
  - downstream consumer currently has an inline `_known_rule_ids` post-filter (see the downstream consumer) — a placeholder v1 validation implementation; once Doc 15 ships, migrate it directly out.
  - LLM-system-wide observability + quality gating is cross-consumer infrastructure, with higher priority than single-product features (compact / tui).
- **Dependencies**:
  - Depends on the `Pipeline.builder()` API being exposed to Python (internal B-6c) — done as a sub-task of Doc 15 / Wave 1.
  - Depends on EventStore being available at the Pipeline layer — currently only used in tars-runtime; need to wire `Arc<dyn EventStore>` into Pipeline.
- **Estimated total effort**: 12 days (both waves combined); ship a wheel per wave.
- **Relationship with B-15 (Stage 4 Telemetry)**: complementary, non-overlapping. `Response.telemetry` carries infrastructure metrics (cache_hit / retry_count / latency); evaluation carries semantic metrics (rubric grounded rate / evidence filled rate). A dashboard can cross-join the two to answer "did retry_count rise at the same time the metric dropped?".
- ** points (folded into W1.1 / W2.1, not separate backlog entries)**:
  - **Tags field** — `LlmCallFinished.tags: Vec<String>` + `EvaluationScored.tags: Vec<String>`, included in the event schema from day one, defaulting empty. Callers tag via `RequestContext::with_tags()` / `Session::tagged()` helpers. Cohort analysis runs as a one-line SQL `WHERE 'dogfood_2026_05_05' IN tags`, much cleaner than adding a new event field per filter dimension.
  - **OnlineEvaluatorRunner sampling configuration** — four modes: `EvaluatorSampling::{Always, Rate(f64), Stratified, OnDimDrop}`. `Always` is the default for deterministic evaluators; `OnDimDrop { watch_dim, threshold }` is intelligent sampling distinct from fixed sampling — cheap evaluators run continuously, expensive ones (LLM-as-judge) only trigger when another dim drops below a threshold, **saving real money on LLM-judge runs**. OnDimDrop is written into the trait so the interface is reserved for the future even if v1 defaults to `Always`.

### B-20.v3. Python `Response.validation_summary` exposure — ✅ shipped 2026-05-08 (~1h)
- `Response.validation_summary` → frozen pyclass `ValidationSummary { validators_run: list[str], outcomes: dict[str, dict], total_wall_ms: int }`. `outcomes[name]` shape: `{"outcome": "pass"|"filter"|"annotate", "dropped"?: list[str], "metrics"?: dict}`. Reject does not enter outcomes — short-circuits into `TarsProviderError`.
- 3 pytests verifying: filter outcome carries dropped list, no-validators yields empty summary, Pass/exported types. See CHANGELOG B-20 v3 section for details.
- **Origin**: downstream consumer (2026-05-08) feedback; upstream dependency for the metrics column in the dogfood report.

### B-20.v2. Typed `ValidationOutcome::Reject { reason: ValidationReason }` — ⭐ unblocks  consumer parse→structured pipeline (1-2 d)
- **Current state (post-W1+W2)**: `Reject { reason: String, retriable: bool }` — string-only. The Python side `TarsProviderError(kind="validation_failed", is_retriable=bool)` only stuffs the reason string into the message. Callers can't programmatically match against the failure reason.
- **Inconsistency**: B-31 v4 already turned `CompatibilityReason{kind, message, detail_json}` into a typed enum + structured detail. Validator failure should be consistent — otherwise fix-stage has to grep `e.message` again, regressing to the brittle string contract from B-31 v1.
- **Shape**:
  - Introduce `ValidationReason` enum (`#[non_exhaustive]`): `JsonShape{json_path, parse_error}` / `NotEmpty{field}` / `MaxLength{field, length, max}` / `Custom{kind: String, message: String, detail: Option<serde_json::Value>}`.
  - Built-in validators use the corresponding typed variant; Python user-side validators go through `Custom` (caller supplies kind+message+detail).
  - Python compat entry point: `tars.Reject(reason=str)` auto-wraps into `Custom{kind="user", message=reason, detail=None}`; add `tars.Reject.typed(kind, message, detail=None)` as the explicit typed path.
  - `ProviderError::ValidationFailed { validator, reason: ValidationReason, retriable }`; Python `TarsProviderError` gains a `validation_reason: dict` attribute (`{kind, message, detail}`) for caller programmatic access.
- **Estimate**: 1-2 days. Changes span `tars-types/validation.rs` + `tars-pipeline/validation.rs` + 3 builtins + `tars-py/{validation.rs, errors.rs}`. Need to deprecate-not-break the existing `reason: str` entry point.
- **Trigger**: must ship before  consumer starts Tier 2 #4 (parse → structured pipeline). Tier 1 #1/#2/#3 use the FilterText path and aren't blocked; they can land in parallel.
- **Dependencies**: none.
- **Origin**: downstream consumer (2026-05-08) feedback; see conversation log for details.

### B-20.W4. Cache × Validator interaction fix — ✅ shipped 2026-05-08 (route A2)
- **Status**: route A2 landed — onion moved to `Telemetry → Validation → Cache → Retry → Provider` + dropped the `Reject{retriable}` field (`ValidationFailed` is always `ErrorClass::Permanent`). The two W4 regression tests in `tars-pipeline/src/validation/tests.rs` now pass directly (`#[ignore]` removed). See CHANGELOG B-20 W4 section for details.
- **Historical diagnosis (preserved as audit trail)**:
- **bug 1 (cache stores post-Filter)**: `ValidationMiddleware` re-emits post-Filter events after rewriting the response in Filter (`tars-pipeline/src/validation.rs:225-232`); Cache sees the stream after ValidationMiddleware re-emit, so cache stores post-Filter. test 1 asserts cache should store raw "hello world", actual was "hello".
- **bug 2 (cache hit doesn't run validator)**: current onion order `Telemetry → CacheLookup → Retry → Validation → Provider` — Cache is outside Validation. Cache hit short-circuits and returns cached events directly, **Validation is never invoked**. test 2 asserts the second (hit) `telemetry.layers` contains `"validation"`; actually does not. This is more severe than bug 1 — W1 doc §2 "validators rerun on hit" is incompatible with the onion.
- **Consequences**:
  - multi-caller with different validator chains sharing the same Pipeline + cache: the second caller's cache hit returns content filtered by the first caller, and the new validator chain doesn't run — silent corruption.
  - single validator chain case: cache never returns raw; after changing the validator config, hits still return the old cached payload + don't rerun the new validator → a config change becomes an implicit SemVer break.
  - W1 doc §2 "Cache stores raw Response (pre-validation), validators rerun on hit" is inconsistent with the implementation on both points.
- **Fix options (must pick one)**:
  - **A. Change onion order** (recommended): move to `Telemetry → Validation → CacheLookup → Retry → Provider`. Validation is outside Cache → cache hit still goes through Validation. Also, ValidationMiddleware no longer needs the raw vs filtered re-emit branch (Cache can't see Validation output). Cost: `ValidationFailed{retriable:true}` no longer triggers `RetryMiddleware` (Validation is outside Retry). Either move retry logic into ValidationMiddleware itself, or accept the "validation-driven retry doesn't exist" semantics (cleaner). Changes: tweak PipelineBuilder call order (~3 callers), update Doc 02 onion diagram + Doc 15 §2, delete the `filtered_any` re-emit branch in ValidationMiddleware, delete the existing "validation drains replay from cache hit" comment.
  - **B. Keep onion + only fix re-emit**: have ValidationMiddleware always re-emit `events_held` (raw). **Only fixes bug 1, not bug 2** — cache hit still skips the validator. Docs must explicitly state the "validators only run on cache miss" limitation. Low cost but the W1 design contract "rerun on hit" can never be met.
  - Pick A — multi-chain safety must be resolved before Tier 1 lands; B pushes the "caller must guarantee cache namespace isolation" burden onto consumers, repeated in each one.
- **Estimate**: A → 1-2 days (onion change + 5 doc sync points + retry-semantics decision + verify W1's 17 unit tests still pass). B → half a day.
- **Trigger**: must be fixed before consumer-side Tier 1 #1 (snippet validator) ships.
- **Dependencies**: none.
- **Origin**: downstream consumer (2026-05-08) raised the "single-validator-chain assumption" flag → tars-side audit + writing a failing test found the actual bug is one layer worse than the audit suspected (not just "chain inconsistency corruption", but "any Filter + Cache coexistence + Cache hit skips validation").

### B-19. `tars-tui` — interactive terminal UI (path C: build-our-own, not fork-codex)
- **Where**: New crate `crates/tars-tui/` (doesn't exist yet). Consumer of `tars-runtime::Session` + `tars-pipeline::Pipeline`. ratatui-based.
- **What**: Interactive terminal frontend for `tars chat`-style multi-turn conversations. v1 scope: chat history rendering, streaming markdown tokens, tool-call display (folded → expanded), slash commands (`/clear` / `/fork` / `/save` / `/quit`), status bar (model / usage / cache hit / latency), multi-line input with editing shortcuts. Sized at ~3-5k lines for v1.
- **Why "build our own" — codex's TUI is not directly reusable**:
  - codex's `tui/` is **57,736 lines / 102 files** and talks to codex's runtime through `app-server-protocol` — an **18,889-line type surface** assuming codex-specific concepts: rollout files, sandbox events, MCP tool dispatch, approval workflows, apply_patch notifications, ChatGPT auth modes, personality/skill/plugin injection. Not portable abstractions; product-specific to codex.
  - **Path A (implement codex's app-server-protocol on tars)** — rejected. Maps to "build all of codex inside tars" — sandbox + MCP + apply_patch + approval + ChatGPT + personalities. ~8-12 weeks. End state: tars becomes codex-clone, loses its library identity.
  - **Path B (fork codex TUI, swap backend)** — rejected. ~70% of those 102 files are codex-product UI (voice / approval / MCP / apply_patch / theme picker / onboarding / multi-agents / realtime / collaboration_modes / etc.) that tars doesn't have backends for. Remaining ~30 files are coupled to codex's event types (chatwidget.rs alone is 11k lines around codex's specific ChatEvent shape) and need rewriting. Net: ~3-4 weeks of work plus permanent fork-maintenance debt as codex iterates.
  - **Path C (build our own with public-domain rendering utilities)** — chosen. Use public-domain rendering utility crates (markdown render / stream, line wrapping, slash-command parsing) as inputs. Write own app loop / chat widget / input box / status bar around tars's Session API. ~2-3 weeks to v1, no maintenance debt.
- **v1 scope (what's in)**:
  - Multi-turn chat with `tars.Session` backing
  - Streaming token rendering with markdown
  - Tool call display (collapsed by default; expand on cursor / Enter to see args + result JSON)
  - Slash commands: `/clear` `/fork` `/save <path>` `/load <path>` `/reset` `/quit` `/model <id>`
  - Status bar: model id / token counts (in / out / cached) / `Response.telemetry.cache_hit` / latency
  - Multi-line input: Ctrl+Enter to send, ↑/↓ for history, Ctrl+C to interrupt mid-stream
  - Theme: minimal — fg/bg/accent/error 4 colors, no theme picker
- **v1 scope (what's deferred — explicit "not now" list)**:
  - Voice input (codex `voice.rs` 486 lines) — wait for first user request
  - Approval / permission prompts (codex's sandbox UI) — depends on B-2 sandbox middleware which is itself deferred
  - MCP tool UI — depends on MCP integration which tars doesn't have (would be M10+)
  - Apply-patch / diff rendering — tars doesn't do code editing
  - ChatGPT account login UI — tars uses env-var auth model
  - Theme picker — single hardcoded theme until someone asks
  - Onboarding wizard — tars expects users who already ran `tars init`
  - Auto-update prompts — leave to package manager
  - Multi-agent UI / collaboration modes — depends on multi-Session orchestration patterns that haven't crystallized
- **Trigger**: After Stage 4 (B-15 telemetry) and Session+EventStore (B-16) ship — both are dependencies for the status bar and `/save` `/load` commands respectively. Realistic landing target: M9 or M10.
- **Out of scope vs. `tars chat` CLI subcommand (B-5)**: `tars chat` could be a one-line entry point that launches `tars-tui`, OR a much simpler line-oriented REPL without ratatui. Probably both — `tars chat --tui` opens the rich UI, `tars chat` alone gives a minimal readline loop. Decided when B-19 lands.

---

## Brainstorm archive (Day-2 + Day-3, 2026-05-05)

The 7 entries below are architectural directions raised during downstream-consumer dogfood feedback + cross-engineer brainstorming — "needed in the future but not blocking right now". **All are explicitly out of M9 scope** — M9 only does B-20 (Validation + Evaluation). These brainstorms are archived so we can pull them back when a real trigger fires.

### B-21. OpenTelemetry distributed tracing exporter
- **What**: ship a full OTel exporter inside `tars-melt` (M5 was already planning it). tars-internal `tracing::*` events → OTLP → Jaeger / DataDog / Grafana Tempo. Hierarchical context propagation with session_id → turn_id → span_id.
- **Semi-finished state**: TelemetryMiddleware already emits full tracing events; what's missing is `tracing-opentelemetry` + the OTLP exporter. About 1.5 weeks.
- **Trigger**: when timeline debugging of multi-stage agent calls (orchestrator + worker + critic) becomes painful — looking only at `pipeline_total_ms` isn't enough; you need a waterfall view. downstream consumer dogfood is likely the first user.
- **Pattern reference**: a standalone OTel exporter crate is the right shape (OTLP / metrics / traces in one place, decoupled from the runtime).
- **Key design — must not be a flat-trace_id-only build**:
  - tars's current `RequestContext.trace_id` is flat — one id per whole request, **with no parent-child relationship**. After a multi-step agent run, you can't tell which stage took how long.
  - The run-tree model (each LLM/tool call is a run with a parent_run_id forming a tree) is the right shape for this observability layer. OTel's span model is exactly this tree.
  - When implementing, **must** add `span_id: SpanId` + `parent_span_id: Option<SpanId>` to `RequestContext`, new events `SpanStarted` / `SpanFinished` written to EventStore, every middleware / agent / tool emits on entry and exit.
  - After landing: Jaeger waterfall + SQL `WHERE op='critic.review'` directly answers "average duration of critic.review op over the past 1d" — much more useful than a single `pipeline_total_ms` total.
  - Without this layer, B-21 degenerates into "add an OTLP exporter" — semi-finished, and real users get a waterfall and find "the trace is just isolated points with no structure".
- **codex borrow list (carry along during implementation, not separate backlog entries)**:
  - **W3C Traceparent cross-service propagation** —— codex `otel/src/trace_context.rs:19-36`: `set_parent_from_w3c_trace_context(headers)` + `current_span_w3c_trace_context() -> W3cTraceContext` + `traceparent_context_from_env()`. tars's current `trace_id` is internally generated, can't string together with existing upstream/downstream traces (downstream consumer embedded in a web app / downstream consumer invoked via RPC). **When implementing, add `RequestContext::from_traceparent` / `to_traceparent`** — tars traces auto-connect to external Jaeger/DataDog.
  - **Dual-stream event macros** —— codex `otel/src/events/shared.rs:4-52` provides `log_event!` / `trace_event!` macros + target-prefix convention. The same event is emitted twice and routed to different backends by target (logs → file/Loki; traces → OTel span). tars's current `tracing::info!` lumps it all together with no way to separate. **When implementing, build `tars_melt::{log_event!, trace_event!}` macros + a standard target-prefix convention**.
  - **Metrics naming taxonomy** —— codex `otel/src/metrics/names.rs:1-48` centralizes 48 metric constants, hierarchically named: `<subsystem>.<entity>.<measure>_<unit>` (`pipeline.turn.e2e_duration_ms` / `provider.responses_api.ttft_duration_ms`). tars currently has inconsistent styles like `pipeline_total_ms` / `provider_latency_ms`; this will churn in half a year. **When implementing, build a `tars_metrics::names` module centralizing constants**, drop codex's `codex.` prefix, and adopt the same hierarchical taxonomy.

### B-22. Shadow Replay — model-swap regression-prevention system
- **What**: mark representative traces in the production EventStore as `golden`; a new `ShadowRunner` resends those requests to a candidate provider/model and back-to-back diff-scores them. CLI: `tars shadow --dataset regression-v1 --provider gemini-3 > report.json`.
- **Reuse base**: 90% shares code with OnlineRunner / OfflineRunner — only adds a "resend + diff" mode + `LlmCallFinished` `tags` field (already added to the schema in B-20). About 4-5 days (after B-20 lands).
- **Trigger**: first model swap (OpenAI silently changes / want to switch to Gemini-3 / want to switch to local). The downstream consumer is already discussing a gemini-3-flash-preview switch.
- **Why not in M9**: needs EventStore + Evaluator to be stable; doing it too early is an empty shell.
- ** points (carry along during B-22 implementation)**:
  - **`PairwiseEvaluator` trait** — beyond single-response scoring (`Evaluator::score`), add a pairwise interface `compare(req, a, b) -> A | B | Tie + confidence`. **The core action of Shadow Replay is pairwise** ("after switching to gemini-3, is it better or worse than before?"); without this interface it can't be done. New event `PairwiseScored { trace_id_a, trace_id_b, evaluator_name, verdict }` written to EventStore.
  - **`Dataset` as a first-class typed object** — not "a pile of jsonl files" or "a group of trace_id ad-hoc variables". `Dataset { id, name, version, trace_ids, metadata }` persisted in EventStore, with API including `create_dataset` / `fork_dataset` / `dataset_traces`. `tars dataset create --name regression-v1 --tag dogfood_2026_05_05 --schema-compliance ">0.8"` distills a regression set out of production traces in one line. Aligned with tars's library positioning (no hosted UI, pure library form).
  - Both items **are hard dependencies for Shadow** — B-22's implementation spec must include them.

### B-23. Circuit Breaker → Routing fallback last mile
- **What**: tars already has `CircuitBreakerMiddleware` + the `Routing` layer (M2 shipped), but the "circuit_open → auto-fallback to the next candidate" wiring may be incomplete. Verify + fill in so "primary provider trips → auto-switch to candidate" is truly usable.
- **Trigger**: first cross-provider fallback need. The downstream consumer currently still configures degradation manually when the critic degrades; automation is an optimization.
- **Estimate**: 1-2 days to fill in wiring + write tests.

### B-24. Prompt Registry / A/B Routing
- **What**: Prompt-as-code, remotely distributed (from git / config center), SemVer versioned; `Router` layer supports prompt_version-based traffic splitting; `LlmCallFinished` carries `prompt_version`, EventStore SQL directly produces v1 vs v2 score comparisons.
- **Currently**: the downstream consumer hardcodes prompts in Python files; prompt changes require a PR. Painful at scale.
- **Trigger**: when the team grows enough that prompt changes need canary rollout / parallel iteration by multiple people; or the first time we want to A/B test two prompt versions.
- **Dependencies**: B-20 EvalFramework landed (provides scores).

### B-25. Semantic Cache Middleware (vector-similarity short-circuit)
- **What**: insert a `SemanticCacheMiddleware` layer ahead of Retry. Similar prompts hitting the threshold return the cached Response directly, skipping the Provider call. Backed by Redis+Vector / Qdrant. After a hit, `LlmCallFinished` is marked `cache_kind: Semantic`; the evaluator still runs (to monitor whether caching degrades quality).
- **Trigger**: when business traffic ramps up and "duplicate thinking" becomes the bulk; or when Provider API latency starts to hurt. The downstream consumer's current scale is fine with the exact-match L1/L2 cache.
- **Why deferred**: adds a runtime dependency (vector store); don't add until the first user asks.

### B-26. LLM FinOps — Token / Cost aggregation + Quota middleware
- **What**:
  - Built-in Price Card (per-model USD/1M token)
  - `LlmCallFinished` gains a `cost_usd: f64` field
  - `QuotaMiddleware`: budget per tenant / user / session, over → block OR auto-downgrade to a cheaper model
- **EventStore SQL bonus**: once in place, "how much did each user spend over the past 7 days" is a one-line SQL query
- **Trigger**: when a second user shows up needing cost split; or the first "ran up the bill" incident. The downstream consumer is currently single-user, single-machine batch jobs with no billing anxiety.
- **Dependencies**: B-20 EventStore landed (cost goes into the LlmCallFinished payload).

### B-27. Pre-flight Guardrails — Input safety gate
- **What**: an interception layer before requests reach the Provider. Prompt injection detection (regex / small classifier), PII redaction (phone/credit-card/SSN → placeholders, restored after the response returns). Trigger → HTTP 400, doesn't consume Provider tokens.
- **Difference from ValidationMiddleware**: Guardrail is an input layer (pre-request); Validation is an output layer (post-response). Don't conflate them: guardrail is a safety gate, validation is contract checking.
- **Trigger**: first public / multi-user product launch; or when compliance requirements appear. The downstream consumer is internal-use with trusted prompts, not blocked.
- **Why deferred**: adds runtime dependencies (classifier models / regex libs), and only matters for external input; that scenario doesn't exist now.

### B-28. DPO / SFT data export (data flywheel)
- **What**: add a `FeedbackReceived` event variant to EventStore (user feedback or business replay signal); `tars dump-trace` CLI supports DPO / SFT format output.
- **Why not build the exporter**: business-specific — how chosen/rejected is defined, whether the format is DPO or SFT or PPO, varies by team. tars provides the (thinnest) data extraction channel + reserves the event variant slot, **without any built-in fine-tuning format converter**.
- **Trigger**: first user wanting to fine-tune. The downstream consumer isn't doing it.
- **Thinnest step we can do now**: add the `FeedbackReceived` event variant + reserve a slot in the EventStore schema (30 lines), no exporter. This minimum-investment option was discussed during brainstorming.

---

**Brainstorm consensus**:

- All 8 entries from Day-2/Day-3 are "needed in the future but not blocking current M9"
- M9 alone completing B-20 (Validation + Evaluation together) = full downstream-consumer unblock + observability/quality-gating infrastructure for the whole LLM system
- The first candidate after M9 is **B-21 OTel exporter** — most-mature semi-finished item, current pain point for downstream-consumer multi-stage agent debugging
- Long-term "panoramic view" of tars evolution: Control Plane (Config / Router / CB) + Data Plane (Pipeline / Middleware / Provider) + Observability (tracing + EventStore) + Intelligence Plane (EvalRunner / ShadowRunner / data extraction)


### 3 gaps found from a deep review of the real code (2026-05-05)

After an external reviewer read the real code in `routing.rs` / `retry.rs` / `middleware.rs` / `provider.rs`, they pointed out 3 production-real gaps. All verified to land. Disposition:

#### B-31. Routing capability pre-flight check — ✅ shipped (`<unreleased>`)
- **Where**: `tars-pipeline/src/routing.rs:202-285`
- **What shipped**:
  - `tars_types::ChatRequest::compatibility_check(&Capabilities) -> CompatibilityCheck` — checks tools / structured_output / thinking / vision; aggregates ALL incompatibility reasons in a single pass (caller sees the full list, not just the first failure).
  - `CompatibilityCheck { Compatible, Incompatible { reasons: Vec<String> } }` — 2-state (deliberately not 3-state — see code-comment for "we don't have global view at per-candidate level" reasoning; routing layer synthesizes the global verdict).
  - `RoutingService::call` candidate loop now calls `compatibility_check` *before* `provider.stream(...)`. Incompatible candidates are skipped with a structured warn log; their reasons are collected into `skipped_with_reasons`.
  - When all candidates are skipped (no wire-level errors), routing returns `ProviderError::InvalidRequest("no candidate could honour request capabilities; skipped: <id>: [<reasons>], …")` — a `Permanent` class error, retry won't help.
- **Why this shape (vs codex's `SafetyCheck` 3-state)**: codex's Skip/Reject is a per-action verdict. Per-candidate compatibility doesn't tell us whether the *request* is malformed globally — only the routing layer (which sees all candidates) knows that. Keeping per-candidate at 2-state and letting the loop aggregate is cleaner.
- **Tests**: 7 new unit tests in `tars-types::chat::tests` (each cap field individually + multi-reason aggregation), 3 new routing tests in `tars-pipeline::routing::tests` (skip-and-try-next / all-skipped-returns-InvalidRequest / pass-through-when-compatible).
- **Behavior change for callers**: requests with tools/vision/thinking that previously wire-400'd or silently drop'd at non-supporting providers now get clean local skips. downstream consumer dogfood will see fewer mysterious provider errors when routing has heterogeneous candidates.

#### B-32. Context length pre-flight check — ⚠️ partially shipped (chars/4 heuristic, full fix pending tokenizer)
- **What shipped (in B-31 v2)**: `compatibility_check` adds a `ContextWindowExceeded { estimated_prompt_tokens, max_context_tokens }` check, estimating prompt size with the `chars / 4` heuristic. Covers obvious-overflow scenarios: a 200k-char request sent to a 32k-context provider is skipped by routing, no wire round-trip wasted.
- **What NOT shipped**: real tokenizer-based precise detection. The current chars/4 heuristic has ±20% error at the boundary (estimate ≈ 80-100% of max), so it only reliably catches "obviously over" requests. Borderline cases still go to the wire and let the provider error out.
- **Trigger for full fix**: real tokenizer integration (D-5 unfreezes, hooking up tiktoken / model-specific tokenizer). Or the downstream consumer actually hitting borderline cases and getting frequent false negatives that slow down debugging.
- **Current state**: 80% of the practical value is already in (chars/4 avoids wire round-trip waste), 20% precision case stays on wire-level fallback. **Sufficient; not moving until the trigger fires**.

#### Middleware-order trap (folded into B-20 W1.2, not a separate entry)
- **Where**: `middleware.rs:96-98` — `PipelineBuilder::layer` docs say "first call adds outermost", **but it's only a doc convention, with no build-time check**
- **Gap**: a developer placing Telemetry inside vs. outside Retry gets completely different observability results, invisible at compile time. Known anti-patterns:
  - Telemetry inside Retry → records N discrete attempts, doesn't know they're the same business request
  - Telemetry outside Retry → records 1 total duration, doesn't know how many internal retries happened
  - CacheLookup inside Retry → cache hit still goes down the retry path, pointless
  - CircuitBreaker outside Retry → still retries after tripping, violating circuit-breaker intent
  - Validation outside Retry → retries triggered by ValidationFailed can't reach the outer layer
- **Fix**: `PipelineBuilder::build()` gains a `validate_order` static check — known anti-patterns hardcoded in a lookup table; violations panic with a helpful message:
  ```rust
  fn validate_order(&self) -> Result<(), BuildError> {
      const ORDER_RULES: &[(name_outer, name_inner, reason)] = &[
          ("telemetry", "retry", "telemetry inside retry records per-attempt; flip them"),
          ("cache_lookup", "retry", "cache hit shouldn't enter retry path"),
          ("retry", "circuit_breaker", "circuit-broken provider shouldn't trigger retry"),
          ("retry", "validation", "ValidationFailed needs retry to wrap it"),
      ];
      // check pairs...
  }
  ```
- **No strong-typed typestate**: keep extensibility for arbitrary layer combinations; only intercept on known anti-patterns.
- **Folded into B-20 W1.2** as a sub-task of Pipeline.builder() build-time validation, **no new entry**.

### B-7. `tars-storage` — `ContentStore` + `KVStore` (EventStore done)
- `EventStore` + `SqliteEventStore` shipped — see CHANGELOG. Two traits still pending:
  - **`ContentStore`** — large-blob refs (image bytes, long-context payloads, raw LLM responses for parser-rewind replay). Slots in once `AgentEvent` payloads need to grow beyond the 4 KiB inline budget Doc 04 §3.2 sets, AND once we add the second `LlmResponseCaptured` event variant (separate from `LlmCallCaptured`) that carries the raw bytes.
  - **`KVStore`** — generic small-value persistence. Slots in when BudgetMiddleware (B-2 cap) needs cross-restart token-bucket state, OR when `tars-cache`'s SQLite L2 wants to be deduped onto a generalised KVStore. Today's `SqliteCacheRegistry` is fine standalone; refactoring just to share scaffolding would be O-style overengineering.
- **Postgres impls** for both EventStore + the future ContentStore/KVStore: M6 work (Doc 14).
- **Trigger**: ContentStore = first agent emits a payload that won't fit inline. KVStore = BudgetMiddleware or Tools idempotency table needs persistence.

### B-8. Full `tars-melt` (metrics, OTel exporter, cardinality validator, `SecretField<T>`)
- Mini version shipped — see CHANGELOG. Pending for M5 (Doc 14 §11): all metrics from Doc 08 §5, OTel SDK + OTLP exporter, cardinality validator, `SecretField<T>` generic wrapper (today `SecretString` covers the only consumer), trace head + tail sampling, `AdaptiveSampler`.
- **Trigger**: M5 starts (Doc 14 calls for it concurrent with CLI/TUI work).

### B-9. `tars-tools` — additional builtins + MCP + tool-call mini-pipeline
- Crate skeleton + `Tool` trait + `ToolRegistry` + `fs.read_file` + `fs.list_dir` + WorkerAgent integration shipped — see CHANGELOG. **Still missing**:
  - **Additional read-only builtins**: `git.fetch_pr_diff`, `web.fetch`. Each is mechanical — same pattern as the shipped `fs.*` tools. Trigger per item: first Worker run where the existing `fs.*` set isn't enough (typically a goal involving "look at git history" or "check this URL").
  - **`fs.write_file`** — gated on Backtrack + Saga (B-4). Writing without a rollback story is exactly the failure mode "tool ran, side effect committed, downstream step failed, no way to undo" we want to avoid normalising. Specifically: `fs.write_file` ships **after** `AgentEvent::CompensationExecuted` exists.
  - **`shell.exec`** — biggest blast radius. Ships **last**, with an explicit allowlist of binaries + jail + per-command audit log. Don't add until Saga + IAM both exist (B-4 + `tars-security` M6).
  - **Tool-call mini-pipeline** (Doc 05 §3.3) — onion of IAM check / idempotency dedupe / budget / audit / timeout around `ToolRegistry::dispatch`. Today's dispatch is bare. Each layer has its own consumer:
    - IAM check → blocked on `tars-security` (Doc 14 M6).
    - Idempotency dedupe (per-tool, distinct from `StepIdempotencyKey`) → blocked on KVStore (B-7).
    - Budget → blocked on BudgetMiddleware (B-2).
    - Timeout → could ship now; defer until first long-running tool.
    - Audit → could ship now; pairs naturally with `tars-melt` metrics (B-8).
  - **MCP integration** (Doc 05 §5) — load external tool servers over the standard MCP protocol. Big surface; defer until either (a) a user has a specific MCP server they want to plug in, OR (b) we hit the wall of "writing built-ins for everything is unsustainable".
- **Trigger / order per item above**.

---


## Doc 01 — LLM Provider gap items

Audit run 2026-05-03 against `docs/architecture/01-llm-provider.md`. Code currently implements ~85% of the doc surface (HTTP + CLI + capability + tool-call + structured-output + cache directive + error model + registry are all in). What's still missing:

> **Vocabulary in this section** — drawn from the `defer > delete > implement` lifecycle:
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
- **Trigger**: M6 (Multi-tenant + Postgres + security) per Doc 14.

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
- All HTTP adapters currently classify context overflow as `ProviderError::ContextTooLong { limit: 0, requested: 0 }` — typed correctly but with placeholder numbers. Doc 01 §11.1 says these fields enable "the upper layer has an explicit handling path (truncate / summarize)". Without the numbers, callers can't make the truncation decision intelligently.
- **Where**: `crates/tars-provider/src/backends/{openai,anthropic,gemini}.rs` — `classify_error` paths.
- **Why deferred**: Each provider's error message format is different and changes without notice. Real fix is "regex over the message body" — hacky but unavoidable.
- **Trigger**: First time the agent loop hits a long-context request and the truncation policy needs the actual numbers (not just the error class). Until then `0/0` is honest about "we know it overflowed, we don't know by how much".

### D-8. Routing layer (Doc 01 §12) — ⏳ partial: M2 cut shipped, advanced policies pending
- **Shipped (a4ebba9)**: `RoutingPolicy` trait + `StaticPolicy` + `TierPolicy` + `RoutingService` (bottom-of-pipeline LlmService). FallbackChain inlined into `RoutingService.call`'s try-each loop — simpler than a wrapper type. CLI `--tier` flag + config `[routing.tiers]` section. `CircuitBreaker` (caf0043) pairs naturally: when a candidate's breaker opens, the typed `ProviderError::CircuitOpen` (Retriable) makes routing fall through automatically.
- **Still pending — all blocked on metrics infra (B-8 / M5)**:
  - `CostPolicy` (per-provider cost tracking)
  - `LatencyPolicy` (per-provider P50 tracking)
  - `EnsemblePolicy` (parallel fan-out + merge — also needs a merge primitive)
- **Tied to**: O-4 (Capabilities slimming) — routing's actual reads decide which fields stay. After M3 lands, audit Capabilities against routing usage.

### D-12. CLI-provider conformance (Doc 01 §14 follow-on) — ⚡ trigger fired
- The HTTP-backend conformance suite (D-9, shipped) doesn't cover `claude_cli` / `gemini_cli` / `codex_cli` — their wire path is fundamentally different (no SSE, no HTTP; subprocess JSON). They'd need a `Scenarios` impl that mounts a fake subprocess runner instead of wiremock.
- **Trigger fired** with codex_cli (`a4e2254`): the 3rd subprocess-style backend has landed. We're paying the per-backend test maintenance cost across 3 separate test files (`*_smoke.rs` for live + per-backend unit tests for mock paths).
- **Each CLI backend's existing tests** (with `FakeRunner` for unit + `*_smoke.rs` for live) DO cover its specific surface. The `Scenarios` harness would dedupe the cross-backend invariants (env-strip pattern, JSON-or-text decoding, cancel-via-Drop, timeout handling, model-not-supported error → ProviderError). Worth ~1 day of refactor; payoff is "add the 4th CLI backend in 100 lines instead of 500".
- **Updated trigger**: when adding a 4th CLI backend (e.g. Cursor CLI, Cline CLI, ChatGPT app's hidden CLI, etc.) — at that point the dedupe pays for itself in one go.

### D-13. Live-API nightly conformance tier (Doc 01 §14)
- Doc 01 §14 calls for a nightly CI tier hitting REAL APIs (~$0.01/run) so the `Scenarios` wiremock fixtures don't drift from the actual provider behaviour. This is the safety net that catches "OpenAI changed the streaming format last week and our fixtures are now lying".
- **Why deferred**: needs a budget mechanism + secret management for the API keys + a separate CI workflow. No urgency until provider-side breakage actually happens.
- **Trigger**: First confirmed wire-format change at any of OpenAI / Anthropic / Gemini that our local conformance suite missed because the fixture was stale.

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
- **D-1 ExplicitCacheProvider**, **D-2 Auth::SecretManager / GoogleAdc**, **D-7 ContextTooLong numbers**, **D-12 CLI-provider conformance**, **D-13 live-API nightly tier** — all still expected to land; the trigger conditions are concrete and likely to fire within v1.0 timeline (Doc 14 M3-M6).
- **D-8 Routing** + **D-9 conformance suite** are partially or fully shipped — see CHANGELOG. Remaining sub-items (Cost / Latency / Ensemble policies; CLI + nightly conformance tiers) live in their own entries above.

---

## Process notes

- **Two files, two roles**:
  - `TODO.md` (this file) is forward-only: deferred / frozen / audit-deferred items + the trigger conditions for each. Reading top-to-bottom answers "what's NOT done and why".
  - `CHANGELOG.md` is the shipped-items audit trail organized by milestone. Reading top-to-bottom answers "what IS done, in roughly what order".
- **Don't delete deferred items silently** — they're institutional memory ("we considered this and decided X"). Items move OUT of TODO.md only when:
  - shipped → relocated to CHANGELOG.md (with the trigger marked satisfied), OR
  - explicitly decided "never doing this" → strikethrough + a one-line "why" stays here. Don't delete; the strikethrough is the audit signal.
- **Trigger conditions are real** — when one fires, open the corresponding work, don't just shuffle the line.
- When in doubt: `defer > delete > implement`. Premature deletion is also overengineering.
- **Deferred ≠ Frozen**. Deferred = expected to ship. Frozen = explicit "no" with thaw conditions. See D-11 for the current frozen list.
