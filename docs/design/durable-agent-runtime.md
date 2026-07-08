# Durable Agent Task Runtime — design

> tars-runtime capability. An agent (carrying tools) accepts a task, executes it, and
> its result flows back **durably** — survivable across process restart, resumable
> without re-paying for completed work, delivered at-least-once to subscribers.
> Grounded in the current tars code (every reuse cites `file:line`). Prior art: the
> Hermes agent runtime (`NousResearch/hermes-agent`) — it validates the core choices.

---

## 1. Overview & goal

`tars_runtime::executor::run_plan` (`executor.rs:695`) runs a DAG of agent steps to
completion **in one `await`, entirely in memory** — the frontier (`pending`
`executor.rs:757`), the in-flight set (`tasks: FuturesUnordered` `:766`), and the
result map (`completed_shared: HashMap<PlanStep.id, AgentMessage>` `:752`) all live on
the stack and are **lost on crash**. Every call starts `completed_shared` empty
(`:752`) and reads nothing from storage — there is no resume.

**Goal:** a durable execution layer where **the persisted step-result store IS the
checkpoint**, so:
- an agent task is an async **job** with a durable lifecycle (submitted → running →
  done/failed), survivable across app restart;
- **resume is a memoized re-run**: on restart, re-drive the DAG; a step whose answer is
  already in the store is skipped (never re-pay the LLM), only un-done steps execute;
- results flow to consumers over a **durable outbox** (at-least-once) while live token
  streaming rides a **separate ephemeral channel**;
- the expensive, non-deterministic, side-effecting nature of agent steps is handled
  explicitly (exactly-once *execution*, at-least-once *delivery*, idempotent receivers).

> ⚠️ **Critical invariant — correctness never depends on the observability store.**
> `tars_storage::EventStore` / `PipelineEventStore` / `BodyStore` are an **off-able
> side-path sink** (diagnosis), disabled by `StoreScope::Off` (`handle.rs:339`,
> `if !cfg.store.enabled`), `ARC_TARS_EVENTS_OFF` (`arc … pipeline_builder.rs:442`),
> `CONCER_TARS_EVENTS_OFF` (`concer … transport.rs:321`), or `NullBodyStore` on release.
> The durable runtime's TRUTH — job state, the answer store, the outbox — lives in **its
> own always-on store** (Blackboard-backed, part of the runtime contract). We reuse the
> `read_since` / monotonic-seq **pattern** in that store; we do **not** layer correctness
> on the shared observability `EventStore`. The AgentEvent log + `build_run_report` stay a
> diagnosis convenience (when events are on), never the status-of-record.

### Non-goals (deliberate, with rationale)

1. **Not an always-on server daemon.** Hermes runs a systemd dispatcher polling every
   60 s; concer is a local app whose promise is **survive-and-resume-on-open**, not
   keep-running-while-closed. The multi-tenant always-on tier is N single-tenant
   processes (Doc 06 §8), future.
2. **Not a Temporal-style frontier-reconstructing workflow engine.** We deliberately
   choose **memoized re-run** (re-drive from the store, skip present-answer steps) over
   reconstructing the exact in-flight frontier. Rationale: the answer store already
   makes resume free; frontier reconstruction is an order of magnitude more complex for
   no gain here (see §8.2).
3. **Does not replace `run_plan` for cheap ephemeral runs.** A short, side-effect-free
   run that never needs to survive a crash keeps using `run_plan`. This layer is for
   work that must be durable/resumable/deliverable.

---

## 2. CUJs (Critical User Journeys)

- **CUJ-1 — comment→agent, crash before the edit lands (concer).** A human comments; the
  Author agent produces `CommentActions{fix_text, reply}` (an expensive LLM call). The
  answer is persisted; that persistence **reactively activates** "apply `fix_text` to the
  document" + "post `reply`". **Crash after the LLM answer is stored but before the edit
  is applied → on resume the stored answer re-fires the edit/reply; the LLM is NOT
  re-called.** Success: document edited + reply posted exactly once, no re-generation.
- **CUJ-2 — multi-node generation pipeline (concer Document=MAS).** author→reviewer→author
  revision DAG; nodes run per `depends_on`; a mid-pipeline crash resumes with completed
  nodes skipped.
- **CUJ-3 — agent-with-tools task (arc fixer).** An agent carries a `ToolRegistry`
  (Write/Edit/Bash), executes a fix task, its tool edits are external side effects.
  Resume must not double-apply an edit.
- **CUJ-4 — restart mid-job.** Process dies mid-job → next open reconciles: un-executed →
  re-run, executed-but-undelivered → re-send (§8.3).
- **CUJ-5 — live streaming + reconnect.** UI shows token deltas live while a step runs;
  the app is closed and reopened → UI re-reads the current snapshot, then resumes live.
- **CUJ-6 — at-least-once delivery to a consequential consumer.** A job result must reach
  an external consumer (webhook / another process) at-least-once, deduped by the receiver.

---

## 3. Feature list (derived from CUJs)

| # | Feature | CUJs |
|---|---|---|
| F1 | Async job: submit → track lifecycle → reconcile on open | 1,4 |
| F2 | Durable **answer store** (step result persisted, keyed by step identity = checkpoint) | 1,2,4 |
| F3 | **Reactive-nudge + DB-driven dispatch** (ready = all deps' answers present) | 1,2 |
| F4 | **Memoized re-run** resume (skip present-answer steps; re-run only un-done) | 2,4 |
| F5 | DAG dependency execution (reuse `PlanStep.depends_on`) | 2,3 |
| F6 | **Durable outbox** delivery: per-consumer cursor + `delivered` ack, at-least-once | 4,6 |
| F7 | **Ephemeral event bus** for live token streaming + UI patches (best-effort) | 5 |
| F8 | **Effect classification** (pure-LLM re-runnable vs external-tool idempotent) + idempotency | 1,3 |
| F9 | Run report as a **replay projection** of the durable log | 4 |

---

## 4. Requirements

### Functional

| FR | Requirement | Traces |
|---|---|---|
| FR-1 | Submit a `Task` (or `Plan` of tasks) as a durable job; return a `JobId`; job state persisted before returning | F1 |
| FR-2 | Execute each step via the existing `Worker::run(plan, step, prior_results, ctx)` seam (`executor.rs:153`) | F5 |
| FR-3 | On step completion, atomically persist `{answer, StepCompleted event}` in one transaction (reuse `Blackboard::commit`) | F2,F6 |
| FR-4 | On (re)start, resume every non-terminal job by re-driving its DAG; skip any step whose answer is already stored | F4 |
| FR-5 | Deliver terminal/lifecycle events to each registered consumer at-least-once, tracking a per-consumer cursor + `delivered` | F6 |
| FR-6 | Stream token deltas + progress live to subscribers over an ephemeral bus; drop-on-lag is acceptable | F7 |
| FR-7 | Classify each step's effect; re-run pure steps freely; require idempotency for external-tool steps | F8 |
| FR-8 | Report job/step status by replaying the event log (reuse `build_run_report` `run_report.rs:38`) | F9 |
| FR-9 | Cancel a running job via a **persisted** cancel marker (not only the in-memory token) | F1 |

### Non-functional

| NFR | Requirement |
|---|---|
| NFR-1 | **Exactly-once execution semantics** for completed steps: a step whose answer is in the store is never re-executed on resume. |
| NFR-2 | **At-least-once delivery** for outbox consumers; receivers dedupe by event id. |
| NFR-3 | **Resume-for-free / crash-safety**: no in-memory frontier is required to recover — state is re-derived from the durable store on every dispatch. |
| NFR-4 | **Dispatch latency** ≤ ~tens of ms in steady state (reactive nudge), independent of any poll interval. |
| NFR-5 | **No LLM re-call on resume** of a completed step (cost + determinism). |
| NFR-6 | The one unavoidable crash window (executed-effect, unpersisted-answer) is bounded to a **single** in-flight step; only external-tool steps in that window need idempotency. |

---

## 5. Infra

| Need | Exists? | Where |
|---|---|---|
| **Always-on durability store** (jobs/answers/outbox) — the truth | **BUILD** (on Blackboard) | `SqliteBlackboard::commit` one-tx, idempotent, five laws (`tars-storage/src/blackboard/sqlite.rs:63`, `mod.rs:27`) — consumer-supplied tables, **not** the off-able observability sink |
| Append-only log w/ monotonic seq + since-cursor read — the *pattern* | **EXISTS**, but on the **off-able** `EventStore` | `EventStore::append/read_since/high_water` (`event_store.rs:52/70/80`). Reuse the SQL pattern in our own always-on store; **do not depend on this shared sink** (it can be `StoreScope::Off`). |
| Content-hash-keyed durable result store scaffold | **EXISTS** (pattern) | `SqliteCacheRegistry` (`tars-cache/src/sqlite.rs:196`) — reuse scaffold, own value/key, **no TTL** |
| Observability event log (diagnosis, OFF-able) | **EXISTS** | `Runtime::append/replay_since` (`runtime.rs:41/50`) → `EventStore`. A side-path projection; `build_run_report` reads it. **Never a correctness dependency.** |
| Live broadcast to N subscribers | build (thin) | `tokio::sync::broadcast` |
| Per-consumer delivery cursor + `delivered` ack | **BUILD** | none exists (grep-confirmed) |

---

## 6. Components

### C1 — `JobManager` (new)
Submit/track/reconcile durable jobs. **Reuses:** `tars_model::Task` (`tars-model/src/task.rs:15`,
serializable) as the job's unit. **New:** a `jobs(job_id, status, plan, created_at,
cancel_requested)` table in the **always-on** store — this table is the **status of
record**, updated in the same blackboard transaction as each step's answer. `submit(plan)
-> JobId` persists before returning; `reconcile_on_open()` lists non-terminal jobs (from
`jobs`, not the log) and hands each to C2. **`build_run_report` (`run_report.rs:38`) is
NOT the status source** — it replays the off-able observability `EventStore`, so it is a
diagnosis/telemetry projection only (rich stats when events are on); job status must be
readable with events fully off.
```rust
pub async fn submit(&self, plan: Plan) -> Result<JobId, JobError>;
pub async fn reconcile_on_open(&self) -> Result<Vec<JobId>, JobError>;  // resume all live jobs
pub async fn cancel(&self, job: JobId) -> Result<(), JobError>;          // persisted marker
pub async fn report(&self, job: JobId) -> Result<RunReport, JobError>;   // replay projection
```

### C2 — `DurableScheduler` (new; **replaces** `run_plan`'s loop)
Re-drives a job's DAG from the **durable answer store**, not an in-memory frontier.
**Replaces:** the `'schedule` loop (`executor.rs:768-963`) + `completed_shared` (`:752`)
+ `pending`/`tasks` (`:757/:766`). **Reuses (shape):** readiness test
(`executor.rs:780`, `deps.all(present)`), `StepCondition::matches` (`orchestrator.rs:150`)
for conditional skip, `Plan::validate` (`orchestrator.rs:381`). One dispatch pass:
```
load answers = AnswerStore.all(job)          // the frontier is DERIVED, not stored
for step in plan where step.id ∉ answers:
    if step.depends_on ⊆ answers.keys():      // ready = deps' answers present
        if step.condition.matches(answers): spawn StepDriver(step, answers)
        else: record Skipped
```
Driven by a **reactive nudge** (C5 signals "answer written → dispatch") + a slow safety
poll (on-open + low-freq). The nudge carries **no state** — dispatch always re-reads the
store, so a lost nudge only delays, never corrupts (NFR-3).

### C3 — `AnswerStore` (new value/key over an existing scaffold)
Persistent `step-identity → StepResult`, the checkpoint. **Reuses:** the
`SqliteCacheRegistry` sqlite scaffold/schema pattern (`tars-cache/src/sqlite.rs:196`) —
but **its own value type** (the existing `CachedResponse` is welded to `ChatResponse`,
`registry.rs:33`) and **its own key** (existing key is welded to `ChatRequest`,
`key.rs:69`) and **no TTL** (the cache's 24 h default `sqlite.rs:61` would expire
checkpoints — forbidden). Key = stable step identity `(job_id, plan_step_id)` (the plan
id is the natural stable key; `completed_shared` already keys by `PlanStep.id`,
`executor.rs:887`). Value = the step's `AgentMessage::PartialResult` (+ usage). Atomic
write with the event via C4.

### C4 — `Outbox` (new, in the runtime's OWN always-on store)
**Reuses:** `SqliteBlackboard::commit` (`blackboard/sqlite.rs:63`) to write
`{answer (C3) + result event}` **atomically in one transaction**, idempotent on
`(key, run, kind)` (`blackboard/mod.rs:164`) — into the runtime's own tables, always-on.
Reuses the `read_since`/monotonic-`sequence_no` SQL **pattern** (`event_store.rs:70`,
`sqlite.rs:107`). **Does NOT reuse the shared `EventStore` instance** — that is the
off-able observability sink (Critical invariant, §1); layering delivery on it would lose
events whenever a consumer sets events-off. **New (the gap):** the runtime's own
`result_events(job_id, seq, payload)` append-only table + a
`delivery(consumer_id, job_id, last_delivered_seq)` cursor/`delivered` table — neither
exists today (grep-confirmed absent). Delivery worker: read own `result_events` where
`seq > cursor` → send → on ack advance cursor; on send-failure **do not advance**
(⇒ re-send ⇒ at-least-once, NFR-2). **Outbox pattern:** `executed` = row in the always-on
`result_events`; `delivered` = cursor past it.

### C5 — `EventBus` (new, thin)
Ephemeral live delivery. **New:** a `tokio::sync::broadcast::Sender<JobEvent>` on the
per-workspace `Tars` handle. Carries token deltas + "answer written" nudges + progress.
Best-effort: lagging/absent subscribers **drop** (backstopped by the snapshot re-read,
§8.5). NOT persisted; the durable path is C3/C4. Subscribers: `sender.subscribe()` →
`Receiver` loop → (in concer) Tauri `emit` → frontend.

### C6 — `StepDriver` (new wrapper; **reuses** the work seam)
Runs one step and persists its result. **Reuses as-is:** `Worker::run(plan, step,
prior_results, ctx) -> WorkerOutput` (`executor.rs:153`) — the per-step work seam,
`TarsAgent` implements it (`tars_agent.rs:230`); `WorkerContext` (`executor.rs:104`);
`ToolRegistry::dispatch` with its permission gate (`tars-tools/src/registry.rs:115`).
**New:** after `Worker::run` returns, classify the effect (§8.4) and, in one blackboard
transaction, write `{answer (C3), StepCompleted event (C4)}` + fire the C5 nudge.

---

## 7. Interfaces with other modules

| Direction | Type / fn | file:line | Use |
|---|---|---|---|
| ← tars-model | `Task`, `Agent::run(task,ctx)->AgentOutput` | `task.rs:15`, `agent.rs:129` | durable job unit + task-level execute |
| ← tars-runtime | `Plan`/`PlanStep`/`StepCondition`, `Worker::run`, `AgentEvent`(9), `Runtime::append/replay_since/next_step_seq`, `RunReport`/`build_run_report` | `orchestrator.rs:49/64/109`, `executor.rs:153`, `event.rs:100`, `runtime.rs:41/50/88`, `run_report.rs:38` | plan model, work seam, log vocabulary, status |
| ← tars-storage | `EventStore::append/read_since/high_water`, `Blackboard::commit`, `SqliteCacheRegistry` | `event_store.rs:52/70/80`, `blackboard/sqlite.rs:63`, `cache/sqlite.rs:196` | log, atomic answer+event, store scaffold |
| ← tars-tools | `ToolRegistry::dispatch`, `Tool` | `registry.rs:115`, `tool.rs:180` | agent-with-tools execution |
| → consumer (concer/Tauri) | C1 `submit/reconcile_on_open/report`, C5 `subscribe`, `get_job_state` command | this doc | UI projection + live stream |

---

## 8. Main algorithms

### 8.1 Reactive-nudge + DB-driven dispatch (F3)
Dispatch is triggered by a nudge but **always re-derives from the store**:
```
on nudge(job)  OR  on safety_poll:
   answers ← AnswerStore.all(job)                  // DB read — the truth
   for step ∉ answers with depends_on ⊆ answers:   // ready = deps' answers present
       if condition.matches(answers): StepDriver(step, answers)   // spawn
       else: persist Skipped(step)
   if every step ∈ answers (or skipped): mark job terminal
```
Reactive latency (nudge on each answer write) + polling's crash-free recovery (dispatch
holds no state). A lost nudge ⇒ the safety poll / next nudge re-derives. **Not** Hermes's
pure 60 s poll (too slow interactively); **not** pure in-memory reactive (loses frontier).

### 8.2 Memoized re-run resume (F4) — why not frontier reconstruction
On restart, C1 lists non-terminal jobs; C2 runs 8.1 for each. Because readiness +
skip-completed derive from `AnswerStore`, **resume needs no reconstruction of the
in-flight frontier** — completed steps are simply present, un-done steps re-run. This is
the whole payoff of "cache == checkpoint": resume is a property of the model, not a
feature. (Hermes likewise re-runs from the task boundary — it feeds prior attempts as
context, `build_worker_context`; we go further and *skip* completed sub-steps via the
answer store.)

### 8.3 Outbox: `executed` vs `delivered` (F6) — the two-bit rule
Two independent bits per step; recovery reconciles three states:
```
answer ∉ store                       → NOT executed        → re-run
answer ∈ store, seq > consumer.cursor → executed, undelivered → re-send (NO re-run)
answer ∈ store, seq ≤ consumer.cursor → executed, delivered   → nothing
```
Guarantees: **exactly-once execution** (store presence gates re-run) + **at-least-once
delivery** (cursor gates re-send). ⇒ receivers must be **idempotent** (dedupe by event
id / step id). Crash-window split: crash *before* the store write ⇒ not-executed ⇒
re-run (the one step needing tool idempotency, §8.4); crash *after* store write, before
ack ⇒ executed-undelivered ⇒ re-send. Validated by Hermes: `task_events` (executed log)
+ `kanban_notify_subs.last_event_id` (delivered cursor) + rewind-on-failure = this exact
pattern.

### 8.4 Effect classification (F8) — bound the hard case
Each step is tagged:
- **pure-LLM** (generation, no external effect) → re-run on the crash window is
  *tolerable* (re-cost, maybe different text); no special handling.
- **external-tool** (Write/Edit/Bash → file/command) → re-run **must** be idempotent.
  In concer's CUJ-1 this is nearly free: `fix_text` = locate-exact-`quote`→replace, so a
  second apply finds no match ⇒ no-op; `reply` upserts by `CommentTurn.id`. General tools
  get an idempotency key (content-addressed write / dedup) — NOT a two-phase protocol.
Note `execute_agent_step` today appends `StepStarted` unconditionally and does **not**
dedupe (`runtime.rs:280-293`) — so the durable answer store (not the LLM) must be the
skip gate.

### 8.5 Frontend projection (Zustand) — push-patch + snapshot-reread (F7, CUJ-5)
The UI store is a **read-model projection** of C3/C4, never the truth:
```
live:            C5 broadcast → Tauri emit → listener → patch Zustand
(re)connect/gap: Zustand invoke('get_job_state') → snapshot from store → replace
```
`push` for latency, `snapshot` for correctness; a dropped live event ⇒ one stale frame,
never a wrong state (re-read reconciles). The **callback is never recovered** — on restart
the frontend re-subscribes (new `Receiver`) and re-reads the snapshot; the runtime holds
a `Sender`, not callbacks.

---

## 9. Integration / E2E tests (each CUJ → ≥1)

- **E2E-1 (CUJ-1):** submit a comment job; kill the process after the answer is in
  `AnswerStore` but before the edit event is delivered; reopen → assert the LLM worker is
  **not** re-invoked (mock provider call-count unchanged), the edit applies once, reply
  posts once. *(exactly-once exec + at-least-once deliver + idempotent apply.)*
- **E2E-2 (CUJ-2):** 3-node author→reviewer→author plan; crash after node 1; reopen →
  node 1 skipped (answer present), nodes 2-3 run; final == no-crash run.
- **E2E-3 (CUJ-3):** agent-with-tools fix job; simulate crash in the tool-apply window →
  re-run applies the edit exactly once (idempotent tool).
- **E2E-4 (CUJ-4):** submit N jobs, kill mid-flight, `reconcile_on_open` → all reach
  terminal; un-executed re-ran, executed-undelivered re-sent (assert delivery cursor).
- **E2E-5 (CUJ-5):** stream deltas to a subscriber; drop the subscriber mid-stream;
  re-subscribe + `get_job_state` → snapshot matches store; live resumes.
- **E2E-6 (CUJ-6):** register a flaky consumer that fails delivery twice → event is
  re-sent until acked; receiver dedupes by id (assert one effect).

---

## 10. Success criteria

- NFR-1/5: a completed step is never re-executed on resume — proven by mock-provider
  call counts in E2E-1/2.
- NFR-2: every outbox event reaches each consumer ≥1× with receiver-side dedup (E2E-6).
- NFR-3: killing the process at any point and reopening always converges to the
  no-crash terminal state (E2E-4).
- NFR-4: steady-state dispatch latency dominated by the nudge, not a poll interval.

---

## 11. Performance considerations

- Hot path = one `Blackboard::commit` per step (one sqlite tx: answer upsert + event
  append). WAL + `spawn_blocking` (existing scaffold) keep it off the async reactor.
- Dispatch reads `AnswerStore.all(job)` per nudge — cache the answer-set per live job in
  memory (a projection, rebuilt from store on resume) to avoid re-reading every nudge;
  the store stays the truth.
- No poll-storm: the safety poll is low-frequency; latency comes from the nudge.

## 12. Reliability considerations

- The **only** unrecoverable window is a single in-flight step whose effect ran but whose
  answer wasn't persisted (NFR-6); bounded to one step, and only external-tool steps in
  that window need idempotency (§8.4).
- Fail-closed: unknown event-store schema **refuses to wipe** (`event_store` sqlite:99) —
  a corrupt/newer store errors rather than dropping durable state.
- Cancel is a **persisted** marker (FR-9), not only the in-memory `CancellationToken`
  (`executor.rs:748`, which is lost on restart) — a cancelled job stays cancelled.

## 13. Security considerations

- Tool execution keeps the existing permission/approval gate in `ToolRegistry::dispatch`
  (`registry.rs:115`, Allow/Deny/Ask) — the durable layer wraps, never bypasses it.
- Answer/body privacy: large results offload to `BodyStore` CAS (`body_store.rs`), whose
  retention (`purge_before`/`purge_tenant`) is independent of the telemetry log — bodies
  stay separable/purgeable (echoes the event-store split, Task 4).
- Tenant scoping via `RequestContext.tenant_id` (the existing partition key).

## 14. Abstraction & reuse (the reuse map)

**Reuse as-is (pure data / pure replay):** `Task` (`task.rs:15`), `Plan`/`PlanStep`/
`StepCondition`/`Fan`/`PlanBuilder`/`Plan::validate` (`orchestrator.rs:49/64/109/203/245/381`),
all 9 `AgentEvent` + `StepIdempotencyKey` (`event.rs:100/45`, as a serde vocabulary),
`Blackboard::commit` (`blackboard/sqlite.rs:63`, the always-on write path),
`RunStatus` (`tars-types/run_report.rs:39`), `TaskOutcome`/`StepOutcome` (`executor.rs:531/477`).

**Observability side-path — reuse the PATTERN, never depend for correctness (off-able):**
`EventStore::append/read_since/high_water` (`event_store.rs:52/70/80`), `Runtime::append/
replay_since` (`runtime.rs:41/50`), `build_run_report` (`run_report.rs:38`). All read/write
the sink that `StoreScope::Off` / `*_EVENTS_OFF` can disable. We copy the `read_since` +
monotonic-`sequence_no` SQL shape into C4's own always-on table; we do not build the outbox
or job-status on these instances. `build_run_report` = telemetry projection only.

**Reuse shape only (re-drive differently):** `Worker`/`Critic` + `WorkerContext`/
`WorkerOutput`/`WorkerRegistry` (`executor.rs:153/104/84/208`), `ToolRegistry`/`Tool`
(`registry.rs:115`, `tool.rs:180`), `TarsAgent` (`tars_agent.rs:49`), `InfraRetryPolicy`
(`executor.rs:362`), the cache sqlite scaffold (`cache/sqlite.rs:196`).

**Replace / build:** the `run_plan` `'schedule` loop + `completed_shared`/`pending`/`tasks`
(`executor.rs:752-963`) → C2 `DurableScheduler`; the in-memory answer map → C3
`AnswerStore`; **new** delivery cursor/ack table → C4; **new** broadcast bus → C5; **new**
`jobs` table + `JobManager` → C1.

**New abstractions justified:** (1) `AnswerStore` — the only genuinely new concept
(persistent, no-TTL, step-identity-keyed result store = checkpoint); everything else is a
new *driver* over existing seams. (2) the `delivery` cursor/ack table — the one persistence
gap (grep-confirmed absent). No new agent/tool/plan abstractions — those are all reused.

---

## 15. Roadmap (crux-first, each independently verifiable)

> **Implementation status — M0 + M1 LANDED** (crate `crates/tars-durable`,
> branch `durable-runtime`). Refinements made while implementing, kept
> faithful to the design:
>
> - **Placement = a new crate `tars-durable`**, layered downstream of
>   `tars-runtime` (which re-exports `Plan`/`PlanStep`/`StepCondition`/
>   `Worker`/`WorkerContext`/`Runtime`/`AgentMessage`) and `tars-storage`
>   (`SqliteBlackboard`/`BlackboardStore`). Clean acyclic layering
>   `tars-durable → tars-runtime → tars-storage`; keeps the durable driver
>   out of the pure runtime core.
> - **M0 tables** live in `store.rs`: `answers` (the AnswerStore — own
>   `StepAnswer` value, key = `job_id␟step_id`, **no TTL**), `result_events`
>   (append-only, monotonic gap-free `seq` per job, `read_since` cursor —
>   the pattern from `SqliteEventStore`, in our table), `jobs` (status of
>   record). One `SqliteBlackboard::commit` writes `{answer + result_event
>   + job.updated_at}` atomically; the blackboard's `run_id` = `job_id`, so
>   `append_event`'s `run` arg carries the job automatically and law-#3
>   idempotency is `(key, kind)` (key embeds the job). Job-state advance is
>   `jobs.updated_at` bumped inside the same `upsert` — the full job status
>   lifecycle (`submit`/`reconcile`/`cancel`) is M2.
> - **M1 driver** (`scheduler.rs`) uses a **batched** dispatch (each pass
>   re-reads `AnswerStore.all(job)`, runs the currently-ready set
>   concurrently via `FuturesUnordered`, persists each answer, re-derives)
>   rather than `run_plan`'s single continuous frontier. The load-bearing
>   property is identical and preserved: readiness + skip are **derived
>   from the store every pass**, so resume needs no in-memory frontier.
> - **Verified green**: `e2e2_crash_mid_dag_skips_completed_and_does_not_recall_llm`
>   (E2E-2), `events_off_still_persists_and_resumes` (the Critical-invariant
>   regression, driven under a no-op `NullRuntime` = events OFF),
>   `m0_answer_event_job_survive_close_and_reopen`,
>   `m0_commit_is_idempotent_on_key_kind`; plus `-p tars-runtime/-storage/-cache`
>   no-regression and `--workspace --exclude tars-desktop-app` clippy.
> - **Remaining: M2–M5** (JobManager + reconcile-on-open + persisted cancel;
>   Outbox `delivery` cursor/ack worker; EventBus + streaming + Zustand;
>   concer CUJ-1 wiring) — untouched this round.

- **M0 — always-on durability store: AnswerStore + atomic commit (the crux substrate).**
  C3 (own value/key over the cache scaffold, no TTL) + C4's `result_events` + `jobs`
  tables, all written via one `Blackboard::commit` — **in the runtime's own store,
  instantiated independently of `StoreScope`/`*_EVENTS_OFF`.** Verify: (a) write a step
  result + its event in one tx, reopen the sqlite file, both survive (mirrors
  `cache/sqlite.rs:493`); (b) **run with the observability EventStore set OFF and confirm
  the answer/job/result_events still persist** (the Critical-invariant regression test).
  **Highest uncertainty first** (durable checkpoint + the off-able-sink separation).
- **M1 — DurableScheduler + memoized re-run.** C2: derive readiness/skip from
  `AnswerStore`; re-drive `Worker::run`. Verify: **E2E-2** (crash mid-DAG, completed steps
  skipped, LLM not re-called).
- **M2 — JobManager + reconcile-on-open.** C1 `jobs` table + `submit`/`reconcile_on_open`
  + persisted cancel. Verify: **E2E-4** (kill N jobs, reopen, all converge).
- **M3 — Outbox delivery.** C4 `delivery` cursor/ack table + delivery worker (at-least-once).
  Verify: **E2E-6** (flaky consumer re-sent until acked; receiver dedupes).
- **M4 — EventBus + streaming + Zustand projection.** C5 broadcast + `get_job_state` +
  frontend patch/snapshot. Verify: **E2E-5** (drop + reconnect → snapshot correct).
- **M5 — concer CUJ-1 end-to-end.** Wire the comment→agent flow onto C1-C6; the effect
  classification (`fix_text` locate-quote, `reply` id-upsert). Verify: **E2E-1**.

Sequenced by **risk-up-front** (M0 durable checkpoint is the crux) and dependency
(delivery/streaming after the core execute/resume). concer's `async-agent-job-manager.md`
is superseded by this as the *core*; it remains the **consumer view** (Tauri wiring,
comment-thread UX).
