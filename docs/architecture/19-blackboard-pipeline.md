# Doc 19 — Blackboard Pipeline & Per-Step Persistence

> Scope: the contract that makes a multi-step DAG a *real* pipeline — steps
> communicate ONLY through a shared **blackboard**, and every step persists its
> own output to that blackboard immediately, stamped with its own provenance
> (commit + time). No step passes its working state to the next as the source
> of truth; no run-end batch writes everyone's results at once.
>
> Upstream (consumers): A.R.C. (github.com/leocaolab/arc) `review → fix → verify
> → merge` is the reference pipeline and supplies every concrete `file:line`
> below. Any tars DAG with durable per-step side effects is in scope.
>
> Downstream (dependencies): Doc 04 Agent Runtime (Plan / PlanStep / Worker /
> `run_plan`), Doc 17 Pipeline Event Store, Doc 09 Storage Schema.

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Blackboard is the only channel** | Steps read/write one shared store. A step NEVER receives the prior step's in-memory working set as truth — it reads the blackboard, which the prior step already wrote. |
| **Each step owns its persistence** | A Worker writes its own findings + events to the blackboard when its unit completes — not deferred to a run-end "finalize" that re-derives everyone's state. |
| **Provenance is per-event** | Each event records the commit it actually happened in + the wall-clock time it happened — captured AT the step, never a single run-level value reused for every event. |
| **Steps are isolated** | A step's working tree is a git worktree; its blackboard writes are concurrency-safe (WAL + busy_timeout). Two steps can run in parallel and neither corrupts the other. |
| **Idempotent + crash-safe** | Re-running a step writes the same blackboard delta; a crash after step N leaves N's results durable (they were written when N finished). |

**Anti-goals**

- No "collect all step outputs, then one batch write at end-of-run." That batch is the anti-pattern this doc exists to kill (see §2).
- No step reaching into another step's job: the fix step does not record merge events; the merge step does not record fix events.
- No single run-level `git_head` / `started_at` stamped onto a whole run's worth of events.
- The blackboard is not a cache of the working threads — it is the truth; the working threads are a transient view reconstructed from it.

---

## 2. The anti-pattern this replaces (grounded in A.R.C.)

A.R.C.'s `review → fix → verify → merge` *looks* like a pipeline but is not one.
Steps hand a `threads` dict (`BTreeMap<String, NormalizedIssue>` — the blackboard
working set) to each other in memory, and a **run-end batch** mirrors the final
state into the durable entity tables:

- `RunSession::finalize_entity_only` (`arc_shell/src/runner/session.rs:71`) calls
  `apply_threads_to_entity` ONCE at the end, passing `self.started_at` and
  `self.git_head` for **every** event it writes.
- `self.git_head` is captured ONCE at session construction
  (`session.rs:39 git_ops::git_head(repo_path)`) = the **base HEAD**.

Three user-visible bugs, one root:

1. **Collapsed event provenance.** `found` and `fixed` events get the same
   `started_at` and the same `git_head`, because one batch wrote both with one
   run-level pair. The timeline can't say *when* / *in which commit* each
   transition happened.
2. **Broken diff pointer.** The `fixed` event records the base HEAD, never the
   commit the fix actually lives in. Under `--no-commit` the fix is on an
   `arc/fix-<id>` branch (e.g. `ece08c8`); under auto-merge it lands on `main`
   later — **either way the real fix commit does not exist yet at the moment the
   batch runs**, so it is structurally impossible for the batch to record it.
3. **Deferred ownership.** `found` events are explicitly punted: the scan worker
   writes finding ROWS per-file (`tars_dag/workers/scan_worker.rs:230`) but its
   comment says "the `found` events + commit-sha location are added by the
   finalize backfill." So even discovery defers its events to the batch.

A *characterization test* (arc `entity_writes/mod.rs::fix_run_keeps_all_findings_
and_stamps_the_fixed_event_per_call`) proves the write PRIMITIVE
(`apply_threads_to_entity`) is per-call correct — distinct calls stamp distinct
`at`+`commit`, and nothing is dropped. So the defect is purely architectural: the
**caller** batches with one run-level provenance instead of letting each step
write its own. (It also refuted a feared "lost findings" symptom — `list` has no
review filter and the writer never `DELETE`s; the missing rows were a
reset-heavy test artifact.)

> The deeper smell: this is the "real-time ETL at read time / batch at write
> time" inversion. The blackboard should be written as the work happens, not
> reconstructed from a working set at the end.

---

## 3. The principle

A DAG step is a pure function of the blackboard plus its isolated side effects:

```
step(blackboard) -> { working set ← read(blackboard)         # the ONLY input channel
                      result, commit ← do_work(working set)  # in an isolated worktree
                      write(blackboard, result, {commit, now}) }  # the ONLY output channel
```

This is the **blackboard** architecture, **not a pipe**: steps are not
transformers wired output→input; they are agents that read/write one shared
model. Two invariants:

1. **Edges are ordering, not pipes — there is NO node-to-node channel.**
   `depends_on` means *happens-after*, nothing more; it carries no data. A step
   reads its working set from the blackboard (`view(scope)`) and writes back
   (`commit`). EVERYTHING a downstream step needs — including a transient fact
   like "the fix landed on branch X at commit Y" — is blackboard state (an
   entity's event + its provenance), read from the blackboard, never piped. So a
   step doesn't know or receive its predecessor's output; it reads the current
   scoped state. tars's `prior_results` is reduced to a **scheduling** signal
   ("does this scope have work?", to skip an empty step) — never authoritative
   state, and even that is derivable from a blackboard query.
2. **Write-own-with-provenance.** When a step finishes a unit, it `commit`s that
   unit's transition immediately, stamped with the commit it produced and the
   time — captured locally, at the step.

---

## 4. The blackboard

The blackboard is an abstract model (§4.1); A.R.C. *backs* it with the entity
store in `arc_db` (one storage choice — the model itself has no DB/tables/files):

- **`findings`** (`arc_db/schema.rs:190`, PK `fingerprint`) — current state of
  each finding (status, file, snippet, the v18 blackboard scalars
  confidence/blast_radius/evidence/prior_id/prior_verdict/dup_of, message,
  fix_reply).
- **`finding_events`** (`schema.rs:206`) — the append-only timeline. Each row is
  a self-contained space/time point via `EventLocation { commit_sha, file,
  approx_line }` (`findings_entity/timeline.rs`), UNIQUE on `(fingerprint,
  review_id, event)` → idempotent.
- **`finding_history`** (v19) — the full multi-turn Critic↔Fixer debate.

**Why parallel "each step writes its own" doesn't race — the MODEL, not the
backing.** The model is append-only + per-entity + idempotent on event identity:
concurrent steps append *distinct* events to *distinct* entities — there is no
shared mutable cell to read-modify-write, so there is nothing to race on; a
replayed `(entity, transition)` is absorbed. That safety is **structural to the
model**. A backing need only provide concurrent-safe appends + idempotent-keyed
inserts; *how* — a log, an actor, MVCC, SQLite WAL — is its own choice.
(Reference: A.R.C.'s SQLite backing happens to use `journal_mode=WAL` +
`busy_timeout` — one way to honor the contract, not the reason it is correct.)

Likewise **durability vs consistency** split cleanly: the model gives
*consistency* (every `commit` is atomic and independent, no run-end batch → the
blackboard is valid after every commit; a crash leaves a consistent prefix); the
backing gives *durability* (whether a committed event survives a crash — fsync /
WAL / replication). The model kills the "torn run-end batch" failure
structurally; the backing decides how durable each commit is.

The contract a **valid backing** must satisfy (the operational guarantees behind
the model's `view`/`commit`): (a) idempotent keyed upserts, (b) an append-only
per-entity event log with per-event provenance, (c) concurrency-safe writes,
(d) consistency after every commit. tars supplies the Worker/Plan machinery
(Doc 04) and the event store (Doc 17); the consumer supplies a backing.

> **Deferred**: the blackboard's **activation** design (how a step is woken when
> its scope has work) — an edge-triggered **notify** (a best-effort hint) plus a
> level-triggered **reconcile-against-state** (the durable board is the truth) —
> is **not designed in this doc**. The principle only: a dropped notify is caught
> by the next reconcile, a wake never blocks on the durable write (Doc 09 §2.2).

### 4.1 The blackboard is a MODEL — and how a step obtains it

The blackboard is an **abstract model, not a database**. No tables, no files, no
SQL. It is a keyed set of **entities**; each entity has a current value and an
**append-only timeline of self-describing events** (each event carries its own
transition, time, and the version it happened in). Two operations — that is the
*entire* model:

```rust
pub trait Blackboard: Send + Sync {     // Send+Sync: shared via Arc<S> across parallel steps
    type Key;               // entity identity, STABLE across runs            (arc: fingerprint)
    type Entity;            // current value: key + attributes + state        (arc: NormalizedIssue)
    type Event: Copy + Eq;  // transition kind — a CLOSED set     (arc: Found|Fixed|Verified|Merged|Reopened)
    type Scope;             // a read selector over entities    (arc: "open findings in run R, file F")
    type Version;           // provenance: the state-of-world a transition happened against (arc: commit sha)

    fn view(&self, scope: &Self::Scope) -> Result<Vec<Self::Entity>, BbError>;
    fn commit(&self, e: &Self::Entity, t: Transition<Self::Event, Self::Version>) -> Result<(), BbError>;
}

pub struct Transition<Ev, Ver> {
    pub kind: Ev,                 // which transition (the event)
    pub at: Time,                 // when — captured AT the step
    pub version: Ver,             // the version it happened in (its provenance)
    pub reason: Option<String>,   // optional audit note
}
```

A handle is **scoped to one run** (it carries the run id), so `commit` stamps
that run automatically; entities PERSIST across runs (found in run 1, fixed in
run 2 — same `Key`, two events).

**Five laws** — the model; a valid backing (§4) must honor them:

1. **Append-only** — `commit` never deletes or mutates a prior event; it appends.
2. **Atomic** — for one `commit`, value-set + event-append is one unit (both or
   neither).
3. **Idempotent on `(Key, run, kind)`** — re-committing the same transition in the
   same run is absorbed (no duplicate event); retries / parallel re-entry safe.
4. **Read-your-writes (per run)** — after `commit(e, ..)`, a later `view(scope ∋ e)`
   sees e's new value.
5. **Value ≡ timeline** — an entity's current value agrees with its latest
   transition (status = last event's `kind`); the timeline is the truth, the
   value is its projection — a backing MAY materialize/cache the value but must
   keep it consistent.

How it is **backed** — in-memory, an event log, a document store, a SQL DB — is a
separate, pluggable concern the model and every step are oblivious to, as long as
the five laws hold. (Reference: A.R.C. backs it with SQLite rows — one storage
choice, not the blackboard.)

**Context = how a step is HANDED the blackboard (the tars seam).** A step never
constructs or fetches it. tars threads a consumer-supplied `Arc<S>` (the
`Blackboard`) through the DAG and injects it into each step's context:

```rust
pub struct WorkerContext<S> { runtime, trajectory_id, cancel, refinements,
                              shared: Arc<S> }          // ← the blackboard, injected
trait Worker<S> { async fn run(&self, plan, step,
    prior: &HashMap<String, AgentMessage>,              // CONTROL signals only (§3.1)
    ctx: WorkerContext<S>) -> WorkerOutput; }
run_plan<S>(.., workers: WorkerRegistry<S>, shared: Arc<S>, ..)   // injects S into every ctx
```

So a step's whole surface is three channels: **state** = `ctx.shared.view(scope)`
/ `ctx.shared.commit(e, ev)`; **control** = `prior.get(dep_id)` (signals only —
which entities have work, which branch — never the authoritative value, §3.1);
**services** = `ctx.runtime` / `ctx.cancel`. It holds nothing else — so it cannot
reach storage, another step's set, or batch.

---

## 5. The steps (reference: A.R.C.)

Each step reads the blackboard, works in its own git worktree, and writes its own
rows + events with its own provenance. `EventKind` is derived from the resulting
status (`entity_writes/serialize.rs:81 transition_event`).

| Step | Reads | Does | Writes to blackboard (provenance) |
|---|---|---|---|
| **scan / review** | repo @ review commit | detect findings in an isolated read | `findings` rows + `found` events @ **review commit + scan time** |
| **fix** | open findings (`entity_to_threads`) | Critic↔Fixer loop in a worktree; commit accepted fix to `arc/fix-<id>` (`orchestration.rs:700 promote_to_branch`) | `fixed`/`verified` events @ **branch commit (`branch_commit_sha`) + fix time** |
| **verify** | fixed findings | re-detect / re-run the rubric | `verified` / `reopened` events @ **verify commit + verify time** |
| **merge** | accepted branches | cherry-pick `arc/fix-<id>` → `main` (`merge_sweep.rs`) | `merged` events @ **on-main commit + merge time** |

Key consequence: the `fixed` event and the `merged` event are **different events
in different steps with different commits** — the branch commit and the
landed-on-main commit. The diff viewer picks whichever the user is inspecting.
Today merge-sweep is the only step that records its own event correctly
(`merge_sweep.rs:403 record_finding_event`, with `branch_audit.commit_sha` from
`:391`); the design generalizes that pattern to *every* step.

---

## 6. What changes

1. **Add per-step persistence at each worker.**
   - fix: after `promote_to_branch` succeeds (`orchestration.rs:700`), open
     `.arc/arc.db` and `apply_threads_to_entity(conn, run_id, now, threads,
     Some(branch_commit_sha))` for that worker's chunk. `outcome.committed_branch`
     (`orchestration.rs:38`) already carries the branch; resolve its sha.
   - scan: stop deferring `found` events — emit them in `scan_worker.rs:230`'s
     write with the review commit, instead of relying on the finalize backfill.
   - verify: replace `verify.rs:266`'s `finalize_entity_only` with a direct
     `verified`/`reopened` event write at the verify commit.
2. **Remove the batch bypass.** `finalize_entity_only` keeps `save_audit_only`
   (the `runs` audit row for legacy `verify`/`resolve`) but DROPS the
   `apply_threads_to_entity` call — every finding is now written by the step that
   produced it. The in-place fix fallback (`fix_loop_core_in_place`, the rare
   no-worktree branch) gets its own direct write so removing the batch leaves no
   coverage gap.
3. **`PartialResult` becomes control-only.** Workers stop stuffing the
   authoritative `threads` through `SerialisedFixerOutcome`; the next step reads
   the blackboard via `entity_to_threads`. (Migration: keep the threads field
   until every reader is cut over, then delete — same staging as the
   "Threads → entity inversion" plan's Phase 2.)

`apply_threads_to_entity` and `record_event_at` are unchanged — they already take
the provenance per call; the whole fix is **moving the call site from the run
end into each step** and passing the right commit.

---

## 7. Reuse map

| Symbol | `file:line` | Role |
|---|---|---|
| `apply_threads_to_entity` | `arc_shell/.../entity_writes/serialize.rs:129` | the per-call writer — reused verbatim, called per-step |
| `transition_event` | `entity_writes/serialize.rs:81` | status → `EventKind` |
| `record_event_at` + `EventLocation` | `arc_db/.../findings_entity/timeline.rs` | append one event with `{commit_sha,file,approx_line}` |
| `resolve_identity` | `entity_writes/identity.rs:23` | drift-stable fingerprint for the event key |
| `entity_to_threads` | `entity_writes/serialize.rs` | reconstruct working threads FROM the blackboard (the inter-step read) |
| `branch_commit_sha` / `promote_to_branch` | `git_ops.rs:844` / worktree | the fix step's own commit (its provenance) |
| `merge_sweep::record_finding_event` / `branch_audit` | `merge_sweep.rs:403/391` | the model to generalize — a step recording its own event with its own sha |
| `arc_db::connection::open` (WAL + busy_timeout) | `arc_db/connection.rs:31,87,90` | concurrency-safe blackboard handle per worker |

---

## 8. E2E verification

`arc auto` on a seeded repo (the user-requested test): one command runs
review → fix → merge.

Setup: a fixture repo with N known findings, deterministic (mock) provider.
Action: `arc auto`.
Assertions (read the blackboard, not stdout):

1. **No collapse**: the `found` event's `(at, commit_sha)` ≠ the `fixed` event's
   `(at, commit_sha)` for the same finding — distinct steps, distinct provenance.
2. **Real fix commit**: the `fixed` event's `commit_sha` is the `arc/fix-<id>`
   branch commit (resolvable, non-base); the `merged` event's `commit_sha` is the
   on-`main` landed commit.
3. **Coverage**: every finding has a `found` event and (if fixed) a `fixed`
   event, with the batch finalize removed — proving each step persisted its own.
4. **Crash-safety**: kill after the fix step; the `fixed` events are already
   durable (no run-end batch to lose).

Plus the existing characterization test stays green as the per-call-writer net.

---

## 9. Relationship to other docs

- **Doc 04 (Agent Runtime)** already says "DAG is the plan, not the runtime" and
  event-sources the trajectory. This doc adds the *durable-state* contract:
  beyond the trajectory event stream, the consumer's domain blackboard is
  written per-step with domain provenance.
- **Doc 17 (Pipeline Event Store)** is tars' own per-call telemetry; the
  blackboard is the *consumer's* domain truth. They are parallel, not the same
  store.
- The A.R.C.-side execution is the "Threads → entity inversion" plan: Phase 0/1
  (entity carries every scalar + the reverse `entity_to_threads` bridge) are the
  prerequisites — done; Phase 2 (flip each loop to read/write the blackboard,
  drop the batch) is exactly §6 of this doc.

---

## 10. The write primitive — `commit` is an observable state transition

§3.2's "each step emits its own event with its own provenance" is, today, a
**convention** — and conventions rot. The recurring bug exists because the write
was hand-coded at ≥4 different *implementations* that share one key and overwrite
each other. An early, WRONG instinct was to "fix" this by taking the write away
from the step — body returns a result, a framework wrapper performs the sole
commit, the body "can't get it wrong". That is backwards: it buries the one
observable transition in a black box, which is the very loss the model exists to
prevent. The cure is not to hide the write — it is to define the primitive and
give every step exactly one of it.

**`commit` is an observable state transition — not "insert a row".** When a step
calls

```rust
bb.commit(entity, Transition { kind, version, at, reason })
```

it publishes, in the open: *"I moved THIS entity one notch — to `kind`, at
`version`, now."* That append to the entity's timeline IS the observable record;
the entity's current value is the timeline's projection (value≡timeline). A
commit is how a step changes the world **and is seen doing it**.

Two things fix the bug WITHOUT hiding the write:

- **One commit FUNCTION, not one call site.** §4 collapses the ≥4 hand-coded
  write implementations into the single, sealed `Blackboard::commit` (atomic +
  idempotent + value≡timeline). Once the write *mechanism* is one funnel, many
  steps calling that same `commit` is safe — they cannot defeat each other. The
  danger was ever multiple write *implementations*, never multiple *callers*.
- **The step commits EXPLICITLY.** A step calls `commit` itself, in plain sight —
  so conditional emits, a different event per entity, or several events are all
  expressible, and the author can SEE what their step writes. A transition you
  cannot see is the bug.

**Observability is the commit timeline** — there is no separate mechanism to add.
Every state change is a committed event, so found→fixed→verified→merged is fully
visible by construction; a step's observable output is *precisely* the
transitions it commits. (A backing MAY mirror each commit into the execution
trajectory — Doc 17 — so domain transitions also surface in the run log; but the
source of truth is the commit.)

### The step abstraction — the `Worker` itself (no `Node` layer)

A step needs a SHAPE, or every worker reads/writes the blackboard ad-hoc. But a
`Worker` (Doc 04) ALREADY IS the step. So rather than wrap it in a new `Node`
trait + a `NodeRunner` adapter, **upgrade `Worker` itself** — give it two
DECLARATIONS, defaulted so every existing worker is untouched:

```rust
trait Worker {
    fn reads(&self) -> Scope { Scope::All }      // DECLARED: which entities it reads
    fn emits(&self) -> Vec<String> { vec![] }    // DECLARED: which transition kinds it MAY commit
    async fn run(&self, plan, step, prior, ctx: WorkerContext) -> WorkerOutput;
    // run commits EXPLICITLY via ctx.shared.commit(..)
}
```

- `reads` / `emits` are **declarations**, not actions — readable BEFORE the run,
  so a pipeline can reason about dataflow (scan emits `found`; fix emits
  `fixed|wontfix`) and the framework can apply an optional guard (actual commits
  ⊆ declared `emits`). They do NOT perform the write. (`emits` is wire strings,
  not a typed `Event`: `Worker` is a trait object with no consumer `Event` type.)
- `run` does the domain work and calls `ctx.shared.commit(..)` itself —
  explicit, visible, controllable (conditional emits, a different event per
  entity, several events — all expressible).
- The blackboard reaches the worker through **`WorkerContext.shared`** — the
  run-scoped blackboard, type-erased (the worker downcasts it to its handle),
  injected once via `RunPlanConfig.shared`. The executor records the step
  lifecycle (`StepStarted`/`Completed`) and may check the `emits` guard; it
  never writes on the worker's behalf.

There is **no `Node` / `NodeRunner` layer**: a `Worker` is the step, a pipeline
is already a `Plan` of workers. The only things that were missing — the
blackboard in the context, and the two declarations on the worker — are now on
`Worker`/`WorkerContext` directly. (A `Node` trait would have been a strict
subset of `Worker` minus the ability to express per-entity events, plus an
adapter — pure cost.)

**Reference: A.R.C.** arc's tangled write set — `scan_worker`,
`persist_fix_step`, `finalize_entity_only`, `backfill`, the `prior_exists`
guard — collapses into four blackboard-based workers (scan/fix/verify/merge),
each calling the one `commit` at source. `review` and `auto` share the SAME
`ScanWorker`, so they cannot diverge. Adding a step is "declare `reads` +
`emits`, write the body, commit your transitions" — no hidden machinery, nothing
to forget, nothing to hide.

---

## 11. One invocation interface: tool · worker · pipeline

A worker (one step) and a pipeline (a DAG of workers) are, to a caller, the same kind
of thing a tool is — a **callable**: a name, a typed input
schema, and `invoke(args) -> result`. The goal is a SINGLE interface so an LLM
has ONE way to call anything — a native tool, a single worker, or
a whole multi-step pipeline — without knowing which it is.

Model it on the **skill contract** (cf. Claude skills, Doc 05): a capability is
`{ name, description, input_schema, invoke }` — the model selects by
`description` and calls by `name` with schema-checked args; multi-step vs
one-shot is an implementation detail behind `invoke`. Under that one contract:

| Callable | Steps | Backed by |
|---|---|---|
| native tool | leaf | in-proc fn (Doc 05 / Doc 23 unified tool layer) |
| **worker** | 1 | one blackboard-based step that commits its own transitions (§10) |
| **pipeline** | DAG | composed workers → a `Plan` run via `run_plan` |

So a one-shot tool and a `run_plan` DAG **register the same way and present the
same face**. The driving LLM (TarsAgent, Doc 20/21) reasons over a flat list of
callables and picks the granularity it needs — a single tool or a whole pipeline
— with no per-kind wiring. The unified registry + the description/schema contract
is **Doc 05's** concern (tools · skills); **Doc 23** (unified tool layer) is
where worker/pipeline join native tools under one surface. This section asserts only
that *a blackboard pipeline IS such a callable*, not a special case — a `Plan`
behind an `invoke`, registered like any skill.

**Reference: A.R.C.** — `Pipeline::{review,fix,auto,verify}` register as composite
callables; `run("fix", {ids:[3,7]})` is one schema-checked call that drives the
shared-worker DAG underneath (one emit site per event; `review` and `auto` cannot
diverge because they share `ScanNode`).

Layering:

| Layer | Owns |
|---|---|
| **tars** | scheduling / dataflow / lifecycle (`run_plan`, `Worker`, `emit_step_lifecycle`); the unified callable registry (Doc 05 / 23) |
| **consumer's blackboard** | durable domain truth + event-at-source via explicit `commit` (§10) |
| **consumer's workers/pipelines** | domain bodies + composition; each registers as ONE callable |
