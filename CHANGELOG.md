# CHANGELOG

Roadmap-level shipped items, organized by milestone (Doc 14). Entries
record **what shipped + commit ref**, plus a short **why** when the
decision wasn't obvious from the diff. The intent: a reader landing
on this repo gets a one-page tour of "where are we" without grepping
git log or wading through TODO.md's deferred / frozen lists.

For things deliberately **not** done, see [TODO.md](./TODO.md):
- Overengineering items (O-1..O-10) — borrowed-or-built scaffolds we
  carry on a trigger-or-delete contract.
- Audit deferrals (A-1, A-4) — non-critical findings revisitable
  on touch.
- Doc-N gap items (D-1..D-13) — explicit deferred / frozen entries
  per Doc N's full surface vs. what's wired today.

For commit-level detail (per-file diffs, exact line numbers): `git log`
is authoritative. This file aggregates.

---

## M3 — Agent Runtime first cut (DONE 2026-05-03; orchestration loop pending)

Doc 14 §9 deliverable. **Substantively** done — the storage primitive,
the runtime facade, the agent contract, and the CLI integration that
proves the whole stack composes are all shipped. **Pending** are the
real orchestration agents (Orchestrator + Worker + Critic with prompt
design + the typed inter-agent message protocol) and the multi-step
loop that drives them. Those need their own PRs once the typed
`AgentMessage` envelope lands; the primitives here are the
foundation.

### tars-storage — EventStore + SqliteEventStore (`e348c09`)
- 8th workspace member. Trait + SQLite impl that backs trajectory
  replay (Doc 04 §3) and is the durability primitive
  recovery-from-checkpoint relies on.
- `EventStore` trait (5 methods: append / read_all / read_since /
  high_water / list_trajectories). Boundary type is
  `serde_json::Value` not generic `<E>` — keeps the trait
  monomorphic so `Arc<dyn EventStore>` works cleanly + makes rows
  debuggable via `sqlite3 events.db`.
- `SqliteEventStore` reuses the `tars-cache::SqliteCacheRegistry`
  scaffolding pattern (single connection in `Arc<Mutex>`,
  spawn_blocking for ops, WAL + `synchronous=NORMAL` +
  `temp_store=MEMORY`, `user_version` migration marker). Key
  departure from the cache crate's policy: an unknown prior
  schema_version **refuses to migrate** rather than wiping —
  events are durable user history, not cache.
- `(trajectory_id, sequence_no)` composite PK; `sequence_no`
  computed inside an open transaction so per-trajectory writes
  stay gap-free under concurrent calls.
- Default location: `dirs::data_dir()/tars/events.sqlite` (XDG
  data dir, NOT cache dir — events ≠ cache).
- 12 tests including `append_survives_close_and_reopen` (the
  recovery promise) and `reopen_with_unknown_schema_version_errors`.
- `ContentStore` + `KVStore` from Doc 14 §6.1 deliberately deferred
  — no consumer yet, per `defer > delete > implement`.

### tars-runtime — AgentEvent + Runtime + LocalRuntime (`7c93e6e`)
- 9th workspace member. Thin facade over `EventStore` that handles
  trajectory creation + typed-event append/read.
- `AgentEvent` enum, 8 variants for the first cut: 4 trajectory-
  lifecycle (Started / Completed / Suspended / Abandoned) + 3
  step-lifecycle (StepStarted / StepCompleted / StepFailed) + 1
  external-call capture (LlmCallCaptured).
- Doc 04 §3.2 has 10 variants including separate
  `LlmResponseCaptured` (raw bytes for parser-rewind replay) +
  `Compensation*` + `Checkpoint`. Skipped — no consumer:
  - Compensation* lands with the Saga work.
  - Llm{Response/Step}Completed split matters when "we changed
    the parser, replay against raw bytes" is real; today we
    record summaries.
  - Checkpoint becomes useful when replay is dominating recovery
    cost; today's trajectories are short.
- `StepIdempotencyKey::compute(traj, step_seq, input_summary)`
  — Doc 04 §3.2 invariant 3. Stored inline on `StepStarted`;
  external operations carry it as metadata for replay dedupe.
  64-char lowercase hex format pinned by test.
- `Runtime` trait + `LocalRuntime` impl. Mints `uuid v4 simple`
  trajectory ids. Defensive guard: `append()` rejects events
  whose embedded trajectory_id doesn't match the append target
  (catches the obvious bug at the runtime layer).
- 15 unit tests + 2 integration tests
  (`tests/recovery.rs::trajectory_survives_runtime_restart`
  proves the recovery-from-checkpoint promise end-to-end).
- Field shapes deliberately primitive (String summaries, plain
  ProviderId/Usage). Doc 04's typed `BranchReason`, `ContentRef`,
  `AgentMessage`, etc. land when their consumers exist.

### tars-cli runtime integration + `tars trajectory` (`4460c3e`)
- Every `tars run` now opens a trajectory and writes the lifecycle:
  `Started → StepStarted → LlmCallCaptured → StepCompleted →
  Completed` (or `StepFailed → Abandoned` on error). Footer
  appears after the summary: `── trajectory: <uuid>`.
- Best-effort discipline: every trajectory write swallows errors
  with `tracing::warn` rather than propagating. SQLite hiccup
  must not block the user's LLM response — same Doc 03 §4.3
  stance the cache uses.
- `--no-trajectory` opt-out + `--events-path` override
  (`TARS_EVENTS_PATH` env). Default location:
  `$XDG_DATA_HOME/tars/events.sqlite`.
- New `tars trajectory` subcommand:
  - `list` — id / event count / status (active / completed /
    abandoned).
  - `show <ID>` — every event as JSON lines on stdout (pipeable:
    `tars trajectory show ID | jq -c …`).
- Deferred subcommands (no consumer): `delete <ID>` (needs
  retention policy), `replay <ID>` (needs the Agent execution
  loop to know what "replay" means at the action level).
- `StreamOutcome` gained `response_text: String` so the trajectory
  log's `output_summary` doesn't re-read the network.
- New shared module `event_store::open()` keeps `tars run` and
  `tars trajectory` from drifting on default-path resolution.

### Agent trait + SingleShotAgent + execute_agent_step (`f6b6c4e`)
- The first M3 agent primitive. Real Orchestrator/Worker/Critic
  agents stack on this once the typed `AgentMessage` protocol +
  prompt design land.
- `Agent` trait: `id() / role() / execute(ctx, input) -> Result<AgentStepResult, AgentError>`.
- `AgentRole` enum: Orchestrator / Worker { domain } / Critic.
  Doc 04 §4.1 also lists `Aggregator` (pure-code agent, no LLM);
  skipped — no consumer.
- `AgentContext` minimal — `trajectory_id + step_seq + llm`
  (`Arc<dyn LlmService>` from tars-pipeline) `+ cancel`. Doc 04 §4.1
  lists more (budget, principal, deadline, context_store,
  tool_registry); each slots in as its backing crate ships.
- `AgentOutput` enum: `Text / ToolCalls / Mixed`. Constructed from
  a drained `ChatResponse`'s (text, tool_calls) via
  `from_response_parts`. `summary(max_chars)` for trajectory log
  payloads (200-char cap today).
- `AgentError`: `Provider(ProviderError) / Cancelled / Internal`.
  `classification()` maps to one-word strings (permanent /
  retriable / maybe_retriable / cancelled / internal) — same
  shape `AgentEvent::StepFailed::classification` expects.
- `SingleShotAgent`: drains an LLM stream → ChatResponse →
  AgentOutput. Cancel-aware: `select!`s `ctx.cancel.cancelled()`
  against both stream-open AND each event poll so a Drop'd parent
  doesn't leak the HTTP/subprocess connection. Role is
  `Worker { domain: "single_shot" }` — placeholder until real
  domain agents land.
- `execute_agent_step()` free function wraps `Agent::execute` with
  full event-log writes (`StepStarted → LlmCallCaptured +
  StepCompleted` or `StepFailed`).
  - **Bug caught + fixed by tests**: `step_seq` was being computed
    as `event_high_water + 1`, off-by-one'ing the very first
    step (TrajectoryStarted occupies event_seq=1). Fixed to count
    `StepStarted` events specifically — `step_seq` is the LOGICAL
    step identifier (Doc 04 §3.2 invariant 3), not the event
    sequencing primitive. The
    `step_seq_increments_across_multiple_agent_calls` test pins it.
  - Storage failures propagate as `RuntimeError` (not best-effort)
    — internal-tool stance, opposite of the CLI's "logging is
    optional, never fatal" stance.
- `AgentExecutionError` splits Agent failure (the model said no)
  from Runtime failure (event store is down).
- `AgentId` joins the existing string-id family in tars-types.
- 6 unit tests in agent.rs + 3 integration tests in
  `tests/agent_step.rs` driving the full stack (Pipeline +
  ProviderService + MockProvider + LocalRuntime + SqliteEventStore
  on disk).

---

## M2 — Multi-provider + Routing (DONE 2026-05-03)

Doc 14 §8 deliverable. Provider impls were already in from earlier
work; M2's defining additions are the **routing + circuit-breaker +
conformance-test triple** that turns 9 backends into a composable
fallback chain.

### Routing (`a4ebba9`)
- `RoutingPolicy` trait + `StaticPolicy` (caller-decides) + `TierPolicy`
  (config-driven `ModelTier → Vec<ProviderId>`) + `RoutingService`
  (bottom-of-pipeline LlmService).
- **FallbackChain inlined** into `RoutingService.call`'s try-each loop —
  returning an ordered `Vec<ProviderId>` from `select()` IS the
  fallback primitive. Dropped the planned `FallbackChain<P>` wrapper:
  composing chains-of-policies is uncommon enough that a list-of-IDs
  is the right shape.
- `tars-config::RoutingConfig` with `[routing.tiers]` TOML section +
  validator that catches dangling `ProviderId` references at startup.
- CLI `--tier <NAME>` flag (mutually exclusive with `--provider`).
- 10 unit tests + integration via the CLI's existing path.

### CircuitBreaker (`caf0043`)
- Per-provider state machine: `Closed → Open → HalfOpen` driven by
  consecutive open-time failures + cooldown. Concurrent HalfOpen
  callers serialise via `probe_in_flight`.
- New typed `ProviderError::CircuitOpen { until }` variant — class =
  `Retriable` so an upstream `RoutingService` falls through to the
  next candidate naturally. `retry_after()` reports remaining cooldown.
- **Design choice**: wrap the Provider, not a separate Middleware
  layer. `CircuitBreaker` impls `LlmProvider` and slots into a
  `ProviderRegistry` slot via `CircuitBreaker::wrap()`. Doc 02 §2
  diagrams it as an onion layer; wrapping the provider is functionally
  equivalent and avoids the contortions of "a middleware that knows
  which provider it's wrapping" given that our `Middleware` trait
  wraps an opaque inner `LlmService`.
- 7 unit tests cover every state transition.

### CLI `--breaker` wiring (`06502a8`)
- New `ProviderRegistry::from_map()` + `map_providers(f)` helpers so
  callers can transform an existing registry without re-running the
  config factory.
- CLI `--breaker` flag (default off) wraps every registry provider in
  `CircuitBreaker` before dispatch. Default-off is deliberate: a
  single `tars run` invocation has no cross-call breaker value;
  Retry already covers within-request retry. The flag exists to demo
  the composition + give long-lived future consumers (REPL / server)
  a reference path.

### Cross-provider conformance suite (`51ae7fc`) — D-9 closed
- `crates/tars-provider/tests/conformance.rs` — `conformance_suite!`
  macro instantiates 6 invariants × 3 HTTP backends = 18 tests:
  streaming text, tool-call args-as-Object, 401→Auth, 429→RateLimited,
  503→ModelOverloaded, capability sanity.
- Adding a 4th HTTP backend is `impl Scenarios` + one
  `conformance_suite!(name, MyScenarios);` line.
- **Caught a real Gemini bug on first run**: `finishReason="STOP"` was
  returning as `EndTurn` even on tool-call responses, while
  OpenAI/Anthropic both normalize to `ToolUse`. Fixed in same commit
  by tracking `had_function_call` per candidate and overriding.

---

## M1 — Single-provider end-to-end (DONE 2026-05-03)

Doc 14 §7 + §11 deliverable. The Doc 14 §7.2 acceptance script
(`tars run --prompt …` streams text + tokens + cost; second identical
call hits cache) works end-to-end.

### tars-pipeline skeleton (`bdaf3b5` + `15600a2`)
- `LlmService` trait — same return shape as `LlmProvider::stream` so
  providers slot in as the innermost service unchanged.
- `Middleware` trait + `Pipeline::builder()` with outermost-first layer
  composition.
- `ProviderService` (bottom adapter), `TelemetryMiddleware` (structured
  tracing on open/fail/finish/mid-stream-error), `RetryMiddleware`
  (open-time only, error-class driven, cancel-aware sleep).
- 10 unit tests + 3 wiremock integration tests.
- **Not** doing mid-stream retry — replaying a partially-consumed
  stream double-emits deltas.

### tars-cache L1 + CacheLookupMiddleware (`aaed3de`)
- `CacheKey` + `CacheKeyFactory` enforcing the Doc 03 §3 security
  shape: hasher_version first, tenant + IAM scopes prefix every hash,
  sorted scope encoding (`{a,b}` == `{b,a}`), `temperature ≠ 0` rejects
  with typed `NonDeterministic` error.
- `MemoryCacheRegistry` (moka W-TinyLFU, 10K entries default,
  5min default TTL).
- `CacheLookupMiddleware` sits between Telemetry and Retry. On hit,
  replays via `ChatResponse::into_events()` — outer middleware can't
  tell the replay from a fresh stream. Per-call `CachePolicy` override
  via `ctx.attributes["cache.policy"]`.
- New `CacheHitInfo.replayed_from_cache: bool` distinguishes a full
  L1/L2 replay from L3-style prefix-discount cache hits.
- 18 cache tests + 6 middleware tests.

### tars-cli (`3cece4f`)
- `tars run --prompt --provider --model --system --max-output-tokens
  --temperature --no-summary -v[vv]`.
- Output discipline: response → stdout (pipeable), tracing → stderr.
- Provider selection rule: explicit `--provider` wins; if exactly one
  provider configured, use it; else error listing candidates.
- Config resolution: `--config` flag → `$TARS_CONFIG` env →
  `dirs::config_dir()/tars/config.toml`.

### tars-cache L2 SQLite (`6079126`)
- `SqliteCacheRegistry` — in-process moka L1 + rusqlite L2 in one
  type. Read-through L1 → L2 → fill L1 on hit. SQLite ops run inside
  `tokio::task::spawn_blocking`.
- WAL + `synchronous=NORMAL` + `temp_store=MEMORY` pragmas. Schema
  versioned via SQLite's `user_version` PRAGMA.
- TTL via `expires_at_ms` per row; lookup filters expired; every 64th
  write fires a best-effort sweep. No background janitor (M3 work).
- CLI `--cache-path` flag (default `dirs::cache_dir()/tars/cache.sqlite`,
  `:memory:` sentinel for tests). Default `CachePolicy` flipped to
  l1+l2 ON now that L2 is real.
- **The Doc 14 §7.2 acceptance script fully passes** — second `tars
  run` invocation reports `(cache hit; cost saved)`.

### tars-melt mini (`080b05f`)
- Shared `tracing` init for all binaries. `TelemetryConfig::from_verbosity(u8)`
  + `TelemetryFormat::{Pretty, Json}` + `TARS_LOG_FORMAT` env knob.
- `TelemetryGuard` placeholder type so future M5 OTel exporter Drop
  hook slots in without breaking callers.
- Replaced the inline init in `tars-cli/src/main.rs`; `tracing-subscriber`
  dep moved out of the CLI.
- **Deferred to M5** (per B-8): metrics, OTel SDK + OTLP exporter,
  cardinality validator, generic `SecretField<T>` (today
  `SecretString` covers the only consumer), trace head + tail
  sampling.

---

## M0 — Foundation (DONE 2026-05-03)

Doc 14 §6 deliverable. Workspace + the type / config / provider /
audit-fix base everything else builds on.

### Workspace + types
- 7-crate Cargo workspace, edition 2024, rust 1.85+.
- `tars-types`: `ChatRequest` / `ChatEvent` / `ToolCall` /
  `Capabilities` / `ModelHint{Explicit,Tier,Ensemble}` /
  `ThinkingMode` / `RequestContext` / `ProviderError` /
  `Auth` / `SecretRef` / `SecretString` / strongly-typed IDs
  (`TenantId`, `SessionId`, `ProviderId`, …).

### tars-config + builtin defaults
- `Config` + `ProvidersConfig` + `ProviderConfig` (8 variants:
  `Openai` / `OpenaiCompat` / `Anthropic` / `Gemini` / `Vllm` /
  `Mlx` / `Llamacpp` / `ClaudeCli` / `GeminiCli` / `Mock`).
- `#[serde(deny_unknown_fields)]` everywhere — typos at the TOML
  layer fail loud.
- `built_in_provider_defaults()` table (codex-rs pattern) + user
  config merging via `merge_builtin_with_user()`.
- `ConfigManager::load_from_file/str()` + structured
  `ValidationError` collection (no fail-fast — operators want the
  full list to fix in one pass).

### tars-provider — 9 backends
- HTTP API: **OpenAI**, **Anthropic**, **Gemini** + 3
  OpenAI-compatible wrappers (**vLLM**, **MLX**, **llama.cpp** — all
  ride the OpenAI adapter with different `base_url` + capability
  profiles).
- CLI subprocess: **`claude_cli`** (env-var stripping for case-
  insensitive matches, kill-on-drop) + **`gemini_cli`**.
- In-process: **`MockProvider`** (canned text / event-sequence /
  error variants) for tests.
- Shared infra: `HttpProviderBase` (single reqwest client, SSE idle
  timeout via `tokio::time::timeout` per chunk), `ToolCallBuffer`
  (parallel-call accumulator with three-stage args parsing),
  `ResolvedAuth` enum with manual redacting `Debug`,
  `ProviderRegistry` with TOML-driven `from_config` factory.

### Audit fixes — A.R.C. self-review tier
Three rounds of automated audit (run IDs `3ab2b7fa`, `65be2621`),
~250 findings total, ~50 fixed across these commits:
- **`9683ce8`**: round-1 critical + 9 errors (UTF-8 boundary panic in
  HTTP error truncation, ContentType=text/event-stream content
  classification, etc.).
- **`67de40d`**: 22 more errors. Highlights: `Capabilities::validate()`,
  `Pricing::cost_for` debug_assert + saturation log, portable
  `systemtime_millis` serde, `Capabilities` modality validation,
  `ChatResponse::into_events()` for cache replay,
  `CacheHitInfo.replayed_from_cache`, OpenAI 7+22 `pending_stop_reason`
  for the usage-only-chunk bug, mock-5 single-Mutex,
  `BasicAuthResolver` warn-on-Inline.
- **`cf1605e`**: round-2 (15 issues). Highlights: `SecretString::Serialize`
  redacts (`<redacted>` instead of plaintext), `ConfigError::Parse`
  preserves `#[source]` chain, `Auth::env` distinguishes
  `VarError::NotPresent` vs `NotUnicode`, `MockProvider::stream`
  no-panic on poisoned mutex, real-bounded HTTP error body read
  (`read_bounded_body` streams chunks, caps at 8 KiB — round-1 only
  swapped the marker; the body was still fully buffered),
  `http_extras` logs invalid header names, every string-id `new()` and
  `From<&str>` panic on empty, `ToolCall::new` runtime `assert!`
  (was `debug_assert!`), `usage::cost_for` adds release-build warn
  on saturation, **deleted `transport.rs`** (TODO O-1 trigger fired:
  hit pipeline MVP without anyone needing the `HttpTransport` trait).

---

## Pre-roadmap moves

These don't fit a milestone label cleanly but were load-bearing
decisions worth recording.

### Renamed CLI provider modules (early)
- `claude_subprocess` → `claude_cli`, added `gemini_cli`. Reflects
  the subscription-auth model rather than the implementation detail
  (`subprocess`).

### Added MLX + llama.cpp HTTP-server backends (`a81378e`)
- Both expose OpenAI-compatible HTTP, so they're thin builders over
  `OpenAiProviderBuilder` with their own capability profiles + default
  ports (both 8080 — pick one per host or override `--port` on
  whichever launches second).
- Distinct `ProviderConfig` variants instead of leaning on
  `OpenaiCompat` so logs / config read deployment posture at a glance
  (`type = "mlx"` vs `type = "llamacpp"` vs `type = "vllm"`).
- vLLM stays the cloud-deployment story (PagedAttention + batching
  shines on A100/H100). Inline docs flag vLLM as overkill for local.
