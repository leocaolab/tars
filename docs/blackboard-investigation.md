# Blackboard investigation — debugging the durable state a pipeline writes

`docs/observability.md` covers three grains: what the agent **decided**
(trajectory), what each LLM **call** did (events DB), and what's happening
**now** (tracing). This is the fourth grain: **what the pipeline actually wrote
to its durable domain state** — A.R.C.'s `findings` + `finding_events`
blackboard, or any consumer store written per
[Doc 19](./architecture/19-blackboard-pipeline.md).

It's the grain where the hardest bugs live, because it's the only state that
outlives the run. The design is
[`architecture/24-pipeline-investigation.md`](./architecture/24-pipeline-investigation.md);
this is the how-to.

> **When to reach for this.** A row on the board is wrong, missing, stale, or
> duplicated. The trajectory and events DB look fine — the model decided
> correctly, the call returned correctly — but the *persisted state* is wrong.
> That gap is exactly what this surface explains.

---

## The motivating bug (so the commands make sense)

The reference incident (arc retro, `docs/retro/2026-06-11-patch-on-patch-
backfill.md`): the finding panel showed findings with an empty **History** and
**Locations**. Root cause: the `found` event wasn't written at the scan — it was
*reconstructed later* by a `backfill` pass reading a legacy table. An unrelated
fix flipped whether the primary write succeeded, which silently broke the
backfill's guard → missing events. **Two wrong hypotheses were chased** before
the real mechanism, because there was no way to ask the blackboard "for this
finding, list every event, and for each, the step + write-path + commit that
produced it."

Each command below answers a question that, that day, had no answer.

---

## 1. `tars blackboard timeline <key>` — full provenance of one entity

The thing whose absence was the symptom. Every event for a key, each annotated
with **who wrote it** (write-path), **as primary or reconstruct**, in which
commit, and whether it passed the validation gate:

```bash
tars blackboard timeline F-12
```

```
key      F-12  rust_best_practices::unitless-primitive-quantity  src/qty.rs:88
event    commit          at                   writer            kind        gate
-----------------------------------------------------------------------------------
found    9adf052         11:02:14  scan_worker       primary     passed
fixed    arc/fix-12@ece  11:04:51  fix_worker        primary     passed
merged   main@4357d8b    11:09:02  merge_sweep       primary     —
```

Run it against the buggy state and the diagnosis is on the screen: the `found`
row is **missing**, or present with `writer=backfill kind=reconstruct` — which
says immediately "this fact was reconstructed, not born here." No hypothesis
needed.

Sweep the whole board to find the *population* of a bug:

```bash
# every finding whose origin event came from the reconstruct path, not the scan
tars blackboard timeline --all-keys --filter event=found,writer=backfill --json
```

---

## 2. `tars blackboard writers <table>` — the drift lint

The "two writers, one fact" detector. Lists every code path that writes a
source-of-truth table and flags any that bypasses the gate the primary path
runs:

```bash
tars blackboard writers findings
```

```
writer                       kind         runs gate?            verdict
-----------------------------------------------------------------------------
scan_worker                  primary      verify_persistable    ok
fix_worker                   primary      verify_persistable    ok
backfill_findings_index      reconstruct  (none)                ⚠ DRIFT
   └─ upserts `findings` but bypasses verify_persistable — a row this writer
      creates can re-introduce exactly what a primary write rejected.
      Fix: gate it, or make it event-only.
```

This is the runtime audit; the same registry powers a **build-time** unit test
(`no Reconstruct writer upserts a gated table ungated`) that stops the
regression from landing again. A validation rule on one writer is a suggestion;
this makes it an invariant.

---

## 3. `tars blackboard verify` — is this a patch-on-patch?

"不会是布丁打布丁的 fix 吧" as an assertion. Two checks:

```bash
tars blackboard verify --review <id>
```

```
round-trip:        ok (0 rows differ)
reconstruct-only:  ⚠ 7 `found` events exist ONLY via backfill_findings_index
                      → these 7 findings have no primary `found` write
                      → the scan step's write is incomplete (Doc 19)
```

- **round-trip** — reading the board into the working set and writing it back
  must be a no-op. A non-empty delta = the working set and the board disagree
  ("two books").
- **reconstruct-only** — with every backfill/reindex path disabled (dry-run),
  which live rows disappear? Any that do are a primary-write hole. Doc 19's
  rule: *if deleting the backfill would lose live data, the live write is
  incomplete — that's the bug.* The second line above **is** the whole retro
  bug, reported before any user sees an empty panel.

---

## 4. `tars blackboard replay <step> --review <id>` — per-step delta

The deepest tool — re-run one DAG step in isolation against the state its
predecessor left, with the same recorded model responses (deterministic, no live
model), and diff the blackboard delta it writes against what the live run
recorded:

```bash
tars blackboard replay scan --review <id> --against recorded
```

```
step `scan` replay delta vs recorded:
  findings:       +12  (match)
  finding_events: +1   ✗  expected +12 `found`
                       → 11 findings got a row but NO `found` event
                       → writer scan_worker emitted the row, deferred the event
```

Had this existed, the retro's mechanism would have shown as a one-line delta the
moment the symptom appeared — instead of after two dead ends. The step runs in a
throwaway worktree against a copy of the store, so it never mutates the real
blackboard. (Same replay engine as a golden test failure — see
[`testing-agents.md`](./testing-agents.md) §6.)

---

## 5. `tars blackboard lint` — provenance-collapse & staleness

Cheap whole-DB checks for the [Doc 19](./architecture/19-blackboard-pipeline.md)
anti-patterns; run it as a CI gate:

```bash
tars blackboard lint --review <id>
```

| Lint | Flags |
|---|---|
| **Collapsed provenance** | a finding whose `found` and `fixed` events share the same `(at, commit)` — a run-end batch stamped both with one run-level pair |
| **Base-HEAD fix pointer** | a `fixed` event whose commit == the review's base HEAD — the real fix commit didn't exist when the write ran |
| **Deferred ownership** | a `found` event written by a `*backfill*` / `*finalize*` path on a live row, not by `scan_worker` |

---

## Pivot to the other grains

The blackboard event's writer tag records the trajectory **step** that produced
it, so you can walk from a wrong row all the way back to the LLM exchange that
caused it:

```bash
# 1. which step wrote this bad `fixed` event?
tars blackboard timeline F-12 --json | jq '.[] | select(.event=="fixed") | .step'
#   → { trajectory_id: "…", step_seq: 4 }

# 2. what did that step decide?
tars trajectory show <trajectory_id> | jq -c 'select(.step_seq==4)'

# 3. what did the Critic↔Fixer LLM calls in that step actually say?
tars trajectory show <trajectory_id> \
  | jq -r 'select(.step_seq==4 and .type=="llm_call_captured") | .event_id' \
  | xargs -I{} tars events show {} --with-bodies
```

One spine, four grains, no manual correlation.

---

## "I want to debug X → look at Y"

| Symptom | First command |
|---|---|
| A finding's History/Locations is empty | `tars blackboard timeline <key>` — is the origin event missing or `kind=reconstruct`? |
| A rejected finding is somehow on the board | `tars blackboard writers <table>` — which writer bypassed the gate? |
| "Is this a backfill patch-on-patch?" | `tars blackboard verify --review <id>` — any reconstruct-only live rows? |
| A step produced the wrong rows | `tars blackboard replay <step> --review <id>` — diff the delta |
| The timeline can't say *when*/*which commit* | `tars blackboard lint` — collapsed provenance? base-HEAD pointer? |
| "Which LLM call caused this wrong row?" | `timeline … .step` → `tars trajectory show` → `tars events show --with-bodies` |

## See also

- [`architecture/24-pipeline-investigation.md`](./architecture/24-pipeline-investigation.md) — the design + roadmap
- [`architecture/19-blackboard-pipeline.md`](./architecture/19-blackboard-pipeline.md) — the *write* contract this debugs
- [`observability.md`](./observability.md) — the trajectory / events / tracing grains this pivots to
- [`testing-agents.md`](./testing-agents.md) — prevent the bug: golden e2e tests share this replay engine
