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
- **Trigger**: First Python or Node user.

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

### L-1. Externalize tool descriptions to `.txt` files
- **What**: every `Tool::description()` becomes `include_str!("read_file.txt")` instead of an inline string literal in Rust source.
- **Source**: `opencode/packages/opencode/src/tool/{read,edit,grep,…}.txt` — each tool ships with its description as a sibling text file, imported via the bundler.
- **Why**: descriptions are LLM-facing prompts that get iterated frequently — currently every tweak is a Rust recompile + commit. Externalizing them unblocks fast iteration, future i18n, and lets non-Rust collaborators edit prompts.
- **Cost**: ~30 min per tool × 2 tools (`fs.read_file` / `fs.list_dir`) + a 5-line refactor pattern.
- **Trigger**: now (cheapest item; no real reason to defer).

### L-2. Universal output truncation in `ToolRegistry::dispatch`
- **What**: `ToolRegistry::dispatch` wraps every tool's `ToolResult` through a per-agent `OutputTruncator` (default: write-overflow-to-file, return path + tail). Today each tool implements its own cap (`fs.read_file` 256 KiB, `fs.list_dir` 256 entries) — no shared limit, no per-agent override.
- **Source**: `opencode/packages/opencode/src/tool/truncate.ts` (referenced as `Truncate.Service` from registry); every tool init goes through `truncate.output(result.output, {}, agent)`.
- **Why**: as the builtin set grows (B-9 plans `git.fetch_pr_diff`, `web.fetch`, `shell.exec`), repeated cap logic compounds and there's no way to tune per-agent (small models need more aggressive truncation).
- **Cost**: ~half-day. Add `OutputTruncator` trait + default impl to `tars-tools`. `AgentContext` grows an `output_budget` field. Existing per-tool caps become "the upper bound the truncator never exceeds even if agent budget is bigger".
- **Trigger**: when adding the 4th builtin OR when a real consumer tunes per-agent truncation.

### L-3. Add `title` field to `ToolResult`
- **What**: `pub struct ToolResult { title: String, content: String, is_error: bool }` — short human-readable label like `"Read foo.rs"`, `"Run cargo test"`, `"Listed src/ (23 entries)"`.
- **Source**: `opencode/packages/opencode/src/tool/tool.ts:30` — `ExecuteResult.title`.
- **Why**: trajectory log's `LlmCallCaptured.response_summary` currently just truncates `content` to 200 chars, which is often noise (file bytes, dir listings). A `title` is what humans actually want to see when scanning `tars trajectory show <ID>`.
- **Cost**: ~10 min — add field, fill in the 2 builtins, optionally project into trajectory event summaries.
- **Trigger**: now (trivial).

### L-4. Parse `Retry-After` headers in `RetryMiddleware`
- **What**: `tars-pipeline/src/retry.rs` reads `retry-after-ms` (millisecond-precision, when present) → `retry-after` (seconds OR HTTP date) → falls back to current exponential backoff. Today we use exponential backoff exclusively, ignoring server hints.
- **Source**: `opencode/packages/opencode/src/session/retry.ts:21` (`delay()` fn) — three-tier resolution.
- **Why**: Anthropic and OpenAI both emit `retry-after` precisely so clients can avoid retry storms during rate limiting. Ignoring it means we either retry too fast (worsen the rate limit) or too slow (waste time).
- **Cost**: 1-2h. Add `extract_retry_after_ms(&ProviderError) -> Option<u64>` helper, wire into RetryMiddleware schedule.
- **Trigger**: now (cheap, real value, no dependencies).

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
