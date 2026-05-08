# TODO

Forward-looking list. Each entry: **what** to do, **why** it's deferred (not "shouldn't", just "not now"), and a **trigger** for when to revisit.

**For shipped items**, see [CHANGELOG.md](./CHANGELOG.md). For the day-to-day commit history, `git log`. This file is "what's NOT done and why".

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
  - **B-6c. PyO3 `PipelineBuilder` for custom middleware** — currently `Pipeline.from_default()` uses a hardcoded layer order (telemetry → cache_lookup → retry → provider). Python can't inject a custom middleware. **Trigger**: first consumer that wants e.g. a custom rate-limit layer or auth-refresh layer specifically from the Python side. ARC and other near-term consumers don't need this.

### B-15. Stage 4 — `Response.telemetry` per-call observability surface — ✅ shipped (`<unreleased>`)
- See CHANGELOG M8 for shipping detail. Surface: `Response.telemetry.{cache_hit, retry_count, retry_attempts, provider_latency_ms, pipeline_total_ms, layers}`. Plumbed via `RequestContext.telemetry: SharedTelemetry` so every middleware writes through the same Arc<Mutex<...>>. Session.send aggregates across the auto-loop's multiple model calls under one handle.
- **Out of scope (preserved as future)**: full OTel exporter (B-8); per-HTTP-attempt visibility (codex exposes `attempt: u64` — tars aggregates retries inside the middleware; revisit if real debugging need shows up); `CallObserver` push-trait (B-18).

### B-16. Session ↔ EventStore integration (durability + multi-agent blackboard)
- **Where**: `tars-runtime/src/{session,event}.rs`, plumbing into existing `tars-storage::EventStore`.
- **What**: Plug `Session` into the existing trajectory + EventStore pipeline rather than build a parallel `SessionStore`. Session optionally takes `Arc<dyn EventStore>` + `TrajectoryId`; emits `AgentEvent` variants for turn lifecycle (TurnOpened / TurnCommitted / TurnRolledBack / ToolCalled / HistoryTrimmed / HistoryReset). `Session::resume(store, trajectory_id)` reads back the trajectory and rebuilds the in-memory `Vec<Turn>`.
- **Why this shape**: tars already has the trajectory (event log, in `tars-runtime`) + blackboard (`EventStore`, in `tars-storage`) two-layer split — agents emit events, store persists. Session is just another agent-shaped thing emitting into the same stream. A new `SessionStore` trait would be parallel infrastructure for the same concern. Multi-agent scenarios (Orchestrator + Worker + Critic each owning a Session writing to the same store) become natural under this model. Same architectural pattern codex-rs uses (their `ThreadStore` is the blackboard, `LiveThread` the agent-side handle).
- **Note on Turn-as-data vs Turn-as-projection**: long-term `Vec<Turn>` should become a derived projection over the event log rather than the primary state, because true async conversations (multi-agent cross-writes, webhook event injection, long-running async tools) don't map cleanly onto strict turn boundaries. Current Turn-as-data is the right pragmatic call (ARC's 80-line Session is turn-based; ARC has none of the async patterns yet); after B-16 lands, the migration to "Turn = `fn turns(events) -> Vec<Turn>` view" is a small refactor since events are already primary. **Don't pre-build the projection now** — wait for the first async consumer.
- **Trigger**: First long-running ARC review where mid-process restart loses 80% of work. Or first multi-agent shared-conversation scenario. Until then, in-memory-only Session is fine.

### B-17. Optional — LLM-summarize compaction (codex-style)
- **Where**: New module under `tars-runtime/src/session/compact.rs` + integration into `Session::trim_to_budget`.
- **What**: When trim would otherwise drop turns, instead invoke the model with a `SUMMARIZATION_PROMPT` to generate a summary of the dropping section, replace those turns with the summary turn. Preserves semantic intent at cost of an extra LLM call.
- **Why deferred**: Current chars-budget trim is "drop oldest whole turn" which is brutally simple but works fine for ARC's profile (review batches don't hit 100k tokens of useful history anyway — at that scale you usually want a fresh Session per PR not a long-lived one). Compaction has real LLM-call cost + risk of summary losing key details.
- **Trigger**: First user complaint that "long agentic loop dropped a critical detail in trim". Or first product where 50+ turn conversations are normal (chat product, not batch reviewer).
- **Pattern reference**: `codex-rs/core/src/compact.rs` has the production-ready version including `InitialContextInjection::BeforeLastUserMessage` semantics for mid-turn invocation.

### B-18. Optional — `CallObserver` trait (rust-side push hook) — ❌ 撤回
- ~~原 design：trait + push callback。~~
- **撤销原因**：B-20 (Evaluation Framework) 用 EventStore stream 解决了 cross-call 聚合的同一类问题，且解耦更彻底（pipeline ↔ aggregator 通过 events 而不是 trait callback 耦合）。CallObserver 写出来会跟 EvaluatorRunner 形成两条同质机制。
- **如果你需要"跨 pipeline 跨 call 聚合 metric"** → 用 B-20 的 OnlineEvaluatorRunner，不是 CallObserver。

### B-20. Output Validation + Evaluation Framework — ⭐ 优先级最高（M9）
- **设计文档**: [Doc 15 — Output Validation](./docs/15-output-validation.md) + [Doc 16 — Evaluation Framework](./docs/16-evaluation-framework.md)
- **拆分**(2026-05-05 review 后调整,3-wave 降低 PyO3 单点风险):
  - **Wave 1 (Rust-only Validator framework)** — ✅ shipped 2026-05-07. `OutputValidator` trait + `ValidationOutcome` enum + `ProviderError::ValidationFailed` + 3 built-in validators (JsonShape / NotEmpty / MaxLength) + `ValidationMiddleware` + `Response.validation_summary` 字段 + `RequestContext.validation_outcome` 侧信道 + 17 单元测试。详见 CHANGELOG B-20 W1 段。
  - **Wave 2 (PyO3 binding)** — ✅ shipped 2026-05-08. Python validators 通过 `[(name, callable), ...]` 挂到 `Pipeline.{from_default,from_config,from_str}`。`PyValidatorAdapter` 把 Python callback 桥接成 Rust `OutputValidator` trait；4 个 outcome pyclasses (`tars.Pass / Reject / FilterText / Annotate`)。Buggy validator (raise / wrong return type) 自动 catch 成 permanent `ValidationFailed` — worker 不会被 user-side bug 打死。17 个 pytest in `crates/tars-py/python/tests/test_validators.py`。详见 CHANGELOG B-20 W2 段。
  - **Wave 3 (ARC 接入 + Evaluation framework Doc 16, ~7.5 天)** — Doc 16 完整实施(`Evaluator` / `AsyncEvaluator` traits + `LlmCallFinished` / `EvaluationScored` events + `OnlineEvaluatorRunner` / `OfflineEvaluatorRunner` + Built-in evaluators + tars-py `tars.eval.Evaluator` base + `Pipeline.with_event_store` API + SQL templates),ARC 删 inline `_known_rule_ids` 并切到 Pipeline-attached validator + dogfood。
- **关键设计决定 (Cache × Validator 交互, W1 实施时锁定 — ⚠️ 实现与设计不一致，W4 修复)**:
  - **设计意图**: Cache stores raw Response (pre-Filter)。Cache hit 仍跑 validator chain。validator 是 pure，重跑 = CPU local cost only，远比 wire round-trip 便宜。多 caller 共享 cache 安全。改 validator 不改 cache key。Validator failure NOT bypass cache。
  - **W1 实现的 bug** (arc 2026-05-08 dogfood flag 引发的 audit 找到): `ValidationMiddleware` Filter 时把 stream re-emit 成 post-Filter events (`validation.rs:225-232`)，cache 看到的就是 post-Filter 流。**任何 Filter validator + Cache 同时存在 → cache 存的不是 raw**。multi-caller 不同 validator 链 → silent corruption；单链情况下 cache 也永远拿不回 raw。Side channel `rec.filtered_response` 已经存在但被冗余化了。
  - **修复 → 见 B-20.W4**。在那之前，arc / 任何 multi-role consumer 必须 per-role 独立 Pipeline 实例，不要复用同一 Pipeline + 不同 validator 链。
- **Why 这个排在 B-16 / B-17 / B-19 前面**:
  - ARC dogfood (2026-05-04 / 05) 暴露的两类痛点都在这里解：(a) 模型造 rule_id / 漏 evidence tag → validation；(b) "metrics 突然掉了我们看看怎么回事" → evaluation。
  - ARC 现在 inline 实现了 `_known_rule_ids` post-filter (见 ARC commit `1fe6cbc`)，是 v1 validation 的占位实现 — 等 Doc 15 落地直接 migrate 出来。
  - 整个 LLM 系统的 observability + quality gating 是 cross-consumer 基础设施，比单产品功能（compact / tui）优先级高。
- **依赖**:
  - 依赖 `Pipeline.builder()` API 暴露到 Python (内部 B-6c) — 这一条作为 Doc 15 / Wave 1 的子任务一起做。
  - 依赖 EventStore 在 Pipeline 层可用 — 当前只在 tars-runtime 用，需要把 `Arc<dyn EventStore>` 接到 Pipeline 上。
- **预估总工作量**: 12 天 (两个 wave 加起来)，可分 wave 出 wheel。
- **与 B-15 (Stage 4 Telemetry) 的关系**: 互补不重叠。`Response.telemetry` 装 infrastructure 指标 (cache_hit / retry_count / latency)；evaluation 装 semantic 指标 (rubric grounded rate / evidence filled rate)。仪表板可以 cross-join 两者出"指标突然掉的同时 retry_count 涨了吗"这种问题。
- **LangSmith borrow points (落进 W1.1 / W2.1 一起做,不单独 backlog)**:
  - **Tags 字段** — `LlmCallFinished.tags: Vec<String>` + `EvaluationScored.tags: Vec<String>`,事件 schema 一开始就带,默认空。caller 通过 `RequestContext::with_tags()` / `Session::tagged()` helper 打标。Cohort 分析靠 `WHERE 'dogfood_2026_05_05' IN tags` 一句 SQL,远比每加一种过滤维度加一个事件字段干净。
  - **OnlineEvaluatorRunner sampling 配置** — `EvaluatorSampling::{Always, Rate(f64), Stratified, OnDimDrop}` 四种模式。`Always` 是 deterministic evaluator 默认；`OnDimDrop { watch_dim, threshold }` 是 LangSmith 没有的智能采样——便宜 evaluator 持续跑,贵的(LLM-as-judge)只在另一个 dim 掉到阈值下时触发,**节省 LLM-judge 的真钱**。OnDimDrop 写进 trait,即使 v1 默认 `Always`,接口为未来留位。

### B-20.v3. Python `Response.validation_summary` 暴露 — ⭐ arc dogfood 报表回归门必需 (~1h)
- **现状**: Rust 侧 `ChatResponse.validation_summary: ValidationSummary { outcomes: BTreeMap, validators_run: Vec, total_wall_ms: u64 }` 已经填好。Python `Response` pyclass **没暴露**这个字段。caller 只能从 `r.telemetry.layers` 看到 "validation 跑没跑过"，看不到哪些 validator / 谁 dropped 多少 / 多少 wall time。
- **shape**:
  - 在 `tars-py/src/lib.rs` 的 `Response` 上加 `validation_summary` getter，返回 frozen pyclass `ValidationSummary{validators_run: list[str], outcomes: dict[str, dict], total_wall_ms: int}`。`outcomes[name]` = `{"outcome": "pass"|"filter"|"annotate", "dropped"?: list[str], "metrics"?: dict}`。
  - 跟 `Telemetry` 同样模式 — frozen pyclass, get_all。
- **预估**: ~1 小时。从 `ChatResponse.validation_summary` → PyValidationSummary 的纯映射。
- **Trigger**: arc Tier 1 #1 (snippet validator) 落地后立刻 ship。没它 cross-run 比较 "snippet validator 丢了几条" 拿不出数。dogfood 报表的 metrics 列、ship signal 全卡这。
- **依赖**: 无。
- **由来**: arc 2026-05-08 反馈。

### B-20.v2. Typed `ValidationOutcome::Reject { reason: ValidationReason }` — ⭐ unblocks arc parse→structured pipeline (1-2 d)
- **现状 (W1+W2 shipped 后)**: `Reject { reason: String, retriable: bool }` — string-only。Python 侧 `TarsProviderError(kind="validation_failed", is_retriable=bool)` 只把 reason 字符串塞进 message。caller 没法 programmatic match 失败原因。
- **inconsistency**: B-31 v4 已经把 `CompatibilityReason{kind, message, detail_json}` 做成 typed enum + structured detail。validator 失败也该一致 — 不然 fix-stage 又得 grep `e.message`，回到 B-31 v1 那种字符串脆弱契约。
- **shape**:
  - 引入 `ValidationReason` enum (`#[non_exhaustive]`)：`JsonShape{json_path, parse_error}` / `NotEmpty{field}` / `MaxLength{field, length, max}` / `Custom{kind: String, message: String, detail: Option<serde_json::Value>}`。
  - 内置 validator 用对应 typed variant；Python user-side validator 走 `Custom` (caller 给 kind+message+detail)。
  - Python 兼容入口: `tars.Reject(reason=str)` 自动包成 `Custom{kind="user", message=reason, detail=None}`；新增 `tars.Reject.typed(kind, message, detail=None)` 显式 typed 路径。
  - `ProviderError::ValidationFailed { validator, reason: ValidationReason, retriable }`；Python `TarsProviderError` 加 `validation_reason: dict` 属性 (`{kind, message, detail}`) 给 caller programmatic 访问。
- **预估**: 1-2 天。改动跨 `tars-types/validation.rs` + `tars-pipeline/validation.rs` + 3 builtin + `tars-py/{validation.rs, errors.rs}`。需要 deprecate-not-break 现有 `reason: str` 入口。
- **Trigger**: arc 开 Tier 2 #4 (parse → structured pipeline) 之前必须 ship。Tier 1 #1/#2/#3 用 FilterText 路径不阻塞，可以并行落。
- **依赖**: 无。
- **由来**: arc 2026-05-08 反馈，详见 conversation log。

### B-20.W4. Cache × Validator interaction fix — ⚠️ 真 bug，结构性改动 (1-2 d)
- **failing regression tests 已锁定 (commit `ce6aa95+`)**: `b20_w4_cache_stores_raw_not_post_filter` + `b20_w4_cache_hit_reruns_validator_chain` 在 `tars-pipeline/src/validation/tests.rs`，标了 `#[ignore]`。`cargo test -- --ignored` 验证两条都 fail。W4 fix 删 `#[ignore]` 同时改代码 → 两条变绿。
- **bug 1 (cache 存 post-Filter)**: `ValidationMiddleware` 在 Filter 改写 response 后 re-emit post-Filter events (`tars-pipeline/src/validation.rs:225-232`)，Cache 看到的是 ValidationMiddleware re-emit 之后的 stream，于是 cache 存 post-Filter。test 1 断言 cache 应存 raw "hello world"，实际是 "hello"。
- **bug 2 (cache hit 不跑 validator)**: 当前 onion 顺序 `Telemetry → CacheLookup → Retry → Validation → Provider`，Cache 在 Validation 外层。Cache hit 直接短路返回 cached events，**Validation 根本不被调用**。test 2 断言第二次（hit）`telemetry.layers` 含 `"validation"`，实际不含。这条比 bug 1 严重 — W1 doc §2 "validators rerun on hit" 跟 onion 不兼容。
- **后果**:
  - multi-caller 不同 validator 链共享同一 Pipeline + cache: 第二个 caller cache hit 拿到的是第一个 caller filter 过的内容，且新 validator 链不会跑 — silent corruption。
  - 单 validator 链情况下: cache 永远拿不回 raw；换 validator 配置后 hit 仍返回老 cached payload + 不重跑新 validator → 配置改动等于隐性 SemVer break。
  - W1 doc §2 "Cache stores raw Response (pre-validation), validators rerun on hit" 与实现两条都不一致。
- **fix 选项 (须选一)**:
  - **A. 改 onion 顺序**（推荐）: 移到 `Telemetry → Validation → CacheLookup → Retry → Provider`。Validation 在 Cache 外面 → cache hit 仍走 Validation。同时 ValidationMiddleware 不再需要 re-emit raw vs filtered 分支（Cache 看不到 Validation 输出）。代价: `ValidationFailed{retriable:true}` 不再触发 `RetryMiddleware`（Validation 在 Retry 外）。要么把 retry 逻辑挪进 ValidationMiddleware 自己，要么接受 "validation-driven retry 不存在" 的语义（更干净）。改动: 调 PipelineBuilder 调用顺序 (~3 处 caller)、调 Doc 02 onion 图 + Doc 15 §2、删 ValidationMiddleware 里的 `filtered_any` re-emit 分支、删现有"validation 从 cache hit replay drain"那段注释。
  - **B. 维持 onion + 仅修 re-emit**: 让 ValidationMiddleware 始终 re-emit `events_held`(raw)。**只修 bug 1，不修 bug 2** — cache hit 仍跳过 validator。doc 必须明写 "validators only run on cache miss" 这条限制。代价低但 W1 设计契约的 "rerun on hit" 永远做不到。
  - 选 A — Tier 1 落地前必须解决 multi-chain 安全；B 把"caller 必须保证 cache 命名空间隔离"的负担推给 arc，又得在每个 consumer 重复一次。
- **预估**: 选 A → 1-2 天（onion 改动 + 5 处 doc 同步 + retry 语义决定 + 验证 W1 17 个 unit test 仍通过）。选 B → 半天。
- **Trigger**: arc Tier 1 #1 (snippet validator) ship 之前必须修。
- **依赖**: 无。
- **由来**: arc 2026-05-08 raised "single-validator-chain assumption" flag → tars 端 audit + 写 failing test 发现实际 bug 比 audit 想的严重一层（不止"chain 不一致 corruption"，是"任何 Filter + Cache 共存 + Cache hit 不验证"）。

### B-19. `tars-tui` — interactive terminal UI (path C: build-our-own, not fork-codex)
- **Where**: New crate `crates/tars-tui/` (doesn't exist yet). Consumer of `tars-runtime::Session` + `tars-pipeline::Pipeline`. ratatui-based.
- **What**: Interactive terminal frontend for `tars chat`-style multi-turn conversations. v1 scope: chat history rendering, streaming markdown tokens, tool-call display (folded → expanded), slash commands (`/clear` / `/fork` / `/save` / `/quit`), status bar (model / usage / cache hit / latency), multi-line input with editing shortcuts. Sized at ~3-5k lines for v1.
- **Why "build our own" — codex's TUI is not directly reusable**:
  - codex's `tui/` is **57,736 lines / 102 files** and talks to codex's runtime through `app-server-protocol` — an **18,889-line type surface** assuming codex-specific concepts: rollout files, sandbox events, MCP tool dispatch, approval workflows, apply_patch notifications, ChatGPT auth modes, personality/skill/plugin injection. Not portable abstractions; product-specific to codex.
  - **Path A (implement codex's app-server-protocol on tars)** — rejected. Maps to "build all of codex inside tars" — sandbox + MCP + apply_patch + approval + ChatGPT + personalities. ~8-12 weeks. End state: tars becomes codex-clone, loses its library identity.
  - **Path B (fork codex TUI, swap backend)** — rejected. ~70% of those 102 files are codex-product UI (voice / approval / MCP / apply_patch / theme picker / onboarding / multi-agents / realtime / collaboration_modes / etc.) that tars doesn't have backends for. Remaining ~30 files are coupled to codex's event types (chatwidget.rs alone is 11k lines around codex's specific ChatEvent shape) and need rewriting. Net: ~3-4 weeks of work plus permanent fork-maintenance debt as codex iterates.
  - **Path C (build our own, borrow only pure-rendering utilities)** — chosen. Cherry-pick codex's `markdown_render.rs` + `markdown_stream.rs` + `transcript_reflow.rs` + `wrapping.rs` + `streaming/` + `slash_command.rs` (these are pure rendering, no runtime coupling) as utility libraries with attribution. Write own app loop / chat widget / input box / status bar around tars's Session API. ~2-3 weeks to v1, no maintenance debt.
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

## Brainstorm 存盘 (Day-2 + Day-3, 2026-05-05)

下面 7 条是 ARC dogfood 反馈 + 跨工程师 brainstorm 期间提到的"未来需要但当前不挡路"的架构方向。**全部明确不在 M9 范围**——M9 只做 B-20（Validation + Evaluation）。这些 brainstorm 落盘给将来真有 trigger 时翻出来对照用。

### B-21. OpenTelemetry distributed tracing exporter
- **What**: 在 `tars-melt` (M5 本就规划) 落地完整 OTel exporter。tars 内部 `tracing::*` 事件 → OTLP → Jaeger / DataDog / Grafana Tempo。带 session_id → turn_id → span_id 的 hierarchical context propagation。
- **半成品现状**: TelemetryMiddleware 已经在发完整 tracing event；缺的是 `tracing-opentelemetry` + OTLP exporter。约 1.5 周。
- **Trigger**: 多阶 agent 调用（orchestrator + worker + critic）的 timeline 调试痛了——光看 `pipeline_total_ms` 不够，要瀑布图。ARC dogfood 可能是第一个 user。
- **Pattern reference**: codex-rs/otel/ 整个独立 crate，OTLP / metrics / traces 都齐了，可以照抄结构。
- **关键设计 — 不能只做 flat trace_id (LangSmith run-tree 借鉴)**:
  - tars 当前 `RequestContext.trace_id` 是扁平的——一整个请求一个 id,**没有 parent-child 关系**。multi-step agent 跑完看不出哪一阶段花多久。
  - LangSmith 的 run tree 模型 (每个 LLM/tool call 是一个 run,带 parent_run_id 形成树) 是这一片观测层最值得抄的形态。OTel 的 span 模型本来就是这棵树。
  - 实施时**必须**给 `RequestContext` 加 `span_id: SpanId` + `parent_span_id: Option<SpanId>`,新事件 `SpanStarted` / `SpanFinished` 进 EventStore,每层 middleware / agent / tool 进入退出都打。
  - 落地后:Jaeger 瀑布图 + SQL `WHERE op='critic.review'` 直接查"过去 1d critic.review 这个 op 平均花多久"——比 `pipeline_total_ms` 一个总数有用得多。
  - 不做这一层,B-21 就退化成"加一个 OTLP exporter"——半成品,真用户拿到瀑布图发现"trace 全是孤立点没有结构"。
- **codex 借鉴清单 (实施时一起带,不单独 backlog)**:
  - **W3C Traceparent 跨服务传播** —— codex `otel/src/trace_context.rs:19-36`:`set_parent_from_w3c_trace_context(headers)` + `current_span_w3c_trace_context() -> W3cTraceContext` + `traceparent_context_from_env()`。tars 当前 `trace_id` 是内部生成,无法跟上下游(ARC 嵌进 web app / ARC 被 RPC 调起)的已有 trace 串起来。**实施时加 `RequestContext::from_traceparent` / `to_traceparent`**——tars trace 跟外部 Jaeger/DataDog 自动衔接。
  - **Dual-stream event macros** —— codex `otel/src/events/shared.rs:4-52` 提供 `log_event!` / `trace_event!` 两个宏 + target prefix 约定。同事件 emit 两次按 target 路由到不同后端(logs → file/Loki; traces → OTel span)。当前 tars `tracing::info!` 一锅端没法分流。**实施时建 `tars_melt::{log_event!, trace_event!}` 两个宏 + 标准 target 前缀约定**。
  - **Metrics naming taxonomy** —— codex `otel/src/metrics/names.rs:1-48` 集中 48 个 metric 常量,层级命名:`<subsystem>.<entity>.<measure>_<unit>`(`pipeline.turn.e2e_duration_ms` / `provider.responses_api.ttft_duration_ms`)。tars 现在 `pipeline_total_ms` / `provider_latency_ms` 风格不一致,半年后会 churn。**实施时做一个 `tars_metrics::names` 模块集中常量**,去掉 codex 的 `codex.` 前缀,采用同样的层级 taxonomy。

### B-22. Shadow Replay — 模型替换防退化体系
- **What**: 把生产 EventStore 中代表性 trace 标记为 `golden`；新 `ShadowRunner` 重发这些请求到候选 provider/model，背靠背 diff 评分。CLI: `tars shadow --dataset regression-v1 --provider gemini-3 > report.json`。
- **复用基础**: 90% 跟 OnlineRunner / OfflineRunner 共代码，仅多一个"重发 + diff"模式 + LlmCallFinished `tags` 字段(已在 B-20 加进 schema)。约 4-5 天（在 B-20 落地之后）。
- **Trigger**: 第一次模型替换（OpenAI 暗改 / 想切 Gemini-3 / 想切本地）。当前 ARC 已经在讨论 gemini-3-flash-preview 切换。
- **Why 不进 M9**: 需要 EventStore + Evaluator 已经稳；过早做就是空架子。
- **LangSmith borrow points (B-22 实施时一起带)**:
  - **`PairwiseEvaluator` trait** — 单 response 评分 (`Evaluator::score`) 之外加一个 pairwise 接口 `compare(req, a, b) -> A | B | Tie + confidence`。**Shadow Replay 的核心动作就是 pairwise** ("切到 gemini-3 后比之前好还是差?"),没这接口做不了。新事件 `PairwiseScored { trace_id_a, trace_id_b, evaluator_name, verdict }` 写 EventStore。
  - **`Dataset` 一等 typed 对象** — 不是"一堆 jsonl 文件"或"一组 trace_id 临时变量"。`Dataset { id, name, version, trace_ids, metadata }` 持久化在 EventStore,API 包括 `create_dataset` / `fork_dataset` / `dataset_traces`。`tars dataset create --name regression-v1 --tag dogfood_2026_05_05 --schema-compliance ">0.8"` 一句话从 production trace 沉淀出 regression set。比 LangSmith 的 hosted Dataset 弱一些(没 UI 加例子)但跟 tars library positioning 一致。
  - 这两条**都是 Shadow 的硬依赖**——B-22 实施 spec 必须包含。

### B-23. Circuit Breaker → Routing fallback 最后一公里
- **What**: tars 已有 `CircuitBreakerMiddleware` + `Routing` layer（M2 shipped），但"circuit_open → 自动 fallback 到下一个 candidate"的 wiring 可能不完整。验证 + 补全，让"主 provider 熔断 → 自动切 candidate"真正可用。
- **Trigger**: 第一次跨 provider fallback 需求。ARC 当前 critic 退化时还是手工配置降级，自动化是优化。
- **估时**: 1-2 天补 wiring + 写测试。

### B-24. Prompt Registry / A/B Routing
- **What**: Prompt-as-code，远程下发（从 git / 配置中心），SemVer 版本号；`Router` 层支持按 prompt_version 比例分流；`LlmCallFinished` 带 `prompt_version`，走 EventStore SQL 直接出 v1 vs v2 评分对比。
- **当前**: ARC 把 prompt 写死在 Python 文件里，改 prompt 要发 PR。规模化后这条会痛。
- **Trigger**: 团队扩大到 prompt 改动需要灰度 / 多人并行迭代时；或者第一次想 A/B 测试两个 prompt 版本时。
- **依赖**: B-20 EvalFramework 已落（提供分数）。

### B-25. Semantic Cache Middleware（向量相似度短路）
- **What**: 在 Retry 之前加一层 `SemanticCacheMiddleware`。相似 prompt 命中阈值直接返回缓存的 Response，跳过 Provider 调用。挂 Redis+Vector / Qdrant。命中后 LlmCallFinished 标 `cache_kind: Semantic`，evaluator 仍跑（监控缓存是否劣化质量）。
- **Trigger**: 业务流量上来发现"重复思考"占大头；或者 Provider API 延迟开始疼时。当前 ARC 量级用 exact-match L1/L2 cache 够。
- **Why 推后**: 增加运行时依赖（vector store），第一个用户没要前不上。

### B-26. LLM FinOps — Token / Cost 聚合 + Quota 中间件
- **What**:
  - 内置 Price Card（per-model USD/1M token）
  - `LlmCallFinished` 加 `cost_usd: f64` 字段
  - `QuotaMiddleware`：按 tenant / user / session 设 budget，超 → 拦截 OR 自动降级到便宜模型
- **EventStore SQL 福利**: 有了之后"过去 7 天哪个用户花了多少钱"一句 SQL 就能查
- **Trigger**: 第二个 user 进来分账时；或者首次出现"被打爆账单"事件。当前 ARC 单用户单机器跑批，没账单焦虑。
- **依赖**: B-20 EventStore 已落（cost 进 LlmCallFinished payload）。

### B-27. Pre-flight Guardrails — Input 安全门
- **What**: 在请求到 Provider 之前的拦截层。Prompt injection 检测（regex / 小分类器）、PII 擦除（手机/信用卡/SSN → 占位符，response 回来再还原）。触发 → HTTP 400，不消耗 Provider token。
- **跟 ValidationMiddleware 区别**: Guardrail 是 input 层（请求前），Validation 是 output 层（响应后）。混不得：guardrail 是安全门，validation 是契约校验。
- **Trigger**: 第一个公开/多用户产品上线时；或合规要求出现时。当前 ARC 内部使用，prompt 受信，不挡。
- **Why 推后**: 加运行时依赖（分类器模型 / 正则库），且只对外部输入有意义；当前没该场景。

### B-28. DPO / SFT 数据导出（数据飞轮）
- **What**: EventStore 加 `FeedbackReceived` event variant（user 反馈或业务回放信号），`tars dump-trace` CLI 支持 DPO / SFT format 输出。
- **Why 不做 exporter**: 业务 specific——chosen/rejected 怎么定义、format 用 DPO 还是 SFT 还是 PPO，每家不同。tars 提供数据萃取通道（最薄）+ 留 event variant 位即可，**不内置任何 fine-tuning format converter**。
- **Trigger**: 第一个想 fine-tune 的 user 出现。当前 ARC 没在做。
- **现在能做的最薄一步**: 加 `FeedbackReceived` event variant + 在 EventStore schema 留位（30 行），exporter 不做。Brainstorm 期间已经讨论过这条最低投入选项。

---

**Brainstorm 共识**:

- Day-2/Day-3 这 8 条全部"未来需要但不挡当前 M9"
- M9 单独完成 B-20（Validation + Evaluation 一起）= ARC 完整 unblock + 整个 LLM 系统的 observability/quality gating 基础设施
- M9 之后第一个候选是 **B-21 OTel exporter**——半成品最熟、ARC 多阶 agent 调试当前痛点
- 长期 tars 演进的"全景图": Control Plane（Config / Router / CB） + Data Plane（Pipeline / Middleware / Provider） + Observability（tracing + EventStore） + Intelligence Plane（EvalRunner / ShadowRunner / 数据萃取）

### LangGraph / LangSmith 借鉴清单 (2026-05-05 brainstorm)

研究了 LangGraph + LangSmith 在 eval / observability / MELT 这一片的做法。**值得抄 5 条,全部已经分散卷进对应 backlog 条目**——不单独成 B-XX 项,因为它们是别人 spec 的子任务。这里登记一下来源以及对照表,免得未来漏:

| # | 借鉴点 | 来源 | 落到哪 |
|---|---|---|---|
| 1 | **Run tree (parent_span_id)** — 每个 LLM/tool call 是一个 run,带 parent 形成树 | LangSmith run tree | B-21 OTel — RequestContext 加 span_id + parent_span_id,新事件 SpanStarted/SpanFinished |
| 2 | **事件 Tags 字段** — `Vec<String>` 通用 escape hatch,cohort 分析用 | LangSmith run tags | **B-20 W1.1/W2.1**——LlmCallFinished/EvaluationScored 事件 schema 一开始就带 |
| 3 | **PairwiseEvaluator trait** — `compare(req, a, b) -> A/B/Tie` | LangSmith pairwise eval | B-22 Shadow Replay — 是 Shadow 的硬依赖 |
| 4 | **Online eval sampling** — `EvaluatorSampling::{Always, Rate, Stratified, OnDimDrop}` | LangSmith sample_rate (我们扩展了 OnDimDrop 智能采样) | **B-20 W2.x**——OnlineEvaluatorRunner config 加 sampling 字段,即使 v1 默认 Always 也要把字段位置留好 |
| 5 | **Dataset 一等 typed 对象** — `Dataset { id, name, version, trace_ids, metadata }` 持久化 | LangSmith Dataset | B-22 Shadow Replay — 跟 PairwiseEvaluator 一起出 |

**明确不抄 LangSmith 的部分**:
- Hosted UI / SaaS dashboard（tars 是 library 不是 SaaS——dump-trace + SQL 是正确路径）
- 自动 instrumentation 整个 LangChain 栈（tars 中间件链显式注册——no magic）
- LangChain ecosystem 耦合（tars provider-neutral）
- 跟模型推荐 / metering 的中央聚合服务（留给 caller 或第二层 SaaS）

**明确不抄 LangGraph 本身的部分**:
- Graph-first agent model（tars 走 pipeline + 固定 3-role agent,不引 graph engine）
- TypedDict + Annotated state schema（tars Rust 强类型 + Python wrapper 优于此）
- Per-channel reducer 任意状态合并（推过 chat-shaped Session 边界）
- Subgraph 嵌套 / send() parallel dispatch（Doc 04 早就规划过的"将来可能,不挡现在"）
- Full-state checkpointer（tars 用 event sourcing 存 EventStore,语义更强;与 LangGraph 的 snapshot-per-step 思路不同但功能等价以上）

**有一条没卷进现有 backlog,可能值得新加**:
- **`Session.interrupt()` HITL primitive**——LangGraph 的 `interrupt()` 让 graph 在 node 中段暂停等 human 注入。tars 当前没等价物;ARC critic 高严重度 finding 想 human-confirm 已经在边缘碰到。**估时 ~1 周,在 B-20 之后**。如果 ARC 那边 trigger 真的来,就开 B-29 entry;否则继续 brainstorm 形态在这里。

### Codex-RS 借鉴清单 (2026-05-05 brainstorm)

研究了 `/Users/hucao/projects/codex/codex-rs/` 在 obs/eval/MELT/validation 这一片的做法。**值得抄 4 条,3 条卷进 B-21,1 条 already done**——明确不抄的部分单独列出避免被诱惑。

| # | 借鉴点 | 来源 (codex 文件 + 行号) | 落到哪 |
|---|---|---|---|
| 1 | **W3C Traceparent 跨服务传播** | `otel/src/trace_context.rs:19-36` | **B-21 spec**——`RequestContext::from_traceparent` / `to_traceparent` |
| 2 | **Dual-stream `log_event!` / `trace_event!` 宏** | `otel/src/events/shared.rs:4-52` | **B-21 spec**——`tars_melt::{log_event!, trace_event!}` + target prefix 约定 |
| 3 | **Metrics naming taxonomy** (层级 `<subsystem>.<entity>.<measure>_<unit>`) | `otel/src/metrics/names.rs:1-48` | **B-21 spec**——`tars_metrics::names` 集中常量 |
| 4 | **3-outcome verdict enum shape** (`SafetyCheck` 形态) | `core/src/safety.rs:21-31` | ✅ already done (Doc 15 `ValidationOutcome` 已经吸收同形态) |

**明确不抄 codex 的部分**:
- **Eval 框架 / golden traces / scoring rubric** —— codex 完全没做,他们的 `compact.rs` 是 history 压缩,`auto_review_denials.rs` 是 approval 决策。抄 narrow pattern 会把 tars 锁进错误抽象——eval 设计独立做(B-20 + Doc 16)。
- **Permission profiles + TOML 沙箱** —— `config/permissions.rs:28-81`。是 codex 作为 code editor 的产品概念(`:read-only` / `:workspace`),tars 的 LLM provider chain 关注点完全不同。
- **`ArcMonitor` 外部安全微服务调用** —— `arc_monitor.rs:27-48`。tars 没场景调外部 risk service。
- **Statsig exporter** —— `metrics/mod.rs:18-37`。厂商绑定;OTLP 标准 + vendor-neutral 是正路。
- **Approval workflow** (`AskForApproval` enum) —— 是 codex 产品 UX,不是 framework 基础设施。
- **Compaction phase 状态机** (`CompactionStatus`) —— 我们 B-17 LLM-summarize compact 已经规划过独立设计。

**对照 LangSmith 借鉴(上一节)的总结**: codex 借鉴**集中在 obs/MELT 的"标准协议+实施模式"层面**(W3C / dual-stream / naming),LangSmith 借鉴**集中在 eval 的"抽象形态"层面**(run tree / pairwise / sampling / Dataset)。两者**没有重叠也没有冲突**——codex 是 tars 落地观测面的工程模板,LangSmith 是 tars 设计评估面的形态参考。

### 真实代码 Deep Review 找到的 3 个 gap (2026-05-05)

外部 reviewer 看了 `routing.rs` / `retry.rs` / `middleware.rs` / `provider.rs` 真实代码后指出 3 个 production-real 的 gap。已验证全部命中。处置:

#### B-31. Routing capability pre-flight check — ✅ shipped (`<unreleased>`)
- **Where**: `tars-pipeline/src/routing.rs:202-285`
- **What shipped**:
  - `tars_types::ChatRequest::compatibility_check(&Capabilities) -> CompatibilityCheck` — checks tools / structured_output / thinking / vision; aggregates ALL incompatibility reasons in a single pass (caller sees the full list, not just the first failure).
  - `CompatibilityCheck { Compatible, Incompatible { reasons: Vec<String> } }` — 2-state (deliberately not 3-state — see code-comment for "we don't have global view at per-candidate level" reasoning; routing layer synthesizes the global verdict).
  - `RoutingService::call` candidate loop now calls `compatibility_check` *before* `provider.stream(...)`. Incompatible candidates are skipped with a structured warn log; their reasons are collected into `skipped_with_reasons`.
  - When all candidates are skipped (no wire-level errors), routing returns `ProviderError::InvalidRequest("no candidate could honour request capabilities; skipped: <id>: [<reasons>], …")` — a `Permanent` class error, retry won't help.
- **Why this shape (vs codex's `SafetyCheck` 3-state)**: codex's Skip/Reject is a per-action verdict. Per-candidate compatibility doesn't tell us whether the *request* is malformed globally — only the routing layer (which sees all candidates) knows that. Keeping per-candidate at 2-state and letting the loop aggregate is cleaner.
- **Tests**: 7 new unit tests in `tars-types::chat::tests` (each cap field individually + multi-reason aggregation), 3 new routing tests in `tars-pipeline::routing::tests` (skip-and-try-next / all-skipped-returns-InvalidRequest / pass-through-when-compatible).
- **Behavior change for callers**: requests with tools/vision/thinking that previously wire-400'd or silently drop'd at non-supporting providers now get clean local skips. ARC dogfood will see fewer mysterious provider errors when routing has heterogeneous candidates.

#### B-32. Context length 主动预检 — ⚠️ 部分 shipped (chars/4 heuristic, full fix pending tokenizer)
- **What shipped (in B-31 v2)**: `compatibility_check` 加 `ContextWindowExceeded { estimated_prompt_tokens, max_context_tokens }` 检测,用 `chars / 4` 启发式估算 prompt 大小。覆盖 obvious-overflow 场景:200k char request 打给 32k context provider 会被 routing 跳过,不浪费 wire round-trip。
- **What NOT shipped**: 真 tokenizer-based 精准检测。当前 chars/4 heuristic 在边界场景 (estimate ≈ 80-100% max) 有 ±20% 误差,所以只能可靠抓"明显超"的请求。borderline case 仍走 wire 等 provider 报错。
- **Trigger for full fix**: 真 tokenizer 集成 (D-5 unfreezes,挂 tiktoken / model-specific tokenizer)。或 ARC 真撞到 borderline case 被频繁 false-negative 拖慢调试。
- **当前状态**: 80% 实用价值已在 (chars/4 救了 wire round-trip 浪费),20% 精度 case 暂留 wire-level fallback。**够用,trigger 没到不动**。

#### Middleware 顺序陷阱 (卷进 B-20 W1.2,不单独 entry)
- **Where**: `middleware.rs:96-98` `PipelineBuilder::layer` 文档说 "first call adds outermost",**只是文档约定,没 build-time 检查**
- **Gap**: 开发者把 Telemetry 放 Retry 内/外得到完全不同的可观测结果,编译期看不出。已知反模式:
  - Telemetry 在 Retry 内 → 记 N 次离散尝试,不知道这是同一业务请求
  - Telemetry 在 Retry 外 → 记 1 次总耗时,不知道内部 retry 几次
  - CacheLookup 在 Retry 内 → 命中缓存还触发 retry 路径,毫无意义
  - CircuitBreaker 在 Retry 外 → 熔断后还 retry,违反熔断意图
  - Validation 在 Retry 外 → ValidationFailed 触发的 retry 走不到外层
- **Fix**: `PipelineBuilder::build()` 加 `validate_order` 静态检查 — 已知反模式硬编码 lookup 表,违反就 panic with helpful message:
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
- **不强类型 typestate**: 保留任意 layer 组合的扩展性,只在已知反模式上拦截。
- **进 B-20 W1.2** Pipeline.builder() build-time validation 子任务里一起做,**不开新 entry**。

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

## Cross-project survey — opencode (2026-05-03)

Surveyed `../opencode` (TypeScript-based AI coding agent, Effect-TS runtime, ~5-10× our LOC) for borrowable patterns. Items below are **specific borrows** with known sources; explicitly NOT a port of opencode's framework choices (Effect / Layer DI / dynamic-import plugins). Each ranked by ROI per implementation hour.

> **Vocabulary**: `L-N` = Lesson learned from prior art. Same `defer > delete > implement` discipline applies — these are recommendations, not commitments. Each carries a trigger condition.

### L-1. Externalize tool descriptions to `.txt` files — ✅ shipped (`7290e27`)
- **What**: `Tool::description()` returns `include_str!("read_file.txt").trim_end()` instead of an inline string literal.
- **Source**: `opencode/packages/opencode/src/tool/{read,edit,grep,…}.txt`.
- **What this actually buys** (correcting an earlier overclaim): `include_str!` is a **compile-time** embed — editing a `.txt` file still requires `cargo build`. The wins are: (a) prompt diffs review cleaner separated from Rust changes; (b) `git log -- read_file.txt` gives a clean per-prompt history; (c) future i18n can swap `.txt` files per locale at compile time.
- **Enterprise security posture** — TARS is targeting enterprise deployments, which raises the bar:
  - Compile-time embed is the **right** posture: prompts are part of the signed binary, no runtime mutation surface, no tenant-cross contamination, audit-friendly (the binary hash pins exactly which prompts were running).
  - Runtime file loading (`std::fs::read_to_string("~/.config/tars/prompts/...")`) would be a **real escalation surface** — any process / user with write access could inject malicious instructions into every subsequent LLM call. In multi-tenant deployments one tenant could affect all others. Don't add this without IAM-gated config dir + signature verification + symlink rejection + tenant scoping.
  - **Follow-on for SOC 2 / audit** — ✅ shipped (`8b60ecc`): `LlmCallCaptured` now carries `system_prompt_hash: Option<String>` (SHA256 hex). External auditors can independently verify by hashing the source `.txt` files. `tars run-task`'s multi-step trajectories pin every LLM call; `tars run`'s single-call path leaves the field `None` (deferred to a separate small refactor — documented at the call site).

### L-2. Universal output truncation in `ToolRegistry::dispatch`
- **What**: `ToolRegistry::dispatch` wraps every tool's `ToolResult` through a per-agent `OutputTruncator` (default: write-overflow-to-file, return path + tail). Today each tool implements its own cap (`fs.read_file` 256 KiB, `fs.list_dir` 256 entries) — no shared limit, no per-agent override.
- **Source**: `opencode/packages/opencode/src/tool/truncate.ts` (referenced as `Truncate.Service` from registry); every tool init goes through `truncate.output(result.output, {}, agent)`.
- **Why**: as the builtin set grows (B-9 plans `git.fetch_pr_diff`, `web.fetch`, `shell.exec`), repeated cap logic compounds and there's no way to tune per-agent (small models need more aggressive truncation).
- **Cost**: ~half-day. Add `OutputTruncator` trait + default impl to `tars-tools`. `AgentContext` grows an `output_budget` field. Existing per-tool caps become "the upper bound the truncator never exceeds even if agent budget is bigger".
- **Trigger**: when adding the 4th builtin OR when a real consumer tunes per-agent truncation.

### L-3. Add `title` field to `ToolResult` — ✅ shipped (`7290e27`)
- **What shipped**: `ToolResult { title: String, content: String, is_error: bool }` + `titled_success` / `titled_error` constructors. `ReadFileTool` fills `"Read foo.rs (4096 bytes)"`-style titles; `ListDirTool` fills `"Listed src/ (23 entries)"`. `ToolRegistry::dispatch` emits a `tracing::info!` with the title; the title is **not** placed into `Message::Tool` (LLM-visible content stays unchanged).
- **Follow-on (deferred)**: project the title into `LlmCallCaptured.response_summary` when the assistant turn includes a tool call — today the trace event is the only consumer. Wait until trajectory-replay or TUI work has a real reason to read it back.

### L-4. Parse `Retry-After` headers in `RetryMiddleware` — ✅ shipped (`c5d8e5d`)
- **What shipped**: new `tars_provider::http_base::parse_retry_after(&HeaderMap) -> Option<Duration>` with three-tier resolution (`retry-after-ms` → `retry-after` seconds → `retry-after` HTTP-date; past dates clamp to ZERO). `HttpAdapter::classify_error` grew a `&HeaderMap` parameter; openai / anthropic / gemini all populate `RateLimited::retry_after` from headers. `RetryMiddleware` already had `respect_retry_after = true` by default — now it actually has a value to honor. `httpdate 1` added as the only new dep.
- **Tests**: 7 unit tests on the helper (priority / formats / garbage / past-date), 2 backend tests pinning the populated field. 99 provider tests total.

### L-5. Permission system (`Ruleset` of `(permission, pattern, action)` rules)
- **What**: a per-agent `Ruleset = Vec<{permission, pattern, action: allow|deny|ask}>` with wildcard match, last-match-wins. Tools call `ctx.ask({permission, patterns, metadata})` to gate side-effecting operations interactively. Replaces `ReadFileTool::with_root` (static jail) with a more general policy.
- **Source**: `opencode/packages/opencode/src/permission/{evaluate,index}.ts` — `evaluate(permission, pattern, ...rulesets)` returns matching rule.
- **Why**: M4 Doc 14 §10.1's full IAM check is M6 work (blocked on `tars-security`). A Ruleset-based permission system is the smaller version that ships before IAM — and it's the **prerequisite for `fs.write_file`** (we should not let an LLM mutate the filesystem without an approval gate). Also subsumes the `with_root` jail pattern.
- **Cost**: 1-2 days. New module `tars-tools/src/permission.rs` (~300 LOC + Wildcard match util). `AgentContext` grows `permission: Arc<Ruleset>`, `ToolContext` grows `ask: AskFn`. `tars run-task --tools` gets a default permissive ruleset for read-only builtins.
- **Trigger**: **fires immediately when `fs.write_file` enters the queue** (B-9). Should ship together — write tool + permission gate in one commit.

### L-6. Tool gating per model
- **What**: `ToolRegistry::for_model(model_id)` filters the advertised tools to ones the model is good at. opencode hands GPT models `apply_patch`, others `edit/write`.
- **Source**: `opencode/packages/opencode/src/tool/registry.ts:284` — `tools()` filter logic.
- **Why**: LLM tool-use proficiency varies sharply by model + tool combination. Surfacing all tools to all models hurts smaller / older models. Today we hand every tool to every Worker.
- **Cost**: ~half-day. Add per-tool `compatible_models: Option<Pattern>` field; registry filter at `to_tool_specs(model_id)`.
- **Trigger**: when we have **both** (a) 4+ builtins AND (b) a measured case where a model misuses a tool. Until then a global tool list is fine.

### L-7. Split `fs.write_file` into `edit` (string-replace) + `apply_patch` (unified-diff)
- **What**: instead of a single `fs.write_file` taking full content, two surgical tools: `fs.edit_file` (oldString/newString) for small mods, `fs.apply_patch` (unified diff) for larger refactors. Pair with L-6 to gate per-model.
- **Source**: opencode ships both — `tool/edit.ts` (711 LOC, exact string replace with locking + BOM + format hooks) and `tool/apply_patch.ts` (309 LOC, unified diff applier).
- **Why**: full-content writes are wasteful for small changes (cost + risk of LLM losing detail in a long re-emit) and clumsy for big changes (LLM has to reproduce 1000 lines verbatim to change 5). Surgical tools match how models actually want to modify files.
- **Cost**: 2-3 days for both. Each needs file locking, BOM/line-ending preservation, format-after-write hook.
- **Trigger**: blocked on **L-5 Permission + Backtrack/Saga (B-4)**. When `fs.write_file` finally ships, ship as these two from the start, never as a single full-content write.

### L-8. Bus / event-publishing for tool side effects
- **What**: tools that mutate state publish events (`File.Edited { path }`, `Patch.Applied { changes }`). Other subsystems subscribe (LSP refresh, snapshot/undo, TUI live update).
- **Source**: `opencode/packages/opencode/src/bus.ts` + per-tool `bus.publish(File.Event.Edited, ...)`.
- **Why**: decouples tool implementations from observers (LSP doesn't need to know about every edit-tool variant; snapshot service doesn't need to import each tool).
- **Cost**: 1 day. Generic event bus + 1-2 event types per tool.
- **Trigger**: when there's a 2nd consumer of "a tool just changed file X" — probably LSP integration (deferred) or snapshot service (Backtrack work). Today we have one consumer (the trajectory log) and pull-based access is fine.

### L-9. `MessageV2` token tracking — split `cache.read` + `cache.write`
- **What**: opencode's `MessageV2.Assistant.tokens` has `cache: { read, write }`. Our `Usage` has `cached_input_tokens` (read only) + `cache_creation_tokens` (write). Same idea, different naming.
- **Source**: `opencode/packages/opencode/src/session/message-v2.ts`.
- **Why**: not a real fix — we're already nominally aligned. Mention only because the structural shape (`cache` as a substructure) reads better than two flat fields. **No change recommended** — renaming touches every provider adapter.
- **Trigger**: never, unless a major Usage refactor for unrelated reasons happens.

### L-10. Compaction service tuning constants
- **What**: when B-4's `ContextStore + ContextCompactor` ships, opencode's tuned constants are a useful starting point: `PRUNE_MINIMUM = 20K tokens`, `PRUNE_PROTECT = 40K`, `MIN/MAX_PRESERVE_RECENT_TOKENS = 2K..8K`, `PRUNE_PROTECTED_TOOLS = ["skill"]` (skill output never pruned).
- **Source**: `opencode/packages/opencode/src/session/compaction.ts:38-43`.
- **Why**: these aren't theoretical — they're empirical numbers from a system in production with real users. Faster to start here and tune than to derive from scratch.
- **Cost**: nominal — just a reference when implementing B-4's ContextStore.
- **Trigger**: when implementing ContextStore (B-4).

### L-11. LiteLLM/Bedrock dummy tool injection
- **What**: when `tools` is empty but message history contains tool calls, inject a dummy `_noop` tool (description: "Do not call this. Exists only for API compatibility.") to satisfy LiteLLM/Bedrock validation.
- **Source**: `opencode/packages/opencode/src/session/llm.ts:212-219`.
- **Why**: LiteLLM proxies and Bedrock both reject requests with stale tool calls but no `tools` param. We'll trip this when a user routes through LiteLLM as an OpenAI-compat backend with tool history.
- **Cost**: ~30 min in `tars-pipeline` or per-backend adapter.
- **Trigger**: first user reports of LiteLLM/Bedrock 400s. Trivial fix when it appears; no need to ship preemptively.

### L-12. `invalid` tool — graceful unknown-tool handler
- **What**: special tool registered under id `"invalid"` that catches "model called a tool that doesn't exist" and returns a clean error message back to the model so it can adapt.
- **Source**: `opencode/packages/opencode/src/tool/invalid.ts`.
- **Why**: today `ToolRegistry::dispatch` returns an `is_error` Tool message when the lookup misses (we already do this — `registry-1` test case in the existing dispatch helper). The `invalid` pattern is the cleaner version: register one tool that handles ALL unknowns, with a tuned message that explains what the model did wrong.
- **Cost**: 1h.
- **Trigger**: when an LLM repeatedly hallucinates non-existent tools and burns through retries. Until then our existing miss-as-is_error covers it.

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
