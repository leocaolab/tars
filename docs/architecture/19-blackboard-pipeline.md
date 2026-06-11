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

Two invariants:

1. **Inter-step channel = blackboard only.** A step's `depends_on` means "the
   blackboard rows my predecessor wrote." tars `PartialResult` carries control
   signals (status, token counts, which branch) — never the authoritative
   finding state. The next step re-reads the blackboard (arc:
   `entity_to_threads(review_id)` reconstructs the working `threads` from the
   entity tables — the inverse of `apply_threads_to_entity`).
2. **Write-own-with-provenance.** When a step finishes a unit, it writes that
   unit's blackboard delta immediately, stamped with the commit it produced and
   the current time — captured locally, at the step.

---

## 4. The blackboard

For A.R.C. the blackboard is the entity store in `arc_db`:

- **`findings`** (`arc_db/schema.rs:190`, PK `fingerprint`) — current state of
  each finding (status, file, snippet, the v18 blackboard scalars
  confidence/blast_radius/evidence/prior_id/prior_verdict/dup_of, message,
  fix_reply).
- **`finding_events`** (`schema.rs:206`) — the append-only timeline. Each row is
  a self-contained space/time point via `EventLocation { commit_sha, file,
  approx_line }` (`findings_entity/timeline.rs`), UNIQUE on `(fingerprint,
  review_id, event)` → idempotent.
- **`finding_history`** (v19) — the full multi-turn Critic↔Fixer debate.

Concurrency: the store runs `journal_mode=WAL` + `busy_timeout(30s)`
(`arc_db/connection.rs:87,90`), so parallel worktree workers each open the repo's
`.arc/arc.db` and write concurrently without corruption — this is what makes
"each step writes its own" safe under the parallel DAG.

The generic tars contract: **a blackboard is any store with (a) idempotent
keyed upserts, (b) an append-only per-entity event log with per-event
provenance, (c) concurrency-safe writes.** tars supplies the Worker/Plan
machinery (Doc 04) and the event store (Doc 17); the consumer supplies the
blackboard schema.

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
