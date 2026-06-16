# Doc 28 — Multi-Agent Collaboration Model

> Status: design, 2026-06-16. Author: Leo Cao. Layer: the agent-**interaction
> protocol** — who talks to whom, via what medium, how a yes-man stays safe,
> how a spec flows idea→product. Composes with [Doc 27](./27-spec-as-reward-mcts.md)
> (the search/reward ENGINE — runs *inside* one milestone here). Wraps
> [Doc 04](./04-agent-runtime.md) `run_task`; reuses [Doc 19](./19-blackboard-pipeline.md)
> (blackboard/worktrees), [Doc 26](./26-tool-trajectory-eval.md) (eval = reward).
> Verifier content grounded in A.R.C.'s retros
> (`github.com/leocaolab/arc/docs/retro`).

---

## 1. Overview & goal — the disease and the cure

**The master pathology: a yes-man without a verifier.** An agent that says *yes
to motion* (the cure for stop-and-ask) but runs with **no grounded verifier
saying no to incorrectness** plows, confidently and fast, straight into the
recurring bugs. A.R.C.'s 11 retros are the **fossil record** of exactly this —
`many-writers-one-truth`, `patch-on-patch-backfill`, `default-masks-failed-unbox`,
`floaty-progress` — each is "an agent proceeded, nothing checked it." The retros
themselves prescribe the cure: every retro's **"Detect it by"** column *is a
detector = a check*.

So the two halves solve each other:

- keep the **yes-man** → kills stop-and-ask, drift, "session too long";
- **bind it to a grounded, independent verifier** (the retro detectors + tests)
  → kills *confident-wrong*.

The yes-man's "yes" is then **bounded**: fast everywhere **except where a
detector fires.**

**THE LOAD-BEARING INVARIANT:** *the yes-man says yes to MOTION, never to
CORRECTNESS.* Correctness is the Verifier's monopoly. **A yes-man + a weak
verifier = fast confident failure = the retros.** Never ship the yes-man alone.

This doc is the **collaboration protocol** that enforces that invariant across a
team of agents taking a spec to a product.

**Non-goals.**
- Not the search engine (Doc 27) — that runs inside one milestone here.
- Not a new runtime — wraps `run_task` (`tars-runtime/src/task.rs:210`).
- **No free agent-to-agent chat** (that's the drift/rabbit factory). **No
  synchronous agent→human blocking call** (that's stop-and-ask). **No auto-merge
  to `main`** (human-gated).
- Does **not** degrade gracefully: where the verifier is weak (taste/judgment
  work), the model collapses back to human-in-the-loop. That's the honest
  fallback, stated up front — **autonomy is bounded by spec testability.**

---

## 2. Critical User Journeys

- **CUJ-1 — spec → product, hands-off between gates.** Actor: a developer.
  Trigger: hands a *testable* spec to the team. Steps: the roster takes it to
  product via blackboard-mediated collaboration — Controller/yes-man drives,
  bounded by the Verifier, escalating only on a spec hole, **zero free
  discussion, zero stop-and-ask**. Success: product (all spec items
  verifier-green); the human was touched only at the two gates.
- **CUJ-2 — a detector blocks a yes-man's bad commit.** Actor: the system.
  Trigger: the Executor proposes a change that adds a *second writer* for one
  event (the `many-writers-one-truth` bug). Steps: the Verifier's detector
  fires → the Controller cannot commit that branch (yes-to-motion ≠
  yes-to-correctness). Success: the recurring bug is **structurally blocked**,
  not re-introduced; the branch is abandoned, an alternative tried.
- **CUJ-3 — Blocked parks, never stalls.** Actor: the system. Trigger: milestone
  B hits a spec hole (`Unevaluable`). Steps: B writes `Blocked{question}`, parks;
  the Controller keeps independent milestones A,C running; a batched digest
  surfaces B to the human. Success: one blocked milestone **never halts the DAG**;
  the human resolves async, B resumes.
- **CUJ-4 — pull, not push.** Actor: a developer not watching. Trigger: agents
  finish steps. Steps: each writes a `Done/Rabbit/Blocked` record and advances —
  **none calls the human**. The human pulls a dashboard (spec items + their
  state) when they choose. Success: **0 synchronous blocking calls**; the human
  reviews on their cadence.
- **CUJ-5 — design-formation pro/con without discussion.** Actor: an architect.
  Trigger: a fork with several viable designs. Steps: a **judge-panel** generates
  N candidates in parallel (not conversing), scores each on fixed dimensions,
  adversarially refutes, emits a **pro/con matrix**. Success: the architect reads
  the matrix and decides — no open chat.
- **CUJ-6 — the spec flows.** Actor: the system. Trigger: an Executor finishes
  `FR-3`. Steps: the Verifier flips `FR-3` to `done`+evidence on the blackboard;
  dependents of `FR-3` become eligible; the critique feeds the Planner's next
  proposal. Success: progress IS the spec's items changing state — no separate
  progress doc; the dashboard is the spec-with-state.

## 3. Feature list

| Feature | Serves | Notes |
|---|---|---|
| F1 — Blackboard record protocol (`Done`/`Rabbit`/`Blocked`) | CUJ-1,3,4 | the ONLY inter-agent + agent→human channel; structured, not chat |
| F2 — Verifier-with-retro-detectors (the yes-man's leash) | CUJ-1,2 | tests/eval (code) + each retro's "Detect it by" as a check; bounds every commit |
| F3 — Controller (Yes-Man + Guard) | CUJ-1,2,3 | drives/selects/commits/advances-DAG/escalates/no-progress — code, never judges |
| F4 — Pull plumbing (Controller pull + Human pull + batched Notifier) | CUJ-3,4 | no per-event push; Blocked parks not halts |
| F5 — Role roster + anti-incest camps | CUJ-1,5 | Planner/Executor/Verifier/Evaluator/Controller + Human (+Synthesizer) |
| F6 — Spec-flow projection (items carry status+evidence) | CUJ-6 | spec+state = dashboard; 4-direction flow; `/design` IDs the join key |
| F7 — Judge-panel pro/con | CUJ-5 | structured tradeoff matrix replaces design discussion |
| F8 — SessionStart/Stop hooks (pull-in / post-out) | CUJ-4,6 | each agent = ephemeral fresh context over the durable blackboard |

## 4. Requirements

**Functional**

| # | Requirement | Feature |
|---|---|---|
| FR-1 | Every inter-agent / agent→human communication is a structured blackboard record `Done{item, evidence}` \| `Rabbit{idea, why_oos}` \| `Blocked{item, question}` — never free text to another agent, never a blocking call. | F1 |
| FR-2 | The Controller MAY commit a branch only if the Verifier returns no failing check (tests + retro-detectors). A failing detector is a hard veto the Controller cannot override. | F2,F3 |
| FR-3 | The Verifier runs a registry of detectors; each A.R.C. retro contributes ≥1 (e.g. `multiple-writers-one-truth`: count writers per `(table,event)` > 1). Adding a retro = adding a detector, no Controller change. | F2 |
| FR-4 | A `Blocked` record parks only its own milestone; the Controller continues every milestone whose deps are met. The DAG halts only when ALL live branches are blocked or done. | F3,F4 |
| FR-5 | The Human is consulted only on: product-gate, a `Blocked` (spec hole), backlog (`Rabbit`) disposition. No other path reaches the human. | F4 |
| FR-6 | The Notifier emits a **batched digest** on a threshold (N pending judgment items OR elapsed T), never one notification per event. | F4 |
| FR-7 | The Evaluator's model ≠ any doer's model (`ensure_anti_incest`); violation is a startup error. | F5 |
| FR-8 | Each spec `/design` ID carries `{status ∈ open|active|done|blocked, evidence, links}`; an agent action is a transition; `done` requires an evidence handle (commit/test). Terminal = all items `done`+evidence. | F6 |
| FR-9 | A design fork is resolved by a judge-panel emitting a `(candidate × dimension)` scored matrix + per-cell pro/con; never by agent discussion. | F7 |

**Non-functional** (measurable)

| # | Requirement | Threshold | Feature |
|---|---|---|---|
| NFR-1 | Synchronous agent→human blocking calls | **0** | F1,F4 |
| NFR-2 | Independent milestones halted by one `Blocked` | **0** | F4 |
| NFR-3 | In-scope items ending silent (neither verifier-green nor escalated) | **0 silent loose ends** | F6 |
| NFR-4 | Human escalations on a fully-testable spec | ≤ 1 per 50 committed steps | F2,F5 |
| NFR-5 | Per-event human notifications | **0** (batched only) | F6 |
| NFR-6 | A commit that a registered detector would flag | **0 reach the trunk** | F2 |

## 5. Infra

| Need | Exists? | Where / new |
|---|---|---|
| Blackboard = durable structured event store | ✅ | trajectory `AgentEvent` (`event.rs`), `LocalRuntime` (`runtime.rs`) |
| Branch/prune on the blackboard | ✅ | `TrajectoryStarted{parent}` / `TrajectoryAbandoned` (`event.rs:107,127`) |
| Rollout substrate (actor/critic) | ✅ | `run_task(orchestrator,worker,critic,…)` (`task.rs:210`) |
| Worktree isolation per branch | ✅ | Doc 19 §1,§5 |
| Grounded checks (tests/eval) | ✅ | `CheckRunner`/`Invariant` (`check.rs:58`), Doc 26 |
| Anti-incest separation | ✅ | `ensure_anti_incest` (`judge.rs:76`) |
| Record protocol / pull / Notifier / Controller | ➕ new | `tars-collab` (or `tars-runtime::collab`) |
| Retro-detectors registry | ➕ new (content authored) | one check per arc retro |
| Spec-flow projection | ➕ new | item-state over the `/design` IDs |
| SessionStart/Stop hooks → blackboard | ➕ new (wiring) | `.claude/hooks/*` |

## 6. Components

### C1 — Blackboard record protocol (new)
- **Responsibility:** the ONLY channel between agents and to the human.
- **Reuses:** the trajectory event store (`event.rs`, `LocalRuntime::{append,replay}`)
  — records are appended events; `TrajectoryAbandoned` prunes a parked/lost branch.
- **New:** the three record types + routing.
- **Interface:**
  ```rust
  pub enum Record {
      Done    { item: ItemId, evidence: Evidence },     // verifier-green
      Rabbit  { idea: String, why_out_of_scope: String },// parked → backlog
      Blocked { item: ItemId, question: String },        // spec hole → park
  }
  pub fn post(bb: &Blackboard, by: RoleId, rec: Record) -> Result<()>; // never blocks
  ```

### C2 — Verifier (with retro-detectors) (new wiring; the yes-man's leash)
- **Responsibility:** the grounded ground truth that bounds every commit.
- **Reuses:** `CheckRunner`/`Invariant` (`check.rs:58`), eval + `trajectory_match`/
  `ArgEquivalenceJudge` (Doc 26), `CriticAgent::critique` (`critic.rs:74`) for the
  residue tier.
- **New:** `DetectorRegistry` — one `Detector` per arc retro (the "Detect it by").
- **Interface:**
  ```rust
  pub trait Detector { fn name(&self) -> &str;
      fn check(&self, branch: &Worktree) -> Verdict; } // Pass | Veto{reason}
  // e.g. MultipleWritersOneTruth: writers_per(table,event) > 1 → Veto
  pub fn verify(spec: &RewardSpec, branch: &Worktree, reg: &DetectorRegistry)
      -> Verdict;  // any Veto ⇒ Controller cannot commit (FR-2)
  ```

### C3 — Controller (Yes-Man + Guard) (new)
- **Responsibility:** drive — select, commit (iff Verifier passes), advance the
  DAG, escalate on `Blocked`, terminate on no-progress. Code, deterministic.
- **Reuses:** `run_task` (`task.rs:210`) per milestone; Doc 27's PUCT when a
  milestone needs search.
- **New:** the loop + the commit-gate (Verifier veto) + the DAG scheduler.
- **MUST NOT** judge correctness — only commits what the Verifier already passed.

### C4 — Pull plumbing: Controller-pull + Human-pull + Notifier (new)
- **Responsibility:** the Controller pulls the blackboard continuously to drive
  the automatable DAG; the Human pulls only {product-gate, blocked, backlog}; the
  Notifier batches.
- **Reuses:** `LocalRuntime::replay` to read the blackboard.
- **New:** `Notifier::digest(threshold)`; a read-model/dashboard projection.

### C5 — Roles + anti-incest camps (new bindings over existing agents)
- **Reuses:** `OrchestratorAgent::plan` (`orchestrator.rs:473`)=Planner;
  `WorkerAgent`(`worker.rs`)=Executor; `CriticAgent`(`critic.rs:74`)=Evaluator;
  `ensure_anti_incest`(`judge.rs:76`)=the doer/judge line.
- **New:** the role contracts + the must-NOTs (§7 table).

### C6 — Spec-flow projection (new)
- **Responsibility:** make the spec a living state machine — each `/design` ID
  carries `{status, evidence, links}`; agents transition; spec+state = dashboard.
- **Reuses:** the `/design` ID discipline (CUJ-n/FR-n/Mn/E2E-n) as the join key.
- **New:** the item-state store + the 4-direction flow (↓ compile, ↑ verify+
  Reflexion, ↗ rabbit→child-spec, ↙ escalate→amend).

### C7 — Judge-panel pro/con (new)
- **Responsibility:** resolve a design fork without discussion.
- **Reuses:** the `/design` evaluation dimensions (reuse map, non-goals,
  reliability/security sections) as the scoring columns; parallel agents.
- **New:** `judge_panel(n, dims) -> ProConMatrix`.

### C8 — SessionStart/Stop hooks (new wiring)
- **Responsibility:** wire each ephemeral agent to the durable blackboard.
- **New:** `SessionStart` → pull the relevant spec-slice into a fresh context;
  `Stop` → post the `Record`. (Claude Code has these; **no `PreCompact`** — so
  cross-agent orchestration is the Controller, above Claude Code.)

## 7. Interfaces with other modules

The role contract table (the both-directions interface of the collaboration):

| Role | reads (blackboard) | writes | tars symbol | MUST NOT |
|---|---|---|---|---|
| Planner | spec items + last critique | candidate steps | `OrchestratorAgent::plan` `orchestrator.rs:473` | execute / score |
| Executor | chosen step + reuse map | a worktree diff | `WorkerAgent` `worker.rs` | self-grade / pick next |
| Verifier | a branch + spec checks + detectors | `Pass`/`Veto` | `CheckRunner` `check.rs:58` + new detectors | trust "looks right" |
| Evaluator | residue + criteria | value + critique | `CriticAgent::critique` `critic.rs:74` (+`ensure_anti_incest`) | share doer's model |
| Controller | the whole blackboard + Verifier verdict | commit / park / advance / escalate | `run_task` `task.rs:210` | judge correctness |
| Human | dashboard (pull) | gate decisions + spec amendments | — | be a runtime value oracle |

## 8. Main algorithms

### The collaboration loop (pull, verifier-bounded)
```
loop over the milestone DAG (Controller pulls continuously):
  m = next milestone with deps met
  cands = Planner.propose(spec_slice(m), last_critique(m))         # F5, no chat
  for c in cands (each in an isolated worktree):                   # Doc 19
     diff   = Executor.do(c)                                       # F5
     verdict = Verifier.verify(spec, c.worktree, detectors)        # F2 — the leash
     if verdict is Veto: Blackboard.post(Abandoned{c, reason}); continue   # CUJ-2
     Evaluator.critique(c) → blackboard (Reflexion → next Planner) # one-way
     if all items(m) done+evidence: Blackboard.post(Done{m, evidence}); break
  if m hit a spec hole: Blackboard.post(Blocked{m, question}); park m       # CUJ-3
  # Controller commits ONLY verifier-passed branches; it never overrides a Veto
advance: other DAG branches keep running; Notifier batches judgment items
```
Invariants: (1) **commit ⟹ Verifier passed** (the load-bearing invariant);
(2) a `Blocked` parks one branch, never the DAG (FR-4); (3) no agent ever blocks
on the human. Edge cases: all cands vetoed → no-progress counter++ → escalate;
all live milestones blocked → halt with the set of open questions.

### Record routing (who pulls what)
```
on Blackboard.post(rec):
  Done{m}    → Controller: mark items done+evidence; release dependents
  Rabbit{r}  → backlog; (human disposes later — pull)
  Blocked{m} → park m; enqueue to human-review; Notifier.maybe_digest()
on Human.pull():  acts only on {product-gate, blocked, backlog} → writes amendments
```

### Spec-item state machine (the flow)
```
open --Planner.pick--> active --Executor+Verifier--> done(+evidence)
                                       └--Veto/hole--> blocked --human--> open'
```
`done` requires evidence (FR-8). The traceability graph (CUJ→FR→Component→E2E)
is the flow graph: a `done` FR releases the CUJ items that depend on it.

### Judge-panel pro/con (no discussion)
```
cands = parallel_generate(n)                       # not conversing
matrix[c][dim] = score(c, dim)  for dim in {coverage, simplicity, reuse,
                                            risk/reversibility, cost, failure-modes}
refute each c adversarially; attach the surviving pro/con per cell
return matrix  → human reads, picks (or grafts)    # CUJ-5
```

## 9. Integration / E2E tests

| Test | CUJ | Setup → Action → Assertion |
|---|---|---|
| E2E-1 | CUJ-1 | seeded spec (mock provider) whose items flip green when applied → run team → `product`; assert **0 human prompts** + every item `done`+evidence. |
| E2E-2 | CUJ-2 | Executor proposes a 2nd writer for one event → the `multiple-writers-one-truth` detector `Veto`s → Controller does NOT commit; branch abandoned (NFR-6: 0 reach trunk). |
| E2E-3 | CUJ-3 | milestone B `Blocked`; A,C independent → assert A,C still complete; DAG not halted (NFR-2=0); B parked + in digest. |
| E2E-4 | CUJ-4 | agents finish steps → assert every "done" is a posted `Record`, **0 blocking calls** (NFR-1); human pull returns the item states. |
| E2E-5 | CUJ-5 | 3 design candidates, mock dimension scores → judge-panel returns a matrix; assert no agent-to-agent message exchanged. |
| E2E-6 | CUJ-6 | Executor finishes `FR-3` → assert `FR-3` state=`done`+evidence on blackboard AND its dependents become eligible (the dashboard == spec-with-state, no separate progress doc). |
| E2E-7 (unit) | FR-7 | Evaluator model == a doer model → startup error (anti-incest). |

## 10. Success criteria
- [ ] FR-1…FR-9 met.
- [ ] NFR-1…NFR-6 thresholds hit (0 blocking calls; 0 independent-halts; 0 silent
  loose ends; ≤1 escalation/50 steps on a testable spec; 0 per-event pushes; 0
  detector-flaggable commits on trunk).
- [ ] Every CUJ's E2E passes (§9).
- [ ] Dogfood: replay an arc retro scenario (the 4-writer fix) → the detector
  blocks the recurrence that originally shipped (CUJ-2 on real history).

## 11. Performance considerations
Hot path = the Verifier (runs per branch) — keep detectors cheap and ordered
cheap→expensive (compile/grep-shaped detectors before the LLM residue tier;
Doc 26's tiered reward). The blackboard is append-only; the dashboard is a
projection (replay or an incremental read-model). Notifier batching bounds human
interrupt cost to O(digests), not O(events). Measure: detector cost/branch,
commits-blocked-by-detector, escalations/step.

## 12. Reliability considerations
- **Yes-man-without-verifier (THE failure mode):** structurally prevented — the
  Controller's commit-gate is `Verifier.verify() != Veto` (FR-2); there is no
  code path that commits an unverified branch. If the detector registry is empty,
  the system is a bare yes-man = the retro factory → **a non-empty Verifier is a
  startup precondition**, not optional.
- **Blocked-parks-not-halts** (FR-4) keeps liveness under spec holes.
- **No-progress guard** (all cands vetoed K rounds) → escalate, never spin.
- **Crash-safe:** blackboard = event-sourced; replay reconstructs; parked/lost
  branches are `TrajectoryAbandoned`, idempotent.
- **Honest degradation:** weak verifier ⇒ escalation rate climbs ⇒ collapses to
  human-in-loop (the fallback), never silent-wrong.

## 13. Security considerations
The Controller (yes-man) removes the human approval gate between steps, so the
**tool sandbox is the only runtime guard**: the Executor runs **read-only,
allow-listed, worktree-jailed tools** (Doc 26 §15) — never `bash`/write outside
its branch. The human gates remain on the two endpoints (spec-in, product-out);
**no auto-merge to `main`.** Anti-incest (FR-7) stops a model grading its own
work. Blackboard records are append-only (no in-place edit of history).

## 14. Abstraction & reuse
Approach: **wrap, don't build.** `run_task` is already actor→executor→critic; the
trajectory log is already a branchable blackboard; Doc 26 eval is already a
grounded verifier. Doc 28 adds only the *protocol*: the record types (Done/Rabbit/
Blocked), the pull plumbing, the Controller's verifier-bounded commit-gate, the
spec-flow projection, the retro-detectors registry, the judge-panel. Nothing else.

**Reuse map**
| Symbol | Location | How the collaboration model uses it |
|---|---|---|
| `run_task(orchestrator,worker,critic,…)` | `tars-runtime/src/task.rs:210` | per-milestone rollout substrate |
| `OrchestratorAgent::plan` | `orchestrator.rs:473` | Planner |
| `WorkerAgent` | `worker.rs` | Executor (sandboxed, Doc 26 §15) |
| `CriticAgent::critique` | `critic.rs:74` | Evaluator (residue + Reflexion critique) |
| `CheckRunner`/`Invariant` | `check.rs:58` | Verifier's test/eval tier |
| `ArgEquivalenceJudge` / eval | Doc 26 (`tars-runtime`) | Verifier's eval/judge tiers |
| `ensure_anti_incest` | `judge.rs:76` | doer/judge camp separation (FR-7) |
| `TrajectoryStarted{parent}` / `TrajectoryAbandoned` | `event.rs:107,127` | blackboard branch + prune |
| `LocalRuntime::{append,replay}` | `runtime.rs` | post/read the blackboard |
| Doc 27 PUCT loop | Doc 27 | the search *inside* a stuck milestone |
| arc retros' "Detect it by" | `arc/docs/retro/*.md` | the Verifier's detectors (F2/FR-3) |

**New abstractions (justified):** the **record protocol** (Done/Rabbit/Blocked —
the structured-not-chat channel that kills drift); the **Controller commit-gate**
(the leash that turns a yes-man safe — there is no other place this invariant can
live); the **detector registry** (turns the retros from prose into checks); the
**spec-flow projection** (makes the spec the live state, killing floaty-progress);
the **judge-panel** (structured pro/con replacing discussion). Each is new because
no existing tars piece carries the *protocol* — only the substrate.

---

## Roadmap

Ship the **smallest slice that makes a yes-man SAFE** first — because a yes-man
without the leash is the retro factory.

- **M0 — the leash.** Scope: C1 record protocol (`Done/Rabbit/Blocked`) + C2
  Verifier with **one** retro-detector (`multiple-writers-one-truth`) + C4 pull +
  the Controller commit-gate (`commit ⟺ no Veto`). Delivers: FR-1,2,3 (one
  detector); NFR-1,6. Verified by: **E2E-2** (detector blocks the bad commit) +
  E2E-4 (pull, 0 blocking calls). *Risk-up-front: the commit-gate + a real
  detector ARE the cure; build them before any roster/search, so a yes-man is
  bounded from line one.*
- **M1 — the roster + DAG + park.** Scope: C5 roles (anti-incest) + C3 Controller
  DAG scheduler + `Blocked`-parks-not-halts. Delivers: FR-4,5,7; NFR-2,3.
  Verified by: E2E-1, E2E-3, E2E-7. Depends: M0.
- **M2 — spec flows.** Scope: C6 spec-flow projection (items carry status+evidence;
  4-direction flow) + the dashboard. Delivers: FR-8; NFR-3 (0 silent loose ends).
  Verified by: E2E-6. Depends: M1.
- **M3 — the rest of the detectors + Notifier batching.** Scope: a Detector per
  remaining arc retro; C4 Notifier digest. Delivers: FR-3 (full), FR-6; NFR-4,5.
  Verified by: dogfood (§10) — replay retro scenarios, each detector blocks its
  recurrence. Depends: M0.
- **M4 — judge-panel + search-inside-a-milestone.** Scope: C7 judge-panel pro/con
  + wire Doc 27's PUCT for a stuck milestone. Delivers: FR-9. Verified by: E2E-5.
  Depends: M1.

M0 alone is the thesis made real: **a yes-man that cannot commit what a detector
vetoes.** Everything else is breadth on top of that single safety property.
