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

## M3 — Agent Runtime (DONE 2026-05-03)

Doc 14 §9 deliverable. **Fully done.** Storage primitive, runtime
facade, agent contract, typed inter-agent envelope, all three default
agents (Orchestrator + WorkerAgent + Critic), the multi-step
`run_task` orchestration loop with Critic-driven Refine retries, the
`tars run-task` CLI, **and** real tool-using WorkerAgent (built on
the new `tars-tools` crate) are all live. WorkerAgent now ships in
two flavours behind the same `Agent`-trait surface: stub (no tools,
LLM describes work) and tool-using (LLM dispatches tool calls
mid-conversation). The same `AgentMessage::PartialResult` envelope
flows out either way, so the orchestration loop is unchanged.

Carried forward to follow-on work (see TODO B-4): replan-on-Reject,
ContextStore + ContextCompactor, PromptBuilder, Backtrack + Saga,
`tars trajectory replay`. None of these block the M3 acceptance
criteria; they're enhancements to a fully functional baseline.

### opencode-borrow P1 wave: L-1 + L-3 + L-4 (`7290e27` / `c5d8e5d`)

First three `defer > delete > implement` items from the opencode
survey (TODO L-1..L-12). All three were "do now" tier — small
cost, immediate value, no dependencies.

**L-1: tool descriptions externalized to `.txt` files** (`7290e27`)
- `Tool::description()` returns `include_str!("read_file.txt").trim_end()`
  via a `LazyLock<String>` instead of an inline `&'static str`.
- New sibling files: `crates/tars-tools/src/builtins/read_file.txt`
  + `list_dir.txt`. Mirrors opencode's tool/<name>.txt pattern.
- **Wins**: prompt diffs review separately from Rust changes; clean
  per-prompt git history; future i18n via per-locale `.txt` swap.
- **Security posture**: `include_str!` is a compile-time embed — the
  prompts are baked into the signed binary, no runtime mutation
  surface, no per-tenant cross-contamination via shared filesystem.
  This is the right enterprise posture; runtime file loading would
  be a real prompt-injection escalation surface and is deliberately
  NOT done. (Earlier "no recompile needed" framing was incorrect —
  editing a .txt does still require `cargo build`.)

**L-3: `ToolResult.title` for trajectory + future-TUI readability** (`7290e27`)
- `ToolResult { title: String, content: String, is_error: bool }`
  with new constructors `titled_success` / `titled_error` alongside
  the untitled ones (back-compat for external callers).
- `ReadFileTool` fills `"Read foo.rs (4096 bytes)"` /
  `"foo.rs not found"` / `"foo.rs is not UTF-8"`.
- `ListDirTool` fills `"Listed src/ (23 entries)"` /
  `"Listed src/ (256+ entries, truncated)"`.
- `ToolRegistry::dispatch` emits a `tracing::info!` with the title;
  the title is **not** placed into `Message::Tool` (LLM-visible
  content stays unchanged).

**L-4: Retry-After header parsing in `RetryMiddleware`** (`c5d8e5d`)
- New `tars_provider::http_base::parse_retry_after(&HeaderMap) ->
  Option<Duration>` with three-tier resolution:
  1. `retry-after-ms` (millisecond-precision; Anthropic uses this)
  2. `retry-after` as positive integer (seconds, RFC 7231)
  3. `retry-after` as HTTP date (past dates clamp to ZERO so the
     caller can retry immediately)
- API change: `HttpAdapter::classify_error` grew a `&HeaderMap`
  parameter between `status` and `body`. http_base.rs snapshots
  headers before consuming the response.
- All three HTTP backends (openai / anthropic / gemini) now populate
  `RateLimited::retry_after` from headers instead of `None`.
- `RetryMiddleware` already had `respect_retry_after = true` by
  default — now it actually has a value to honor.
- Dep added: `httpdate 1` (~5 KB compiled, RFC 7231 date parser).
- 7 unit tests on the helper + 2 backend tests pinning the populated
  field. tars-provider: 99 tests (was 90).

### `codex_cli` provider + `tars probe` (`a4e2254` / `72091b4` / `dd99b48` / `8712937`)

Third subscription-CLI provider lands alongside `claude_cli` /
`gemini_cli` — gives TARS users a way to leech ChatGPT Plus/Pro
inference for `gpt-5.5` / `gpt-5.4` / `gpt-5.3-codex` etc. without
burning API credits. All three subscription-leech paths verified
end-to-end against real binaries.

- **`codex_cli` backend** (`a4e2254`) — `tars-provider/src/backends/
  codex_cli.rs`. Spawns `codex exec --json --model X --sandbox
  read-only -c approval_policy="never" --skip-git-repo-check -`,
  feeds prompt on stdin, streams stdout JSONL line-by-line. Strips
  `OPENAI_API_KEY` / `CODEX_API_KEY` / `CODEX_AGENT_IDENTITY` env
  vars (case-insensitive) to force codex through `~/.codex/auth.json`
  (ChatGPT OAuth) instead of the API path. Same posture as
  `claude_cli`'s `ANTHROPIC_API_KEY` strip.

- **ThreadEvent → ChatEvent mapping (v1, conservative)**: drops
  codex's internal scratch work, surfaces only the LLM's text:

      agent_message.text  → Delta { text }
      reasoning.text      → ThinkingDelta { text }
      turn.completed      → Finished { EndTurn, <converted usage> }
      turn.failed         → ProviderError::CliSubprocessDied
      lifecycle / tool / file_change / mcp_tool_call / web_search
                           / todo_list events → drop in v1
      unknown variants    → log + skip (forward-compat)

  Folding tool/file events into Delta text is a v2 knob. Today's
  consumer (TARS `WorkerAgent` + Critic) only cares about the final
  text answer, and surfacing codex's internal sandbox-shell
  invocations would pollute summaries + confuse the Critic.

- **Real-binary fixes** (`72091b4`) — first smoke run caught two
  bugs the docs lied about:
  - `--ask-for-approval` flag doesn't exist in codex 0.128 — the
    documented flag is gone in favor of `-c approval_policy="never"`
    config override.
  - Bare model names `gpt-5` / `gpt-5-codex` are API-only; ChatGPT
    accounts must use tier-specific names like `gpt-5.5` / `gpt-5.4`
    / `gpt-5.3-codex`. Backend doesn't hardcode anything — caller
    picks per their account.

- **Live smoke tests for all three subscription CLIs** (`72091b4` /
  `dd99b48`) — `#[ignore]`-d so normal `cargo test` doesn't burn
  quota. Run with `cargo test -p tars-provider --test
  <claude|gemini|codex>_cli_smoke -- --ignored --nocapture`.
  Every backend produces a clean Started → Delta → Finished triple
  with usage (input/output/cached/thinking) all populated:

      claude_cli  sonnet           "hello from claude"
                    in=3 out=6 cached=11631 thinking=0
      gemini_cli  gemini-2.5-pro   "hello from gemini"
                    in=5804 out=4 cached=0 thinking=17
      codex_cli   gpt-5.5          "hello from codex"
                    in=13023 out=19 cached=11648 thinking=9

- **`tars probe <cli-provider>`** (`8712937`) — exposes the smoke
  pattern as a user-facing subcommand. Loads config, validates the
  named provider is `*_cli` type (HTTP types get a friendly hint to
  use `tars run` instead), sends a fixed "say hi" prompt, streams
  every ChatEvent to stderr in human-readable form. Useful for
  debugging "why doesn't `tars run-task -P codex_cli` work" —
  shows exactly which step (auth / binary / mapping) broke.

  Args: `--model <NAME>` (override default for tier-restricted
  accounts), `--prompt <STR>` (custom prompt).

### Audit fixes — round 4 (`57c893d`)

A.R.C. run `1d8e3308` against `148cda5`. Ergonomics + actionability
fixes across 7 files:

- **tars-cache/error.rs**: explicit test that `CacheError::Serialize`
  is NOT classified as `is_not_cacheable` (a refactor of the helper
  could silently misclassify it without coverage).
- **tars-config/error.rs + manager.rs**: `ValidationError` gets the
  `thiserror::Error` derive (cleaner Display via `#[error("...")]`,
  drops the hand-rolled fmt impl). New `ConfigError::validation_failed`
  ctor with `debug_assert` against an empty error list — prevents
  the "config validation failed (0)" footgun.
- **tars-config/builtin.rs**: better panic messages in tests
  (expect/got context instead of bare `panic!`); borrow on match
  arms to avoid moves before formatted-panic branches.
- **tars-provider/auth.rs**: empty/whitespace env vars + empty
  credential files now surface as `AuthError::Missing` instead of
  becoming a mysterious downstream 401.
- **tars-types/context.rs**: split `is_deadline_exceeded` out from
  `is_cancelled`, but `is_cancelled` now ORs with it so deadline
  expiration cancels in-flight requests cleanly. Callers wanting to
  distinguish caller-cancel from hard timeout get the separate
  accessor.
- **tars-pipeline/cache.rs**: cache-write failure log now includes
  the cache key's `debug_label` so warnings are actionable.

### `PromptBuilder` extraction (`8fdeed1`)

By the end of M3 four agents (Orchestrator + Critic + WorkerStub +
WorkerTools) had hand-rolled the same six lines: `ChatRequest::user`
+ system prompt + structured_output + `temperature=0.0` + optional
`tools`. Trigger-4 reached → extracted to `PromptBuilder` (fluent
recipe builder), 7 unit tests + all 60+ existing agent tests still
green.

What this is **not**: Doc 04 §6's full block-composition
PromptBuilder (persona + role + tool-doc + format-rules as separate
typed blocks). No agent today has multi-source prompts; the block
variant slots in once a second persona ships (probably alongside
multi-tenant work in M6).

### WorkerAgent + tools — stub becomes real (`148cda5`)

Wires the new `tars-tools` crate into WorkerAgent. The stub still
exists (`WorkerAgent::new`); `WorkerAgent::with_tools(..., registry)`
adds the tool-using flavour.

- **Inner dispatch loop** lives in `WorkerAgent::execute` itself,
  NOT in `run_task` — one `Agent::execute` call drives N internal
  LLM calls (drain stream → on tool calls dispatch via registry →
  append assistant + tool messages → re-prompt → repeat). Stops on
  first text-only answer or `max_tool_iterations` (default 8).
  Usage sums across calls.
- **Trajectory observability tradeoff**: the loop is invisible to
  the trajectory layer (one StepStarted/LlmCallCaptured/StepCompleted
  per Worker step regardless of tool round-trip count). Deferred
  until per-call replay has a consumer — the new event variants
  would be `LlmSubcallCaptured` + `ToolCallExecuted`, slotting in
  alongside Backtrack + Saga.
- **Two system prompts**: `WORKER_SYSTEM_PROMPT` (no tools) +
  `WORKER_SYSTEM_PROMPT_WITH_TOOLS` (instructs "call tools when you
  need them, only emit final JSON when done"). `structured_output`
  stays set in both — strict mode + tool calls coexist; tool calls
  bypass the response_format constraint, only the final text-only
  answer must conform.
- 4 integration tests in `tests/worker_with_tools.rs` using a small
  `EventQueueProvider` that pops `Vec<ChatEvent>` per call. Cover
  real `fs.read_file` dispatch + result threading, tool-spec
  advertising on first call, max-iteration safety cap, and
  stub-flavour regression.

### `tars-tools` crate — Tool trait + ToolRegistry + fs.read_file (`c4c5357`)

10th workspace member. The executable side of tool calling — typed
plumbing (`ToolSpec` / `ToolCall` / `Message::Tool`) already lived in
`tars-types`; this crate adds what actually runs.

- **`Tool` trait** — async `name() / description() / input_schema() /
  execute(args, ctx) -> Result<ToolResult, ToolError>`. Same
  `Arc<dyn Tool>` handle pattern as `Arc<dyn Agent>` in
  `tars-runtime`.
- **`ToolContext`** — `cancel` + `cwd` today; principal/tenant/
  deadline/budget slot in as their backing crates ship (matches
  `AgentContext` rationale).
- **`ToolResult { content, is_error }`** distinct from `ToolError`:
  Result is "tool ran but the operation failed, LLM should adapt";
  Error is "couldn't even attempt — Cancelled / InvalidArguments /
  Execute".
- **`ToolRegistry`** — name-keyed lookup, `register` errors on
  duplicate (silent overwrite would be a footgun), `to_tool_specs()`
  for `ChatRequest.tools`, `dispatch(call) → Message::Tool`. Both
  lookup-miss and execute-error become `is_error=true` messages
  rather than `Result::Err` so the agent loop has something to feed
  the model on the next turn.
- **Built-in `fs.read_file`** — UTF-8 read with optional path-jail
  (canonicalize-then-starts_with), 256 KiB default cap,
  NotFound/Binary/TooLarge surface as clean `is_error` results.
  Cancel-aware. The first real Tool — exercises every trait
  responsibility end-to-end so additional read-only tools become
  mechanical to write.
- **Out of scope**, each gets its own commit when consumer appears:
  idempotency tags (today's `StepIdempotencyKey` covers per-step
  dedupe), side-effect declarations (need Saga from Doc 04 §6),
  iam_scopes (need `tars-security` M6), budget_hint (need
  BudgetMiddleware), timeout (CancellationToken covers
  upstream-cancel today). Additional builtins (`fs.write_file`,
  `fs.list_dir`, `git.*`, `web.fetch`, `shell.exec`) ship one at a
  time as WorkerAgent needs them — `fs.write_file` specifically
  waits for Saga thinking before it can ship safely.
- 19 unit tests covering trait basics, registry register/get/names/
  dispatch, ReadFileTool happy + jail + size cap + binary + cancel +
  invalid args + missing file paths.

### `fs.list_dir` builtin + CLI wire-up (`b4cc406`)

Second built-in tool. Pairs with `fs.read_file`: the LLM can't read
what it hasn't located, so adding `fs.list_dir` lets prompts like
"summarise the README in this repo" work without prompts containing
literal paths.

- Same safety posture as `fs.read_file`: optional path-jail, cancel-
  aware, 256-entry default cap (truncation flagged in output so the
  LLM knows to try a more specific path).
- Output format: one entry per line, sorted, with type glyph (`d`
  dir / `f` file / `l` symlink) + name + optional size or symlink
  target. Compact for the LLM context, structured enough that the
  LLM doesn't have to guess what's a directory.
- Edge cases (not found, not-a-directory, outside jail root) surface
  as `is_error=true` ToolResult — the not-a-directory path also
  hints "use fs.read_file instead" so the model self-corrects.
- Wired into `tars run-task --tools` alongside `fs.read_file`,
  sharing the same jail root.
- 10 unit tests; tars-tools now has 29 unit tests total.

### `tars run-task --tools` flag (`87845aa`)

Capstone on M3. Wires `fs.read_file` (jailed to cwd by default) into
the CLI's WorkerAgent so `tars run-task -g "summarise the README in
this repo" --tools` actually drives a real tool-using triad — no
Rust call-site needed.

- `--tools` enables the default safe set (today: `fs.read_file` only;
  read-only ones like `fs.list_dir` / `git.fetch_pr_diff` /
  `web.fetch` will join as they ship).
- `--tools-root <PATH>` overrides the jail root (default: process cwd).
- Side-effecting tools (`fs.write_file`, `shell.exec`) won't join the
  default set — they'll get explicit opt-in flags so the safe baseline
  stays safe.
- Stderr prints the enabled tool list + jail root before any prompt
  fires.

### `tars run-task <goal>` CLI subcommand (`959be20`)

Wires `tars_runtime::run_task` into the CLI alongside `tars run` and
`tars plan`. The user-facing M3 entry point — humans can now drive
the full Orchestrator → Worker → Critic loop from a single command
instead of needing Rust call-site access.

- Shares the same `DispatchArgs` (provider/tier/model/cache/breaker/
  trajectory) as `tars run` / `tars plan` so flag semantics stay
  uniform.
- Specific args: `--goal/-g`, `--max-refinements N` (default 2 —
  matches `RunTaskConfig::default`), `--worker-domain LABEL`
  (default `general`; surfaces in `AgentRole::Worker`), `--json`
  (full `TaskOutcome` as JSON instead of human format).
- Trajectory id printed to stderr **always** — including on failure
  paths — so the user can immediately `tars trajectory show <ID>`
  to inspect what happened.
- `--no-trajectory` falls through to an in-memory `SqliteEventStore`
  rather than disabling the runtime (run_task requires a real
  `Runtime`); events still flow but leave no SQLite artefacts.

### `run_task` multi-step loop + WorkerAgent stub (`c3cea5b`)

The first user-facing M3 piece. Given a goal, drives the typed agent
triad end-to-end with full trajectory logging. The actual M3
milestone Doc 14 §9 specifies.

- **`WorkerAgent`** (`worker.rs`) — third concrete default agent.
  Stub today (no tool registry until B-9 ships); emits the typed
  `AgentMessage::PartialResult` envelope a real tool-using Worker
  will produce later, so downstream code (Critic, orchestration
  loop, replay) doesn't need to change when the stub becomes real.
  Same flat-JSON-schema-on-the-wire pattern as Critic: model emits
  `{summary, confidence}`, we map to typed `PartialResult` with
  confidence clamped to `0.0..=1.0`. `temperature=0.0` baked in for
  determinism.
- **`run_task`** (`task.rs`) — the loop itself. Orchestrator → Plan
  → for each step: Worker → Critic → (Approve advance / Refine retry
  with suggestions threaded into next Worker prompt / Reject fail).
  Bounded by `RunTaskConfig::max_refinements_per_step` (default 2).
  Every agent call routes through `execute_agent_step` so the
  trajectory log captures `StepStarted + LlmCallCaptured +
  StepCompleted` per call (or `StepFailed` on error). Trajectory
  closes with `TrajectoryCompleted` on success or
  `TrajectoryAbandoned` on any failure path so a recovery scan sees
  it as terminal. Replan-on-Reject deferred — first cut treats
  Reject as task-failed.
- **Refactor**: `OrchestratorAgent::build_planner_request` +
  `parse_plan_response` and `CriticAgent::build_critique_request`
  + new `parse_verdict_response` are now `pub` (were `pub(crate)`)
  so `run_task` can split build/execute/parse across
  `execute_agent_step`. The typed `plan()` / `critique()` helpers
  still exist for callers that want one-shot use.
- **6 integration tests** (`tests/run_task.rs`) using a local
  `QueuedProvider` that pops canned JSON in FIFO order — happy
  path single-step, refine-then-approve with suggestion threading,
  reject aborts + abandons trajectory, refine exhaustion with
  attempt count, multi-step plan ordering, malformed plan abandons.
  Per-test trajectory event-count assertions pin the
  `1 + 3*N + 1` shape so future changes that miss an event boundary
  fail loudly.

### Agent ecosystem additions (2026-05-03 follow-up wave)

- **`OrchestratorAgent` + `Plan`/`PlanStep` types** (`09546bd`) — first
  concrete LLM-driven planner. `OrchestratorAgent::plan(goal)` typed
  helper builds a strict-JSON-schema-enforced ChatRequest
  (system + temperature=0 + Plan schema), runs through the LLM,
  parses + validates the dependency graph. Linear plans for MVP
  (`depends_on` field reserved for parallel-fan-out work later).
  9 unit + 4 integration tests.
- **`tars plan <goal>` CLI subcommand + `dispatch` module refactor**
  (`89dba0f`) — wires Orchestrator into the CLI. Pretty/compact JSON
  output, full trajectory logging via `execute_agent_step`. Same
  refactor extracted ~200 lines of dispatch / cache / registry /
  pick_provider plumbing from `run.rs` into a shared
  `tars-cli/src/dispatch.rs` module so future subcommands (`tars chat`)
  flatten the same `DispatchArgs` and can't drift on flag semantics.
- **`AgentMessage` typed inter-agent envelope** (`5d0d2a5`) — Doc 04
  §4.2's "禁止纯文本互喷" piece. 4 variants chosen for concrete
  near-term consumers: `PlanIssued{plan}`,
  `PartialResult{from_agent, step_id, summary, confidence}`,
  `Verdict{from_agent, target_step_id, verdict: VerdictKind}`,
  `NeedsClarification{from_agent, question}`. `VerdictKind` =
  `Approve` / `Reject{reason}` / `Refine{suggestions}`. No
  `#[serde(other)]` catchall — unknown variants fail loudly.
  Tag names pinned per-variant by test. 9 unit tests.
- **`CriticAgent`** (`02ac233`) — second concrete default agent.
  `critique(plan, partial_result, goal)` returns a typed
  `AgentMessage::Verdict` envelope. Flat JSON schema on the wire
  (`{kind, reason, suggestions}` all required) avoids `oneOf`
  gymnastics that OpenAI strict mode handles awkwardly; mapped to
  typed `VerdictKind` in the typed helper. `PartialResultRef<'a>`
  borrowed view so the owned message stays available for the
  trajectory log. `CriticError` separates Decode failure from
  semantically-broken-but-parseable (`InvalidVerdict` — e.g. `kind=reject`
  with empty `reason`). Critic system prompt biases toward
  actionable feedback ("when uncertain between approve and refine,
  prefer refine"). 11 unit + 6 integration tests.

### Audit fixes — round 3 (`af2d8f1`)

A.R.C. run `71d49588` against `09546bd`. 76 findings: 5 errors +
9 info + 62 warnings.

- **registry-1**: `MemoryCacheRegistry::write` was lying about
  per-write `l1_ttl` — the constructor doc claimed override, but
  `let _ = ...` discarded the value silently (moka's `Cache::insert`
  doesn't take per-entry TTL without an `Expiry` policy on the
  builder). Now logs at debug when caller passes a non-default
  `l1_ttl` so the gap is visible. `SqliteCacheRegistry`'s L2 path
  always honored it.
- **trajectory-1**: `tars trajectory list` bailed out on the first
  per-row replay() failure, hiding all other (working) trajectories
  from the user. Per-row failures now render as `<error>` with cause
  logged via `tracing::warn`; the listing continues.
- **cache-1 / cache-5**: `RwLock` poison cases in `read_policy` /
  `set_cache_policy` were silent. Now log at `tracing::error`
  before degrading to default — poisoned-lock incidents leave a
  trace.
- **context-1**: re-disputed (duplicate of round-2's same finding;
  tracked as A-6 in TODO with M6 trigger).
- 9 info + 62 warnings bulk-ignored as TODO A-1's "test-quality
  pass" bucket.

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
