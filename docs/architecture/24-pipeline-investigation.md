# Doc 24 — Pipeline Investigation & Blackboard Inspector

> Scope: the **read/debug** side of Doc 19. Doc 19 makes every DAG step write
> its own durable domain state to a shared **blackboard** with per-event
> provenance. This doc adds the surface that lets a human (or the agent
> dogfooding itself) *interrogate* that blackboard after the fact: who wrote
> this row, in which step, at which commit, through which write-path — and is
> any of it lying.
>
> Upstream (motivating case): A.R.C. (github.com/leocaolab/arc). Every concrete
> `file:line` below is from its `review → fix → verify → merge` pipeline.
>
> Downstream (dependencies): Doc 19 Blackboard Pipeline, Doc 17 Pipeline Event
> Store, Doc 04 Agent Runtime (trajectory), Doc 09 Storage Schema.

---

## 1. Why this exists — the bug that has no debugger

tars ships three observability grains today (`docs/observability.md`):

| Grain | Surface | Answers |
|---|---|---|
| Agent decision | `tars trajectory` | "What did the agent decide to do, in what order?" |
| LLM call | `tars events` | "Which call was slow / cached / hallucinated? Show me the prompt." |
| Live trace | stderr / OTLP | "What's happening right now?" |

A fourth grain has no surface at all: **the durable domain state the pipeline
mutates** — A.R.C.'s `findings` + `finding_events` blackboard, or any consumer
store written per Doc 19. The trajectory tells you the agent *decided to fix
finding F-12*; the events DB tells you the *Critic LLM call returned this JSON*;
**neither tells you what landed on the blackboard, stamped with what
provenance, written by which of the (possibly several) code paths that can
write that table.** That is exactly where production bugs in a per-step
pipeline live, because that is the only state that outlives the run.

### The grounding case (arc retro 2026-06-11)

The reference incident is documented in arc's
`docs/retro/2026-06-11-patch-on-patch-backfill.md`. Compressed:

- **Symptom:** the VS Code finding panel showed findings with an empty
  **History** and **Locations** ("No history recorded"). The user: *"为什么这么
  复杂?还要 backfill,不会是布丁打布丁的 fix 吗?"* — why a backfill, isn't this a
  patch-on-patch.
- **Mechanism:** the timeline's origin event (`found`) was not written at the
  scan. The scan worker wrote the finding *row* but deferred the *event* to a
  separate `backfill_from_findings_index` pass that re-derived `found` from a
  **legacy** `findings_index` table, gated by
  `should_upsert = (entity_at, rowid) < (row.started_at, row.rowid)`.
- **The trap:** an *unrelated, correct* fix (fold the critic reply into history
  before the per-step persist, `fbaf51b`) flipped whether the per-step write
  **succeeds**. That changed an input to the backfill's guard, which then
  skipped the sighting → no `found` event on all but the first file. The fix
  never touched the backfill. Pure hidden coupling between two write paths.
- **The cost:** **two wrong hypotheses were chased first** (the `should_upsert`
  guard around the upsert; a new `delete-on-reject`) before the real mechanism.
  *Because* the truth was spread across a primary write + a reconstruct pass,
  it was hard to reason about precisely — there was no way to *ask the
  blackboard* "for finding F, list every event, and for each event the step +
  write-path + commit that produced it."

Every minute of that debugging was spent reconstructing, by hand and by
hypothesis, a fact the system already half-knew but never surfaced: **the
provenance of a row**. This doc is that surface.

### Design stance

This is **not** a new store. Doc 17 (pipeline events) and Doc 19 (the
blackboard) already hold the data. This is a **read model + lints + replay**
over data that already exists, plus the one piece of metadata the blackboard
must start carrying for any of it to work: **a writer tag on every event**
(§4). The whole subsystem is "make the existing truth queryable, and make the
ways it can lie detectable."

---

## 2. Design goals

| Goal | Description |
|---|---|
| **Provenance is answerable** | For any blackboard key, return the full event timeline, each event annotated with the step, write-path, commit, and time that produced it — and the trajectory / LLM calls behind that step. |
| **Drift is detectable, not discoverable** | The "two writers, one fact" class (Instance 2 of the retro) is found by a lint that enumerates write-paths, not by a user noticing a wrong row weeks later. |
| **Reconstruct ≠ source is a command** | "Is this a patch-on-patch?" — the round-trip `write → read(reconstruct) → write` identity, and "rows on the board not reachable from a live write" — are assertions a developer runs, not intuitions they earn. |
| **A step's effect is replayable in isolation** | Re-run one DAG step against captured input; diff the blackboard delta it produces against what was recorded. "The fix step succeeded but emitted no `found` event" becomes a visible delta, not a six-step deduction. |
| **Provenance collapse is a lint** | The Doc 19 anti-pattern (one run-level `(commit, time)` stamped on every event) is caught by asserting per-event provenance is *distinct* where the model says it must be. |
| **Generic over the blackboard** | tars supplies the trait + CLI + lints; the consumer (arc) supplies the schema binding. Same split as Doc 19. |

**Anti-goals**

- Not a second copy of the blackboard. Investigation reads the consumer's store
  + the tars event streams; it owns no durable domain data of its own.
- Not a live tracer. That's the stderr/OTLP grain. This grain is *post-hoc*:
  the run is over, the state is durable, now explain it.
- Not a replacement for the consumer's own UI (arc's VS Code panel). It is the
  layer *under* that panel — the panel's empty History was the symptom; this is
  the tool that would have explained the emptiness.

---

## 3. The fourth grain, and how it pivots to the other three

```
trajectory (agent decision)        ← tars trajectory show <traj>
  └─ step "fix:F-12"
       ├─ llm_call_captured ──────→ LlmCallFinished   ← tars events show <id>
       └─ blackboard_write ───────→ finding_events row ← tars blackboard timeline F-12   ◀ NEW
                                       (event=fixed, commit=arc/fix-12@ece08c8,
                                        writer=fix_worker, at=…)
```

The new surface is `tars blackboard`. Its rows are *joinable* to the existing
two streams by the same keys they already carry:

- A blackboard event's **producing step** is a trajectory `step_seq` (the writer
  tag, §4, records it). → pivot to `tars trajectory show`.
- That step's **LLM calls** are the `llm_call_captured` events under it. → pivot
  to `tars events show --with-bodies`.

So "this `fixed` event landed with the wrong commit — what produced it" walks:
`tars blackboard timeline F-12` → writer=`fix_worker`, step_seq=4, traj=… →
`tars trajectory show <traj> | jq 'select(.step_seq==4)'` → the
`llm_call_captured` ids → `tars events show <id> --with-bodies` for the Critic
↔ Fixer exchange that drove the (mis)write. One spine, three grains, no manual
correlation.

---

## 4. The one new requirement on the blackboard: a writer tag

Everything in §5 rests on a single schema addition. Doc 19's event row is
self-contained in *space/time* (`EventLocation { commit_sha, file, approx_line
}`, arc `findings_entity/timeline.rs`) but **not in causal origin** — it does
not record *which code path wrote it*. Today you cannot distinguish a `found`
event the scan worker emitted from one the backfill re-derived. That ambiguity
is the retro bug, encoded.

Add to every blackboard event a **`WriteProvenance`**:

```rust
// tars-types/src/blackboard.rs (new) — a data contract, no backend dep.
pub struct WriteProvenance {
    /// Stable id of the code path that wrote this event. NOT free text —
    /// a registered writer (see WriterRegistry §6). e.g. "scan_worker",
    /// "fix_worker", "merge_sweep", "backfill_findings_index".
    pub writer: WriterId,
    /// Was this the PRIMARY write (the step that owns this fact) or a
    /// RECONSTRUCT write (a backfill/reindex/import net)? Doc 19 §1 says
    /// reconstruct paths may exist ONLY as a net for external/historical
    /// rows — this flag makes "a reconstruct path wrote a live row" a
    /// queryable fact, not an archaeology project.
    pub kind: WriteKind,            // Primary | Reconstruct
    /// The trajectory step that emitted it, for the cross-grain pivot (§3).
    pub step: Option<StepRef>,      // { trajectory_id, step_seq }
    /// Did this write pass the consumer's validation gate? A row written
    /// Reconstruct that did NOT run the gate the Primary path runs is the
    /// Instance-2 drift (retro §"Instance 2"). None = no gate on this path.
    pub gate: Option<GateOutcome>,  // Passed | Rejected{reason} | None
}

pub enum WriteKind { Primary, Reconstruct }
```

This is **additive** — `#[serde(default)]`, old rows deserialize with an
`unknown` writer. It does not change the event's space/time identity or its
idempotency key `(fingerprint, review_id, event)`. It is captured *at the write
site*, locally, like the commit and time already are (Doc 19 §3
"write-own-with-provenance"). arc's `record_event_at`
(`arc_db/.../findings_entity/timeline.rs`) gains one parameter; the call sites
(`entity_writes/serialize.rs`, `merge_sweep.rs:403`,
`backfill_from_findings_index`) each pass their own constant `WriterId` + kind.

> Why a registered `WriterId` and not a string: §6's lint must be able to
> *enumerate* the writers of a table and check the gate invariant. A free
> string can't be enumerated at build time; a registry can. This is the same
> "don't model a closed set as a magic string" rule arc's own Critic fires
> (`rust_best_practices::stringly-typed-domain-value`).

---

## 5. The investigation primitives

Five commands, each mapped to a step in the retro where the developer was
blind. All read-only; all generic over the `Blackboard` trait (§6).

### 5.1 `tars blackboard timeline <key>` — full provenance of one entity

The thing whose absence was the symptom. For a blackboard key (arc: a finding
fingerprint), print every event in order, each with its `WriteProvenance`:

```
$ tars blackboard timeline F-12
key      F-12  rust_best_practices::unitless-primitive-quantity  src/qty.rs:88
event    commit          at                   writer            kind        gate
-----------------------------------------------------------------------------------
found    9adf052         11:02:14  scan_worker       primary     passed
fixed    arc/fix-12@ece  11:04:51  fix_worker        primary     passed
merged   main@4357d8b    11:09:02  merge_sweep       primary     —
```

Run it against the buggy state and the bug is *on the screen*: the `found` row
is missing, or present with `writer=backfill_findings_index kind=reconstruct` —
which immediately says "this fact was reconstructed, not born here." No
hypothesis needed. The empty-History panel had no way to show this; the panel
renders the timeline, this explains *why the timeline is what it is*.

`--json` for scripting; `--all-keys --filter event=found,writer=backfill` to
sweep ("show me every finding whose origin event came from the reconstruct path,
not the scan" — the population of the retro bug, listed in one query).

### 5.2 `tars blackboard writers <table>` — the drift lint

The detector the retro's "How to detect this class §0" asks for, made a command.
Enumerate every registered `WriterId` that targets a source-of-truth table, and
for each report whether it runs through the same validation gate the primary
path runs:

```
$ tars blackboard writers findings
writer                       kind         runs gate?   verdict
-----------------------------------------------------------------------
scan_worker                  primary      verify_persistable   ok
fix_worker                   primary      verify_persistable   ok
backfill_findings_index      reconstruct  (none)               ⚠ DRIFT
   └─ upserts `findings` but bypasses verify_persistable — a row this
      writer creates can re-introduce exactly what a primary write rejected
      (retro Instance 2). Either gate it or make it event-only.
```

This is both a **static** check (the `WriterRegistry`, §6, declares each writer's
table + gate at build time → a unit test asserts "no `Reconstruct` writer
upserts a gated table ungated") and a **runtime** confirmation (over the actual
event store: "does any live row carry `kind=Reconstruct, gate=None` on a gated
table?"). The static form is the one that stops regressions; the runtime form
is the one that audits an existing DB. A validation rule that lives on one
writer is a suggestion, not an invariant — this makes it an invariant.

### 5.3 `tars blackboard verify` — reconstruct ≠ source

"不会是布丁打布丁的 fix 吧" turned into an assertion. Doc 19's principle is that
the working set is a *transient view reconstructed from* the blackboard
(`entity_to_threads` is the inverse of `apply_threads_to_entity`). That makes
two properties checkable:

1. **Round-trip identity.** `read(blackboard) → working set → write(blackboard)`
   must be a no-op on a settled review. A non-empty delta means the working set
   carries state the board doesn't (or vice versa) — the "two books" smell.
2. **No live data behind a reconstruct path.** Doc 19 §1: *"If deleting the
   backfill would lose live data, the live write is incomplete — that's the
   bug."* The command (dry-run) computes: with every `Reconstruct` writer
   disabled, which rows/events disappear? Any **live** (non-external,
   non-historical) row in that set is a primary-write hole.

```
$ tars blackboard verify --review <id>
round-trip:        ok (0 rows differ)
reconstruct-only:  ⚠ 7 `found` events exist ONLY via backfill_findings_index
                      → these 7 findings have no primary `found` write
                      (Doc 19: the scan step's write is incomplete)
```

That second line is the entire retro bug, reported in one run, before any user
sees an empty panel.

### 5.4 `tars blackboard replay <step> --review <id>` — per-step delta

The deepest tool, and the one that collapses the "two wrong hypotheses" cost.
Re-run **one** DAG step in isolation against the blackboard state its
predecessor left, with the same (recorded) LLM responses (Doc 17 bodies make
this deterministic — replay the captured Critic/Fixer exchange, no live model),
and **diff the blackboard delta it writes** against what the live run recorded:

```
$ tars blackboard replay scan --review <id> --against recorded
step `scan` replay delta vs recorded:
  findings:       +12  (match)
  finding_events: +1   ✗  expected +12 `found`
                       → 11 findings got a row but NO `found` event
                       → writer scan_worker emitted the row, deferred the event
```

This is the Doc 19 §8 E2E assertion ("every finding has a `found` event")
turned into an *interactive* probe you point at any historical review, not just
a fixture. Had it existed, the retro's mechanism — "the per-step write succeeds,
so the entity exists, so the backfill skips the event" — would have shown as a
one-line delta the moment the symptom appeared, instead of after two dead ends.

Replay isolation reuses the Doc 19 worktree contract: the step runs in a throw-
away worktree against a copy of the blackboard, so it never mutates the real
store.

### 5.5 `tars blackboard lint` — provenance-collapse & staleness

The Doc 19 anti-pattern (§2: one run-level `(started_at, git_head)` stamped on
every event) as a lint over the event store:

- **Collapsed provenance:** for a finding with both `found` and `fixed`, assert
  `(at, commit)` differ. Equal pairs = a batch wrote both with one run-level
  stamp (Doc 19 bug #1).
- **Base-HEAD fix pointer:** a `fixed` event whose `commit` equals the review's
  base HEAD is structurally suspect — the real fix commit didn't exist when a
  batch ran (Doc 19 bug #2). Flag it.
- **Deferred ownership:** a `found` event whose `writer ≠ scan_worker` (e.g.
  `writer=*finalize*` or `*backfill*`) on a live row (Doc 19 bug #3).

These are cheap `WHERE`-clause checks over `finding_events`; they ship as a CI
gate the consumer runs against a seeded `arc auto` run, the same fixture Doc 19
§8 specifies.

---

## 6. The `Blackboard` trait — what tars supplies vs. what arc binds

Same split as Doc 19 §4: tars owns the machinery, the consumer owns the schema.

```rust
// tars-storage/src/blackboard.rs (new)
#[async_trait]
pub trait Blackboard: Send + Sync {
    /// The append-only event log for one key, oldest first. Each event
    /// carries its WriteProvenance (§4).
    async fn timeline(&self, key: &EntityKey) -> Result<Vec<BlackboardEvent>>;

    /// Every key (optionally filtered) — for the --all-keys sweep.
    async fn keys(&self, filter: &KeyFilter) -> Result<Vec<EntityKey>>;

    /// The registry of writers that target this blackboard — populated at
    /// build time by the consumer (#[distributed_slice] or an inventory
    /// registration). Drives `writers` (5.2) and `lint` (5.5).
    fn writers(&self) -> &WriterRegistry;

    /// Reconstruct the working view from the board (arc: entity_to_threads).
    /// Drives `verify` round-trip (5.3).
    async fn reconstruct(&self, review: &ReviewId) -> Result<WorkingSet>;
}

pub struct WriterRegistry { /* WriterId → { table, kind, gate: Option<GateId> } */ }
```

The generic contract (Doc 19 §4 restated): **a blackboard is any store with (a)
idempotent keyed upserts, (b) an append-only per-entity event log with
per-event provenance — now including the writer tag, (c) concurrency-safe
writes, (d) a declared writer registry.** arc's `arc_db` already has (a)–(c);
this doc adds (d) and the writer tag to (b).

| tars supplies | arc binds |
|---|---|
| `Blackboard` trait, `tars blackboard` CLI, the five primitives, the lints | impl `Blackboard` for `arc_db`; register `scan_worker` / `fix_worker` / `merge_sweep` / `backfill_*` in the `WriterRegistry` with their gate ids |
| `WriteProvenance` / `WriterId` / `WriteKind` data contracts | thread `WriteProvenance` through `record_event_at` at each call site |
| replay harness (worktree isolation, Doc 17 body replay) | the per-step entry points already exist as Workers (Doc 19 §5) |

### Reuse map

| Symbol | `file:line` | Role |
|---|---|---|
| `apply_threads_to_entity` | arc `entity_writes/serialize.rs:129` | primary writer — gains a `WriteProvenance` arg |
| `record_event_at` + `EventLocation` | arc `findings_entity/timeline.rs` | the event append — carries the new writer tag |
| `entity_to_threads` | arc `entity_writes/serialize.rs` | `Blackboard::reconstruct` impl — the round-trip read (5.3) |
| `backfill_from_findings_index` | arc runner | registered `Reconstruct` writer; the `writers` lint's subject |
| `merge_sweep::record_finding_event` | arc `merge_sweep.rs:403` | the model writer that already stamps its own sha — `WriterId=merge_sweep` |
| `EventStore<E>` / `SqliteEventStoreCore` | `tars-storage/event_store.rs` | the event substrate the lints/timeline query |
| trajectory `AgentEvent` (`step_seq`) | `tars-runtime/src/event.rs` | the `StepRef` the writer tag points at, for the §3 pivot |
| Doc 17 `bodies.db` (CAS) | `tars-storage/body_store.rs` | deterministic replay input (5.4) |
| Worker / `run_plan` worktree | Doc 04 / Doc 19 §5 | replay isolation (5.4) |

---

## 7. What this is NOT (boundaries vs. Docs 16/17)

- **vs. Doc 17 (pipeline event store):** Doc 17 is tars' *own* per-LLM-call
  telemetry (`pipeline_events.db`). This doc's subject is the *consumer's
  domain blackboard*. They are parallel stores (Doc 19 §9). Investigation joins
  them via the writer tag's `StepRef`, but does not merge them.
- **vs. Doc 16 (evaluation):** Doc 16 scores *call quality* (did the model
  answer well). This scores *state integrity* (did the pipeline write the truth
  correctly). A run can have perfect eval scores and a corrupt blackboard — that
  is the retro, exactly: the Critic LLM was fine, the persistence drifted.
- **vs. the consumer UI (arc VS Code panel):** the panel *renders* the
  blackboard; this *explains and validates* it. The panel showing "No history
  recorded" is the bug report; `tars blackboard timeline` is the diagnosis.

---

## 8. E2E verification

Reuse the Doc 19 §8 fixture: a seeded repo with N known findings, a mock
provider, `arc auto`. On top of Doc 19's assertions, add:

1. **Timeline completeness:** `tars blackboard timeline <each F>` shows a
   `found` event with `writer=scan_worker, kind=primary` (not `backfill`).
2. **Drift lint is clean:** `tars blackboard writers findings` reports no
   `Reconstruct` writer on a gated table without a gate.
3. **Round-trip identity:** `tars blackboard verify` reports 0 round-trip rows
   and 0 reconstruct-only live rows.
4. **Replay catches a planted regression:** re-introduce the Doc 19 deferral
   (scan writes the row, not the event); assert `tars blackboard replay scan`
   reports the `+0 found events / expected +N` delta — i.e. the tool *fails the
   build* on the exact regression that shipped in the retro.
5. **Lint catches collapse:** plant a run-level batch write; assert
   `tars blackboard lint` flags equal `(at, commit)` across `found`/`fixed`.

Assertion #4 is the keystone: the test that turns "we found this bug twice by
hand" into "the bug cannot ship green again."

---

## 9. Roadmap (phased)

**Phase 1 — the writer tag + timeline (unblocks everything).**
1. `WriteProvenance` / `WriterId` / `WriteKind` in `tars-types`.
2. `Blackboard` trait + `WriterRegistry` in `tars-storage`.
3. arc: thread `WriteProvenance` through `record_event_at`; register the four
   writers. `tars blackboard timeline <key>` (+ `--json`, `--all-keys`).

**Phase 2 — the lints (stop the regression class).**
4. `tars blackboard writers <table>` (static registry check + runtime audit).
5. `tars blackboard lint` (collapse / base-HEAD / deferred-ownership).
6. Wire #4–#5 into arc CI against the Doc 19 fixture.

**Phase 3 — verify + replay (the deep tools).**
7. `Blackboard::reconstruct` binding + `tars blackboard verify`.
8. `tars blackboard replay <step>` over Doc 17 recorded bodies + worktree
   isolation.

**Phase 4 — cross-grain pivot polish.**
9. `tars blackboard timeline --pivot` to walk straight to the trajectory step
   and its LLM-call bodies (§3) in one invocation.

Phase 1 alone would have ended the retro at minute one (the missing/`backfill`-
authored `found` event is visible in `timeline`). Phase 2 prevents it from
recurring. Phase 3 makes the *next*, unknown member of the "two-stores-drift
family" (retro §4) a command rather than a debugging session.

---

## 10. Open questions

| # | Question | Default if undecided |
|---|----------|----------------------|
| Q1 | Is `WriterId` a global tars enum or a consumer-owned registered set? | **Consumer-owned, registered.** tars can't know arc's writers. `WriterId(String)` newtype validated against the `WriterRegistry` at write time; the registry is the closed set the lint enumerates. |
| Q2 | Does `WriteProvenance` go on every event, or only events on gated tables? | **Every event.** The cost is one small struct; the value (the §3 pivot, the deferred-ownership lint) needs it everywhere. Partial coverage reintroduces "can't tell who wrote this." |
| Q3 | Replay (5.4) — live model or recorded bodies only? | **Recorded only for v1.** Determinism is the whole point of "diff against recorded"; a live model makes the delta non-reproducible. A `--live` mode is a Phase 4+ affordance for "what would the new model do," not a debug primitive. |
| Q4 | Does `verify` round-trip (5.3) mutate to test, or simulate? | **Simulate.** Compute the would-be delta against a copy; never write. Same worktree-isolation as replay. |
| Q5 | Where does the writer-table-gate mapping live — code or config? | **Code (the registry), build-time.** The lint must run with no DB. Config would let the mapping drift from the actual call sites — the exact failure mode this doc fights. |
| Q6 | Is this `tars blackboard` or folded into `tars events`? | **Separate top-level `blackboard`.** `events` is tars' own call store (Doc 17); `blackboard` is the consumer's domain store. Folding them recreates the "which store am I querying" confusion `observability.md` warns against. |

Q1, Q2, Q6 want sign-off before Phase 1; the rest land with their phase.

---

## 11. Relationship to other docs

- **Doc 19 (Blackboard Pipeline)** is the *write* contract; this is its *read /
  audit / replay* dual. Doc 19 §8's batch E2E assertions become this doc's
  interactive `replay`/`verify`/`lint` commands (§5, §8).
- **Doc 17 (Pipeline Event Store)** supplies the recorded bodies that make
  replay deterministic, and is the parallel (call-grain) store the writer tag's
  `StepRef` pivots toward.
- **Doc 04 (Agent Runtime)** owns the trajectory whose `step_seq` the writer tag
  references; the cross-grain pivot (§3) is the join.
- **Doc 16 (Evaluation)** scores call quality; this scores state integrity — the
  retro proves the two are independent and both necessary.
