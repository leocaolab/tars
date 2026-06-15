# Doc 27 — Spec-as-Reward MCTS Development

> Status: design, 2026-06-15. Author: Leo Cao. Builds on Doc 04 (Agent
> Runtime / trajectory tree), Doc 16/18/26 (the evaluation framework = the
> reward), Doc 19 (worktree isolation). Reference lineage: AlphaZero, LATS
> (Language Agent Tree Search), Reflexion, process-reward / "Let's verify step
> by step".
>
> One line: turn a `/design` spec into a **reward function**, then let an
> autonomous loop — *propose → commit → roll out → score → backprop → prune* —
> search the implementation tree until the spec's checks pass, asking a human
> only when the spec itself is underspecified.

---

## 1. Theoretical basis (why spec + MCTS, not tooling taste)

A bare LLM agent is a **policy** `π(a|s)` with **no value function** `V(s)` and
**no planning**: autoregression is greedy with horizon 0 — it optimizes "the
next move looks locally plausible," never "the trajectory's return." The three
observed pains of long-horizon agents are precisely the three faculties a bare
policy lacks:

| Pain | Mechanism | Missing faculty |
|---|---|---|
| **注意力漂移 / attention drift** | the objective is diluted in a growing context (lost-in-the-middle); no `V(s)` gradient pulls toward the goal | anchored objective + **value** |
| **总停下来问 / stop-and-ask** | a high-uncertainty fork with no try-and-verify mechanism → the agent outsources the decision to the human as a value oracle | **value oracle** |
| **不知道下一步 / no next step** | greedy argmax over a flat policy, no lookahead | **planning** |

The cure supplies exactly those three:

- **spec → reward.** A `/design` spec's CUJs / E2E tests / success criteria
  **compile** to a reward `R(state)` + a terminal predicate. The objective stops
  being a sentence stated once and becomes a signal re-asserted at every node.
- **grounded verifier → value.** Running the spec's checks (compile → test →
  eval) yields `V(s)` — a *cheap* progress estimate. This exploits the
  **generation–verification gap**: verifying is cheaper and more reliable than
  generating, so we build a strong solver from a weak generator + a cheap
  grounded verifier + search.
- **MCTS → planning + persistent objective.** Lookahead via simulation, value
  backup, and (as a side effect) the objective re-evaluated at each visited
  node.

This is the AlphaZero decomposition with the pieces swapped: **LLM = policy
prior, verifier = value, MCTS = planner, spec = reward.** The closest published
framework is **LATS** (MCTS over LLM agent actions with environment-feedback
value + reflection); **Reflexion** supplies the gradient-free backprop
(natural-language critique as the value-update signal); **process-reward /
rStar / AlphaCodium** supply the verifier-grounded-search-for-code precedent.

### 1.1 The actor–critic + controller decomposition

Three agents, the minimal complete set, mapping 1:1 to RL/MCTS roles — and each
killing one pain:

| Agent | RL/MCTS role | Kills |
|---|---|---|
| **Planner** ("从讨论生成进一步计划") | policy `π` / node **expansion** | no-next-step |
| **Goal-Eval** ("eval 目的是否达成") | **reward `R` / value `V`** + terminal + drift detector | drift |
| **Yes-Man** | **controller / liveness** (select + commit, never returns to human) | stop-and-ask |

Plus a non-agent **No-Progress Guard** (convergence: K idle rounds → prune /
backtrack / escalate; + a token/step budget) — without it the yes-man perpetuates
forever.

### 1.2 The load-bearing safety invariant

**The yes-man says YES to *motion*, never to *correctness*.** Correctness is
Goal-Eval's exclusive monopoly. Separating "drive forward" from "judge correct"
is the entire reason a yes-man is safe: a yes-man + a weak verifier = *fast,
confident failure*. Therefore the yes-man may exist **only** packaged with a
**grounded** (runs real tests/eval, not vibes) and **independent** (anti-incest:
a different model from planner/executor) Goal-Eval.

### 1.3 The upstream insight (the real cure for stop-and-ask)

**The yes-man's autonomy ceiling = the spec's testability.** An agent stops to
ask precisely when it hits a decision it cannot try-and-verify. If the spec has
concrete measurable success criteria, every *"should I?"* becomes *"does it pass
the check?"* — which Goal-Eval answers, so the yes-man never needs the human.
Hence the real fix lives **upstream, in the spec**: the `/design` output's E2E
tests + measurable success criteria pre-compile runtime decisions into a judge.
Vague spec → constant escalation; testable spec → near-full autonomy.
**"Stop and ask" downgrades from a runtime behavior to a spec-completeness
defect** — escalation is the system telling you the spec has a hole, not the
agent being timid.

**Non-goals.**
- Not for a linear roadmap. `M0→M1→…` is a topological order — *just execute*
  (we did exactly that for Doc 26's nine commits; MCTS there would be pure
  overhead). MCTS earns its cost only where there's real branching: design-
  approach **selection** and getting **unstuck**.
- Not thousands of simulations. Project rollouts are expensive (building a
  milestone = many agent-hours), so this is **AlphaZero-style few-real-rollouts**
  — the LLM is policy prior *and* value estimator; real build+test+eval rollouts
  happen sparingly at leaves to *correct* the estimates (§11).
- Not a new agent runtime. The actor/critic/rollout substrate is `run_task`
  (Doc 04). This adds the tree, the controller, and the reward wiring around it.
- Not auto-merge to `main`. The loop produces a *winning branch*; landing it is
  a separate human-gated step (the one place a human always stays).

---

## 2. Critical User Journeys (CUJs)

- **CUJ-1 — autonomous milestone.** Actor: a developer. Trigger: `tars develop
  --spec docs/.../26-*.md --milestone M2''` on a hard milestone. Steps: compile
  spec → reward; loop {Planner proposes N candidate steps → Yes-Man PUCT-selects
  & commits → Executor rolls out in a worktree → Goal-Eval scores against the
  spec's checks → backprop → prune}. Success: Goal-Eval reports the milestone's
  E2E tests pass; the winning branch is presented for review. The human is *not*
  consulted during the loop.
- **CUJ-2 — escalate only on a spec hole.** Actor: same. Trigger: mid-loop,
  Goal-Eval finds a success criterion it *cannot evaluate* (ambiguous /
  unmeasurable). Steps: the loop pauses, surfaces the exact criterion + the two
  branches it can't choose between. Success: the human's answer is recorded as a
  spec amendment (not a one-off), and the loop resumes — escalation count is a
  measured spec-quality metric.
- **CUJ-3 — get unstuck (micro-MCTS).** Actor: a developer whose agent is
  looping on a hard fix. Trigger: `tars develop --unstuck`. Steps: branch the
  current trajectory into K alternative action sequences in isolated worktrees,
  score each by the milestone's test/eval, keep the best, abandon the rest.
  Success: a branch passes where greedy execution didn't.
- **CUJ-4 — design-approach selection (macro, 1-ply).** Actor: an architect.
  Trigger: a requirement with several viable designs. Steps: Planner emits N
  design sketches; each gets a shallow rollout (sketch + the design's E2E);
  Goal-Eval + a judge-panel score; the best is synthesized (grafting runners-up
  ideas). Success: a chosen design with a recorded score rationale.
- **CUJ-5 — no-progress termination.** Actor: same. Trigger: K consecutive
  rounds with no `V` improvement. Steps: the guard prunes the stalled subtree,
  backtracks to the best ancestor, and either re-expands with a diversified
  prompt or escalates. Success: the loop never runs forever; it converges or
  escalates with a reason.

## 3. Feature list

| Feature | Serves | Notes |
|---|---|---|
| F1 — SpecRewardCompiler (spec → `RewardSpec`) | CUJ-1,2,4 | parses a `/design` doc's E2E tests + success criteria into tiered checks + a terminal predicate |
| F2 — Planner (candidate emission + critique intake) | CUJ-1,3,4 | extends `OrchestratorAgent::plan` to emit **N** ranked candidates and consume prior critique (Reflexion) |
| F3 — Goal-Eval (tiered grounded value) | CUJ-1,2,5 | compile→test→eval→judge; per-criterion verdict; terminal + drift signals; anti-incest |
| F4 — Yes-Man controller (PUCT select + commit) | CUJ-1,3 | drives the loop; escalates only on a spec hole (F6) |
| F5 — SearchTree + backprop (on the trajectory tree) | CUJ-1,3,4 | nodes = trajectory branches; expand/rollout/backprop/prune |
| F6 — Escalation gate (spec-hole only) | CUJ-2 | a human is consulted iff Goal-Eval returns `Unevaluable` |
| F7 — NoProgressGuard + budget | CUJ-5 | K-idle termination, token/step budget, backtrack-to-best |
| F8 — `tars develop` CLI | CUJ-1,3 | one command; `--unstuck` for micro mode |

## 4. Requirements

**Functional**

| # | Requirement | Feature |
|---|---|---|
| FR-1 | `SpecRewardCompiler::compile(spec_path)` parses a design doc into a `RewardSpec { tiers: [Compile, Test{cmd}, Eval{corpus,checks}, Judge{criteria}], terminal: AllOf(criteria) }`; an un-parseable/criteria-less spec is a hard error ("spec has no measurable success criteria"). | F1 |
| FR-2 | Planner emits `N` (default 4) candidate `PlanStep`s with a prior weight each, given `(RewardSpec, done-so-far, latest critique+eval)`. | F2 |
| FR-3 | Goal-Eval returns `Verdict { terminal: bool, value: f64∈[0,1], per_criterion: Vec<(id, Pass|Fail|Unevaluable, reason)> }`, evaluating cheapest tier first and short-circuiting on a tier that already decides. | F3 |
| FR-4 | Goal-Eval's judge tier is anti-incest: its provider ≠ planner/executor provider (`ensure_anti_incest`); violation is a startup error. | F3 |
| FR-5 | The Yes-Man selects the next node by PUCT over children's `(value, visit_count, prior)` and commits a rollout **without human input**; it never overrides a `Fail` into a pass. | F4 |
| FR-6 | A human is consulted **iff** Goal-Eval yields a `Unevaluable` criterion; the answer is persisted as a spec amendment + an escalation event. | F6 |
| FR-7 | Each rollout runs in an isolated git worktree; a losing branch emits `TrajectoryAbandoned`; the winning branch is never auto-merged to `main`. | F5 |
| FR-8 | NoProgressGuard terminates after `K` (default 3) idle rounds (no `value` improvement over the incumbent best) or on budget exhaustion, returning the best branch found. | F7 |

**Non-functional** (measurable)

| # | Requirement | Threshold | Feature |
|---|---|---|---|
| NFR-1 | Human escalations on a *fully-testable* spec | ≤ 1 per 50 committed steps | F3,F6 |
| NFR-2 | Loop terminates (converge or escalate) | within `K` idle rounds AND ≤ budget; never unbounded | F7 |
| NFR-3 | Cheap-tier short-circuit: a compile-failing rollout is scored without running the (expensive) judge | 0 judge calls when an earlier tier decides | F3 |
| NFR-4 | Reward determinism: same artifacts + same spec → same compile/test/eval value (judge tier excepted) | deterministic non-judge tiers | F1,F3 |
| NFR-5 | Executor tool sandbox: any tools the rollout agent runs are read-only + jailed (Doc 26 §15) | no writes outside the branch worktree | F4 |

## 5. Infra

| Need | Exists? | Where / new |
|---|---|---|
| Event-sourced trajectory **tree** (branches) | ✅ | `AgentEvent::TrajectoryStarted{parent}` / `TrajectoryAbandoned` (`event.rs:107,127`); `LocalRuntime` (`runtime.rs`) |
| Rollout = one task run (actor/critic) | ✅ | `run_task(runtime, llm, orchestrator, worker, critic, goal, config, cancel)` (`task.rs:210`) |
| Worktree isolation per branch | ✅ | Doc 19 §1/§5 |
| Grounded reward (checks/eval/judge) | ✅ | `CheckRunner`/`Invariant` (`check.rs`), eval CLI (`eval.rs`), `trajectory_match`/golden/`ArgEquivalenceJudge` (Doc 26) |
| Anti-incest model separation | ✅ | `ensure_anti_incest` (`judge.rs:76`) |
| PUCT search + backprop + tree value store | ➕ new | `tars-develop` crate (or `tars-runtime::search`) |
| SpecRewardCompiler | ➕ new | parse a `/design` doc → `RewardSpec` |
| Yes-Man controller + NoProgressGuard | ➕ new | the loop driver |

## 6. Components

### C1 — SpecRewardCompiler (new)
- **Responsibility:** parse a `/design` doc into a machine reward.
- **Reuses:** the design-doc structure this very doc follows (§9 E2E table, §10
  success criteria); `eval.rs` corpus/check vocabulary for the Eval tier.
- **New:** the parser + `RewardSpec`.
- **Interface:**
  ```rust
  pub struct RewardSpec { pub tiers: Vec<RewardTier>, pub terminal: TerminalPredicate }
  pub enum RewardTier {
      Compile,                                   // cargo check / build
      Test { cmd: String },                      // the milestone's E2E test(s)
      Eval { corpus: PathBuf, checks: Vec<String> }, // tars eval (Doc 26 checks)
      Judge { criteria: Vec<Criterion> },        // LLM-judged, un-checkable remainder
  }
  pub fn compile(spec_path: &Path, milestone: &str) -> Result<RewardSpec>;
  ```

### C2 — Planner (extends OrchestratorAgent)
- **Responsibility:** policy `π` — emit `N` ranked candidate next-steps from the
  blackboard + latest critique (Reflexion replan).
- **Reuses:** `OrchestratorAgent::plan` (`orchestrator.rs:473`), `Plan` /
  `PlanStep{worker_role,instruction,depends_on,condition}` (`orchestrator.rs:49,64`).
- **New:** candidate fan-out (N plans, not 1) + a prior weight + a critique field
  in the planner request.
- **Interface:**
  ```rust
  pub struct Candidate { pub step: PlanStep, pub prior: f64 }
  pub async fn propose(&self, ctx: AgentContext, spec: &RewardSpec,
                       done: &Blackboard, critique: Option<&str>, n: usize)
      -> Result<Vec<Candidate>>;
  ```

### C3 — Goal-Eval (new; wraps Critic + eval)
- **Responsibility:** reward `R` / value `V` + terminal + drift; tiered + grounded
  + anti-incest.
- **Reuses:** `CheckRunner`/`Invariant` (`check.rs:58`), `trajectory_match` /
  `ArgEquivalenceJudge` (Doc 26), `CriticAgent::critique` (`critic.rs:74`) for the
  judge tier, `ensure_anti_incest` (`judge.rs:76`).
- **New:** the tier ladder + `Verdict`.
- **Interface:**
  ```rust
  pub enum CriterionStatus { Pass, Fail, Unevaluable }
  pub struct Verdict { pub terminal: bool, pub value: f64,
                       pub per_criterion: Vec<(String, CriterionStatus, String)> }
  pub async fn evaluate(&self, spec: &RewardSpec, branch: &Worktree) -> Result<Verdict>;
  ```

### C4 — SearchTree + PUCT + backprop (new)
- **Responsibility:** the MCTS over the trajectory tree.
- **Reuses:** `LocalRuntime` (`runtime.rs`) — nodes are persisted as trajectory
  branches (`TrajectoryStarted{parent}`); `TrajectoryAbandoned` prunes; `replay`
  reconstructs a subtree.
- **New:** `Node { trajectory_id, parent, prior, visits, value_sum }`, PUCT
  selection, `backprop`.
- **Interface:**
  ```rust
  fn select(&self) -> NodeId;                          // PUCT over children
  fn expand(&mut self, n: NodeId, cs: &[Candidate]) -> Vec<NodeId>;
  fn backprop(&mut self, leaf: NodeId, value: f64);
  fn best(&self) -> NodeId;                            // most-visited / highest-V
  ```

### C5 — Yes-Man controller + NoProgressGuard (new)
- **Responsibility:** drive the loop autonomously; escalate only on `Unevaluable`;
  terminate on convergence/idle/budget.
- **Reuses:** `run_task` (`task.rs:210`) as the rollout; the budget pattern.
- **New:** the loop + escalation gate + idle counter.
- **Interface:**
  ```rust
  pub struct DevelopConfig { pub fanout: usize, pub idle_k: u32, pub budget: Budget,
                             pub c_puct: f64 }
  pub async fn develop(spec: RewardSpec, planner: Planner, goal_eval: GoalEval,
                       rollout: Rollout, cfg: DevelopConfig)
      -> Result<DevelopOutcome>;   // Converged{branch} | Escalate{criterion} | Exhausted{best}
  ```

## 7. Interfaces with other modules

| Direction | Module | Symbol / signature | Purpose |
|---|---|---|---|
| calls → | `tars-runtime` | `run_task(runtime, llm, orchestrator, worker, critic, goal, cfg, cancel)` (`task.rs:210`) | one rollout (a simulation) |
| calls → | `tars-runtime` | `OrchestratorAgent::plan` (`orchestrator.rs:473`) | Planner base |
| calls → | `tars-runtime` | `CriticAgent::critique(ctx, plan, result, goal)` (`critic.rs:74`) → `VerdictKind` | judge tier |
| calls → | `tars-runtime` | `ensure_anti_incest(judge_id, &[provider])` (`judge.rs:76`) | Goal-Eval independence |
| calls → | `tars-runtime` | `LocalRuntime::{append,replay}` + `TrajectoryStarted{parent}` / `TrajectoryAbandoned` (`event.rs:107,127`) | tree persistence + prune |
| calls → | `tars-cli`/`tars-runtime` | eval `CheckRunner` / `trajectory_match` / `ArgEquivalenceJudge` (Doc 26) | grounded reward tiers |
| ← run by | human | escalation prompt (only on `Unevaluable`) | spec-hole resolution |

## 8. Main algorithms

### The develop loop (PUCT + tiered reward)
```
spec = SpecRewardCompiler::compile(spec_path, milestone)        # FR-1
root = tree.new_root(initial_state)
idle = 0; best = root
while !budget.exhausted() && idle < K:
    leaf = tree.select(root)                                    # PUCT, FR-5
    critique = goal_eval.last_critique(leaf)
    cands = planner.propose(spec, blackboard(leaf), critique, N) # F2 (Reflexion)
    children = tree.expand(leaf, cands)
    for c in children (concurrently, each in its own worktree): # FR-7
        outcome = run_task(... goal=c.step.instruction ...)     # rollout
        v = goal_eval.evaluate(spec, c.worktree)                # FR-3 tiered
        if v.has_unevaluable(): return Escalate{criterion}      # FR-6 (CUJ-2)
        tree.backprop(c, v.value)                               # F5
        if v.terminal: return Converged{branch: c.worktree}     # CUJ-1
    if tree.best().value <= best.value: idle += 1               # FR-8
    else: idle = 0; best = tree.best()
    prune_dominated(tree)                                       # TrajectoryAbandoned
return Exhausted{best}
```
Invariants: the yes-man never converts a `Fail` to a pass (correctness ⟂
motion, §1.2); a node's `value` is monotone in backprop only via real leaf
evals, never the LLM's self-report. Edge cases: zero passing children → idle++;
all children `Unevaluable` → escalate the cheapest-to-clarify one; cancellation
→ persist tree, return best.

### Tiered reward (Goal-Eval)
```
for tier in [Compile, Test, Eval, Judge]:        # cheap → expensive (NFR-3)
    r = run(tier, branch)
    if tier.is_decisive(r): return Verdict(...)   # e.g. compile fails → value≈0, stop
value = weighted_sum(tier_scores)                 # spec defines weights
terminal = spec.terminal.eval(per_criterion)      # AllOf(success criteria)
```
Determinism: Compile/Test/Eval are deterministic (NFR-4); only Judge varies, and
it runs last on the smallest residue. Anti-incest checked at construction (FR-4).

### Spec → reward compile
```
parse design doc → { §9 E2E tests → Test/Eval tiers (cmd or corpus+checks),
                     §10 success criteria → terminal predicate + Judge criteria,
                     a criterion with no check/cmd → Judge or Unevaluable }
if no measurable criterion: error("spec has no measurable success criteria")  # the
                                          # stop-and-ask cure is enforced here (§1.3)
```

## 9. Integration / E2E tests

| Test | CUJ | Setup → Action → Assertion |
|---|---|---|
| E2E-1 | CUJ-1 | seeded repo + a spec whose Test tier is a known-failing E2E; mock planner emits a fix candidate; mock Goal-Eval flips to terminal once applied → `develop` returns `Converged`; **0 human prompts**. |
| E2E-2 | CUJ-2 | spec with one `Unevaluable` criterion → `develop` returns `Escalate{criterion}` naming it; no rollouts wasted after the gate. |
| E2E-3 | CUJ-3 | greedy single-branch fails the test; `--unstuck` fans out K branches, one mock-passes → best branch is the passing one; losers emit `TrajectoryAbandoned`. |
| E2E-4 | CUJ-4 | requirement with 3 design candidates, mock scores (0.3/0.7/0.5) → winner is the 0.7; runner-up ideas recorded. |
| E2E-5 | CUJ-5 | Goal-Eval never improves → after K idle rounds `develop` returns `Exhausted{best}`, never loops forever; budget path likewise. |
| E2E-6 (unit) | F3 | tiered reward short-circuits: a compile-failing branch yields value≈0 with **0 judge calls** (NFR-3). |
| E2E-7 (unit) | F1 | a spec with no measurable criteria → `compile` errors (the §1.3 enforcement). |

## 10. Success criteria
- [ ] FR-1…FR-8 met.
- [ ] NFR-1…NFR-5 thresholds hit (≤1 escalation/50 steps on a testable spec;
  bounded termination; 0 judge calls on cheap-tier decisions; deterministic
  non-judge tiers; sandboxed executor).
- [ ] Every CUJ's E2E test passes (§9).
- [ ] On Doc 26's `M2''` as the live spec, `tars develop` reaches the milestone's
  E2E green with **zero** human prompts (the dogfood: it would have built what we
  hand-built, without me stopping to ask).

## 11. Performance considerations
Hot path = **rollouts** (each = a `run_task`, i.e. agent-minutes). The cost model
forces AlphaZero-style design: **the LLM is policy prior AND value estimator**,
so most of the tree is explored with *imagined* value (cheap LLM `V̂`), and
**real** build+test+eval rollouts happen only at sparingly-chosen leaves to
*correct* `V̂` (backprop). Budgets: fan-out `N` small (4), `c_puct` tuned to
prefer exploiting a green branch; cheap reward tiers gate the expensive ones
(NFR-3); concurrent sibling rollouts cap at the worktree/CPU limit. Measure:
rollouts-to-convergence, judge-calls-per-run, escalations-per-step.

## 12. Reliability considerations
- **Yes-man runaway** → NoProgressGuard (K-idle) + hard budget (FR-8/NFR-2);
  backtrack-to-best on stall.
- **Bad-verifier failure (the dangerous one)** → a yes-man + weak Goal-Eval is
  fast-confident-wrong. Mitigations: Goal-Eval is **grounded** (real
  compile/test/eval, deterministic tiers dominate the judge tier) and
  **anti-incest** (FR-4); the judge tier scores only the residue and is
  capped/cached (Doc 26 args-judge pattern).
- **Crash mid-search** → the tree is the event-sourced trajectory log; replay
  reconstructs it (Doc 04 recovery). Losing branches are `TrajectoryAbandoned`,
  idempotent.
- **Spec drift** → the objective is re-asserted as the reward each node; a
  criterion that silently can't be evaluated escalates rather than passing.

## 13. Security considerations
Trust boundary: the executor runs **tool-using agents** driven autonomously — the
yes-man removes the human gate, so the *tool* sandbox is the only guard.
**Reuse Doc 26 §15 verbatim:** read-only allow-listed tools, `::with_root`-jailed
to the branch worktree; no `bash`/write outside it (NFR-5). Auto-merge to `main`
is out of scope — landing the winning branch stays human-gated. Secrets: the
judge/eval providers carry API keys via the existing config path; anti-incest
prevents a model grading its own output.

## 14. Abstraction & reuse
Approach: **don't build a runtime — wrap one.** `run_task` already *is*
actor(Orchestrator)→executor(Worker)→critic(Critic); the trajectory log already
*is* a tree (parent-branches); the eval framework already *is* the grounded
verifier. Doc 27 adds the three missing layers — a **reward compiler** (spec →
checks), a **search** (PUCT + backprop on the existing tree), and a **controller**
(the yes-man + guard) — and nothing else.

**Reuse map**
| Symbol | Location | How MCTS uses it |
|---|---|---|
| `run_task(...)` | `tars-runtime/src/task.rs:210` | one rollout / simulation |
| `OrchestratorAgent::plan` | `orchestrator.rs:473` | Planner (policy) base |
| `Plan` / `PlanStep` | `orchestrator.rs:49,64` | a candidate action |
| `CriticAgent::critique` → `VerdictKind` | `critic.rs:74` | Goal-Eval judge tier + Reflexion critique |
| `CheckRunner` / `Invariant` | `check.rs:58` | reward Test/Eval tiers |
| `trajectory_match` / `ArgEquivalenceJudge` | Doc 26 (`tars-runtime`) | reward Eval/judge tiers |
| `ensure_anti_incest` | `judge.rs:76` | Goal-Eval ≠ planner/executor |
| `AgentEvent::TrajectoryStarted{parent}` / `TrajectoryAbandoned` | `event.rs:107,127` | tree node/branch + prune |
| `LocalRuntime::{append,replay}` | `runtime.rs` | tree persistence + subtree reconstruction |
| worktree isolation | Doc 19 §1,§5 | branch state |

**New abstractions (justified):** `RewardSpec` + `SpecRewardCompiler` (the
spec→value bridge — the thing that makes a doc drive a loop); `SearchTree`/PUCT/
backprop (planning the existing tree lacks); the Yes-Man controller + NoProgress
Guard (autonomy + convergence). Each is new because no existing tars piece plans
or supplies a value function — exactly the faculties §1 says a bare policy lacks.

---

## Roadmap

Ship **micro first** (substrate all present, verifiable immediately); lift to
macro only once the value-net is trusted.

- **M0 — SpecRewardCompiler + Goal-Eval (the reward).** Scope: C1 + C3 (tiered,
  grounded, anti-incest). Delivers: FR-1,3,4; NFR-3,4. Depends: Doc 26 eval.
  Verified by: E2E-6, E2E-7. *Risk-up-front: if the reward isn't grounded &
  deterministic, nothing above it is safe — build and trust it first.*
- **M1 — micro-MCTS / get-unstuck (CUJ-3).** Scope: C4 (SearchTree/PUCT/backprop)
  + C5 minimal (fan-out K trajectory branches in worktrees, score, keep best,
  abandon rest) + `tars develop --unstuck`. Delivers: FR-5,7,8; F5. Depends: M0.
  Verified by: E2E-3, E2E-5. The LATS core; cheapest place MCTS beats greedy.
- **M2 — full autonomous loop (CUJ-1,2).** Scope: C2 (Planner candidate fan-out +
  Reflexion intake) + C5 full (yes-man + escalation gate + `tars develop`).
  Delivers: FR-2,6; NFR-1,2,5; F4,F6,F8. Depends: M1. Verified by: E2E-1,2.
  Dogfood gate: re-derive Doc 26 `M2''` with zero human prompts (§10).
- **M3 — macro / design-selection + value-net (CUJ-4).** Scope: 1-ply judge-panel
  over design candidates, then LLM value-estimator to amortize real rollouts
  (AlphaZero-style). Delivers: CUJ-4. Depends: M2 + a trusted Goal-Eval. Verified
  by: E2E-4. *Deferred last: needs a value net good enough that few real rollouts
  suffice — the highest-uncertainty piece, so it goes last, not first.*
