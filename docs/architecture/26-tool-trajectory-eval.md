# Design — Tool-Trajectory A/B Evaluation

> Status: design, 2026-06-11. Author: Leo Cao. Implements: extends
> [Doc 16 Evaluation Framework](./16-evaluation-framework.md),
> [Doc 18 Agent Testing](./18-agent-testing.md) §2/§4 (behavioral diff,
> McNemar), and [Doc 25 Agent/DAG Testing](./25-agent-dag-testing.md).
> Reference practice: Google ADK's `tool_trajectory_avg_score`.

## 1. Overview & goal

Add a **tool-trajectory** dimension to tars eval: score *which tools an agent
reaches for* (and in what order) against a reference, or **A/B** two configs by
how their tool choices diverge. ADK's `adk eval` scores an exact-match of the
tool-call sequence against a hand-authored expected trajectory; this brings that
in and extends it with a **no-oracle head-to-head** mode (the Doc 18 behavioral
diff) plus the **McNemar** significance that Doc 18 specced but never instantiated.

**Non-goals.**
- Not scoring tool *output correctness* — only *which tool was selected, with
  what args, in what order*. (Output quality stays in `eval judge` / Doc 16.)
- Not a new runtime. Tools are already selected by the model and surfaced in
  `ChatResponse.tool_calls`; this reads them, it does not change dispatch.
- P1 is **not** multi-call agent-loop trajectory matching — see the grain note
  in §8 and the phasing in the Roadmap. P1 scores the tool *selection* of a
  single completion; the cross-call sequence is P2.
- Not LLM-judged arg matching in P1 (deterministic only); that's P3.

### Grounding correction (why P1 is smaller than first sketched)

The original plan was "extract tool calls from `LlmCallCaptured`." Grounding
refuted it and made P1 *smaller*:
- `LlmCallCaptured` (`crates/tars-runtime/src/event.rs:221`) persists only
  `prompt_summary` / `response_summary` **strings** — no structured tool calls.
- But `run_eval` runs each case as a **single `Pipeline::complete()`**
  (`crates/tars-cli/src/eval.rs`, `run_eval`), and `ChatResponse` already
  carries `tool_calls: Vec<ToolCall>` (`crates/tars-types/src/response.rs:21`,
  `ToolCall { id, name, arguments }` at `crates/tars-types/src/tools.rs:83`).

So P1 reads the **live `ChatResponse`** in the eval loop — **zero event-store
changes** — and only P2 (cross-call sequences) needs to enrich the trajectory
event. The richer the grain, the later the phase.

## 2. Critical User Journeys (CUJs)

- **CUJ-1 — Reference-mode score (ADK parity).** Actor: an eval author. Trigger:
  `tars eval run --corpus tasks/ --check trajectory-match:ordered`, where each
  case dir carries `expected_tools.json`. Steps: run each case → read
  `resp.tool_calls` → score vs the case's expected list → roll up. Success: the
  manifest has a `trajectory-match` check with a per-case pass/fail and an
  aggregate violation rate, same shape as `valid-json`.
- **CUJ-2 — A/B two configs (reference).** Actor: someone swapping model/prompt.
  Trigger: run CUJ-1 for config A and config B, then `tars eval diff runs/A
  runs/B`. Steps: the existing check-delta diff reads each run's
  `trajectory-match` violation rate. Success: a `trajectory-match: 0.10 → 0.22
  (+0.12)` row, telling them B got *worse* at picking the right tools.
- **CUJ-3 — Head-to-head, no oracle (P2).** Actor: someone with no expected
  trajectories. Trigger: `tars eval diff runs/A runs/B --trajectory
  --trajectory-mode ordered`. Steps: pair case_i in A with case_i in B, diff
  their tool sequences directly. Success: "tool-seq divergence 31% (12/39
  cases); McNemar p=0.04" + the list of diverging case ids.
- **CUJ-4 — Multi-call agent trajectory (P2/P3).** Actor: evaluating an agent
  that loops (tool → call → tool). Trigger: same flags, but the case ran through
  an agent loop. Steps: read the **cross-call** tool sequence from the trajectory
  store. Success: the score reflects the whole loop, not one turn.

## 3. Feature list

| Feature | Serves | Notes |
|---|---|---|
| F1 — `trajectory_match` scorer (exact/ordered/set) | CUJ-1,2,3 | pure fn, no I/O, no LLM — unit-testable |
| F2 — `expected_tools.json` corpus field | CUJ-1,2 | per-case reference list |
| F3 — `--check trajectory-match:<mode>` wiring (case-parameterized) | CUJ-1,2 | evaluated in the run loop, not via the global `build_invariant` |
| F4 — manifest carries the tool trajectory + check rollup | CUJ-1,2,3 | reuse `CheckSummary` / `CaseCheckResult` so `eval diff` works unchanged |
| F5 — `eval diff --trajectory` head-to-head axis + McNemar | CUJ-3 | P2 |
| F6 — cross-call sequence extraction (enrich `LlmCallCaptured` w/ tool_calls; or `ToolDispatched` event) | CUJ-4 | P2/P3 |
| F7 — `args` / judge match modes | (strict variants) | P3 |

## 4. Requirements

**Functional**

| # | Requirement | Feature |
|---|---|---|
| FR-1 | A pure scorer maps `(actual: [ToolStep], expected: [ToolStep], mode)` → score ∈ [0,1], for modes `exact`, `ordered`, `set`. | F1 |
| FR-2 | `exact` = ADK semantics: 1.0 iff the name sequences are identical, else 0.0. | F1 |
| FR-3 | `ordered` = LCS(name seq)·2 / (len a + len b) (Dice over the LCS) ∈ [0,1] — partial credit for prefix/substring agreement. | F1 |
| FR-4 | `set` = Jaccard over the tool-name multiset, order-insensitive. | F1 |
| FR-5 | A case dir MAY carry `expected_tools.json` = `["search","fetch",...]` (names) or `[{"name":..,"args":{..}}]`; absent → the case is *skipped* for this check (counted as skipped, never a silent pass). | F2 |
| FR-6 | `--check trajectory-match:<mode>` (default mode `ordered`) is recognized; an unknown mode errors at parse time with the valid set. | F3 |
| FR-7 | Per case, the check passes iff `score >= threshold` (default 1.0 for `exact`, configurable via `trajectory-match:<mode>:<thresh>`). Result recorded as a `CaseCheckResult`; rollup as a `CheckSummary` with `violation_rate = 1 - mean(score>=thresh)`. | F3,F4 |
| FR-8 | The extracted tool sequence is written to the per-case `report.json` (`tool_trajectory: [{name,args_hash}]`) so a later run / diff need not re-run the model. | F4 |
| FR-9 | `tars eval diff` shows the `trajectory-match` rate delta with no code change (it already diffs every `CheckSummary`). | F4 |
| FR-10 | (P2) `eval diff --trajectory --trajectory-mode <m>` pairs cases by id, scores A-vs-B directly, reports divergence rate + diverging ids + McNemar p. | F5 |
| FR-11 | (P2/P3) For agent-loop cases, the sequence spans all LLM calls in the trajectory, read from the event store. | F6 |

**Non-functional** (measurable)

| # | Requirement | Threshold | Feature |
|---|---|---|---|
| NFR-1 | Scorer is allocation-light and O(n·m) for `ordered` (LCS), n,m = seq lengths. | < 50µs/case at n,m ≤ 32 | F1 |
| NFR-2 | P1 adds zero live LLM calls beyond what `eval run` already makes. | 0 extra calls | F3 |
| NFR-3 | P1 adds zero new persisted-event schema fields. | 0 event changes | F3,F4 |
| NFR-4 | McNemar **reuses** the repo's existing `tars_types::mcnemar` (χ² with continuity correction, significant at χ²>3.841/α=0.05) so the trajectory A/B uses the *same* statistic as the judge A/B. Exact-binomial small-n refinement deferred. | consistent w/ judge A/B | F5 |

## 5. Infra

| Need | Exists? | Where / new |
|---|---|---|
| Corpus loader | ✅ | `eval.rs::load_corpus` — add one optional-file read |
| Per-case + aggregate check rollup | ✅ | `EvalRunManifest` / `CheckSummary` / `CaseCheckResult` (`eval.rs:237+`) |
| A/B diff over check rates | ✅ | `eval.rs::run_diff` |
| Live `ChatResponse.tool_calls` | ✅ | `tars-types/src/response.rs:21` |
| Pure scorer module | ➕ new | `tars-runtime/src/trajectory_match.rs` |
| Head-to-head trajectory axis + McNemar | ➕ new (P2) | `eval.rs` diff + a `mcnemar` fn |
| Cross-call sequence | ➕ new (P2/P3) | enrich `LlmCallCaptured` or add `ToolDispatched` |

## 6. Components

### C1 — `trajectory_match` scorer (new, P1)
- **Responsibility:** pure scoring of two tool-step sequences. No I/O, no LLM.
- **Reuses:** `tars-types::ToolCall` (`tars-types/src/tools.rs:83`) for the input
  shape; mirrors the `CheckResult` pass/fail vocabulary of
  `tars-runtime/src/check.rs:33`.
- **New:** the module + `ToolStep` view + the three modes.
- **Interface:**
  ```rust
  // crates/tars-runtime/src/trajectory_match.rs
  pub struct ToolStep { pub name: String, pub args: serde_json::Value }

  #[derive(Clone, Copy)]
  pub enum MatchMode { Exact, Ordered, Set }

  /// score ∈ [0.0, 1.0]; 1.0 = perfect agreement under `mode`.
  pub fn score(actual: &[ToolStep], expected: &[ToolStep], mode: MatchMode) -> f64;

  /// Names only (P1). `from_tool_calls` adapts a response's calls.
  pub fn from_tool_calls(calls: &[tars_types::ToolCall]) -> Vec<ToolStep>;
  ```

### C2 — `TrajectorySpec` (the case-parameterized check, new, P1)
- **Responsibility:** parse `--check trajectory-match:<mode>[:<thresh>]`, hold
  `(mode, threshold)`, and evaluate a case given its `expected_tools` + the live
  response. *Not* an `Invariant` — it needs per-case data the global
  `Invariant::check(req,resp)` can't carry (see §8).
- **Reuses:** `eval.rs::build_invariant` (`crates/tars-cli/src/eval.rs:214`) for
  the spec-parsing pattern; emits `CaseCheckResult` (`eval.rs`) + `CheckSummary`.
- **New:** the parse + the per-case eval call site in `run_eval`'s case loop.
- **Interface:**
  ```rust
  pub struct TrajectorySpec { mode: MatchMode, threshold: f64 }
  impl TrajectorySpec {
      pub fn parse(spec: &str) -> anyhow::Result<Option<Self>>; // None if spec isn't trajectory-match:*
      pub fn eval_case(&self, resp_calls: &[ToolCall], expected: Option<&[ToolStep]>)
          -> Option<CaseCheckResult>; // None = skipped (no expected)
  }
  ```

### C3 — corpus `expected_tools` (new field, P1)
- **Responsibility:** carry the reference tool list per case.
- **Reuses:** `eval.rs::load_corpus` + the `Case` struct + `read_optional_text`.
- **New:** `Case.expected_tools: Option<Vec<ToolStep>>`, read from
  `expected_tools.json`.

### C4 — manifest extension (P1)
- **Responsibility:** persist the extracted sequence per case.
- **Reuses:** `EvalCaseReport` (`eval.rs`), already serialized to `report.json`.
- **New:** `EvalCaseReport.tool_trajectory: Vec<ToolStepRef>` (`#[serde(default)]`,
  back-compatible).

### C5 — head-to-head diff axis + McNemar (M1, shipped)
- **Responsibility:** `eval diff --trajectory`: pair cases by id; (a) **no-oracle
  divergence** — score A-vs-B persisted `tool_trajectory` with `score_names`,
  report divergence rate + diverging ids; (b) **McNemar** over any shared
  `trajectory-match*` check's per-case pass/fail (the oracle is baked into each
  run's check result, so no expected_tools needed at diff time).
- **Reuses:** `eval.rs::run_diff`, `tars_types::mcnemar` (`tars-types/src/judge.rs:204`,
  the same fn the judge A/B uses) + `McNemarResult`, `trajectory_match::score_names`
  (new in C1), C4's persisted `tool_trajectory`.
- **New:** `--trajectory` / `--trajectory-mode` flags; `compute_traj_diff` +
  `case_check_passmap` in `eval.rs`. **No new McNemar** — reused.

## 7. Interfaces with other modules

| Direction | Module | Symbol / signature | Purpose |
|---|---|---|---|
| calls → | `tars-types` | `ChatResponse.tool_calls: Vec<ToolCall>` (`response.rs:21`) | read selected tools |
| calls → | `tars-runtime` | `trajectory_match::score(...)` (new C1) | score |
| ← called by | `tars-cli::eval` | `run_eval` case loop invokes C2 `eval_case` | per-case scoring |
| ← called by | `tars-cli::eval` | `run_diff` reads `CheckSummary{name:"trajectory-match", violation_rate}` | A/B (no change for reference mode) |
| calls → (P2) | `tars-storage` | trajectory `EventStore` read | cross-call sequence |

## 8. Main algorithms

### `ordered` scorer (LCS / Dice)
```
1. a = names(actual); b = names(expected)
2. if mode == Exact:  return if a == b { 1.0 } else { 0.0 }
3. if mode == Set:    return |multiset(a) ∩ multiset(b)| / |multiset(a) ∪ multiset(b)|
4. if mode == Ordered:
     l = LCS_length(a, b)                 # classic O(n·m) DP
     return if a.is_empty() && b.is_empty() { 1.0 } else { 2*l / (len(a)+len(b)) }
```
Invariants: score ∈ [0,1]; empty-vs-empty = 1.0; the function is total (never
panics on empty/None). Edge cases: both empty (perfect), one empty (0 unless
other empty), duplicate tool names (multiset semantics in `set`, positional in
`ordered`).

### Grain note (why P1 is single-completion)
`run_eval` issues one `Pipeline::complete()` per case, so `resp.tool_calls` is
*one turn's* requested tools — a real and useful signal ("did A vs B pick the
right tool for this prompt"), but **not** a multi-call agent loop. The cross-call
sequence (CUJ-4) requires either (a) eval driving a `Session`/`run_task` loop, or
(b) reading the trajectory store — both P2, because both need the tool calls
persisted, which today they are not (`LlmCallCaptured` has no `tool_calls`).

### McNemar (M1, shipped — reuses `tars_types::mcnemar`)
Two distinct signals, deliberately separated:
```
divergence (no oracle):  per paired case, differ := score_names(A_traj, B_traj, mode) < 1.0
                         divergence_rate = #differ / #paired
McNemar (needs the check): pair each run's `trajectory-match*` per-case pass/fail
                         b = #(A pass, B fail);  c = #(A fail, B pass)
                         χ² = (|b-c|-1)² / (b+c)   [tars_types::mcnemar, continuity-corrected]
                         significant at χ²>3.841 (α=0.05) / >6.635 (α=0.01)
```
The divergence number needs **no oracle** (it only asks "did A and B pick
differently"). McNemar needs a per-run notion of *correct*, which the
`trajectory-match` check already supplies — so McNemar runs only when both runs
ran that check. Invariant: only discordant pairs carry signal. Edge: b+c==0 →
`chi_squared = None` (runs agree; nothing to test).

## 9. Integration / E2E tests

| Test | CUJ | Setup → Action → Assertion |
|---|---|---|
| E2E-1 | CUJ-1 | corpus with 1 case + `expected_tools.json=["search"]`; mock provider returns a response whose `tool_calls=[search]` → `eval run --check trajectory-match:exact` → manifest `checks["trajectory-match"].violation_rate == 0.0`, case report `tool_trajectory==["search"]`. |
| E2E-2 | CUJ-1 | same, but mock returns `tool_calls=[fetch]` → violation_rate == 1.0, `CaseCheckResult::Failed{reason}` names expected-vs-actual. |
| E2E-3 | CUJ-2 | two runs A (rate 0.0) and B (rate 0.5) on disk → `eval diff A B` → output row `trajectory-match 0.000 → 0.500 (+0.500)`. |
| E2E-4 (unit) | F1 | `score` table test: exact/ordered/set on identical, reordered, subset, disjoint, empty pairs → expected fractions. |
| E2E-5 | CUJ-1 | case with **no** `expected_tools.json` → check is *skipped*, `evaluated` excludes it, no false pass. |
| E2E-6 (P2) | CUJ-3 | A and B runs with per-case `tool_trajectory` persisted → `eval diff A B --trajectory` → divergence count + McNemar p present and correct on a hand-checked 2×2. |

## 10. Success criteria
- [ ] FR-1…FR-9 met (P1); FR-10…FR-11 (P2/P3).
- [ ] NFR-1…NFR-3 hold (scorer micro-bench < 50µs; 0 extra LLM calls; 0 event-schema changes in P1).
- [ ] E2E-1…E2E-5 pass (P1); E2E-6 (P2).
- [ ] `eval diff` shows the trajectory-match delta with no diff-code change for reference mode.

## 11. Performance considerations
Hot path: the scorer, once per case, off the network path. `ordered` is O(n·m)
DP with n,m = tool counts (typically < 10), negligible vs. the LLM call. Persist
the extracted sequence (FR-8) so re-diffing N runs is O(read), no re-inference.
Measure: a `criterion`/unit micro-bench asserting NFR-1.

## 12. Reliability considerations
Failure modes: malformed `expected_tools.json` → hard parse error at corpus
load (fail-closed, names the file), never a silent skip. A case with no expected
tools is *explicitly skipped* (FR-5), distinguished from a pass in `evaluated`.
Scorer is total (no panics on empty). Idempotent: same inputs → same score;
persisted trajectory makes diff reproducible (ties into Doc 25 golden replay).

## 13. Security considerations
Trust boundary: `expected_tools.json` and tool args are author/model data, not
executed — only compared. `args_hash` (sha256 of canonical-JSON args) is stored
rather than raw args to avoid leaking secrets that may appear in tool arguments
into committed `report.json`. No authz/secrets surface beyond what `eval run`
already has.

## 14. Abstraction & reuse
Approach: P1 is a *case-parameterized check* layered onto the existing
run→manifest→diff pipeline; the scorer is a standalone pure module so it's
reusable by the P2 head-to-head path and any future trajectory consumer
(Doc 24 replay, Doc 25 golden). We deliberately do **not** force trajectory-match
into the `Invariant` trait — it needs per-case reference data the trait's
`(req,resp)` signature can't carry; pretending otherwise would be the wrong
abstraction.

**Reuse map** (existing code to call):
| Symbol | Location | How we use it |
|---|---|---|
| `ChatResponse.tool_calls` | `tars-types/src/response.rs:21` | the live tool selection (P1 source) |
| `ToolCall { id, name, arguments }` | `tars-types/src/tools.rs:83` | scorer input shape |
| `build_invariant` | `tars-cli/src/eval.rs:214` | spec-parse pattern to mirror for `trajectory-match:*` |
| `EvalRunManifest` / `CheckSummary` | `tars-cli/src/eval.rs:237` | aggregate rollup; `eval diff` reads it unchanged |
| `EvalCaseReport` / `CaseCheckResult` | `tars-cli/src/eval.rs` | per-case result + persisted trajectory |
| `run_diff` | `tars-cli/src/eval.rs` (`EvalDiffArgs` `:170`) | reference-mode A/B, free; P2 adds `--trajectory` |
| `load_corpus` / `Case` | `tars-cli/src/eval.rs` | add `expected_tools` read |
| `CheckResult` vocabulary | `tars-runtime/src/check.rs:33` | pass/fail shape the scorer result maps to |
| `tars_types::mcnemar` / `McNemarResult` | `tars-types/src/judge.rs:204` | the A/B significance test — reused verbatim (same fn the judge A/B uses) |
| `trajectory_match::score_names` | `tars-runtime/src/trajectory_match.rs` | head-to-head similarity over persisted name sequences |
| `LlmCallCaptured` | `tars-runtime/src/event.rs:221` | M2 enrichment target (add `tool_calls`) |

**New abstractions:** `ToolStep` + `MatchMode` + `score()` (justified: the
scorer is pure and reused across 3 phases and the Doc 24/25 replay paths);
`TrajectorySpec` (justified: case-parameterized checks are a category the
`Invariant` trait can't express, and there will be more of them — e.g. expected
final-state checks).

## Roadmap

- **M0 — scorer + reference mode (P1).** Scope: C1 `trajectory_match` module
  (F1), C2 `TrajectorySpec` parse + case-loop eval (F3), C3 `expected_tools`
  (F2), C4 persist trajectory (F4). Delivers: FR-1…FR-9. Depends: —. Verified by:
  E2E-1,2,3,4,5. Risk-up-front: the scorer DP + the case-parameterized wiring
  (the one place that doesn't fit the global `Invariant` model) land first.
- **M1 — head-to-head axis + McNemar (P2). ✅ shipped.** Scope: C5 (F5),
  reusing `tars_types::mcnemar`. Delivers: FR-10. Depends: M0's persisted
  `tool_trajectory`. Verified by: E2E-6 (`compute_traj_diff_*` tests). Note: no
  new McNemar written — reused the judge A/B's `tars_types::mcnemar` so both A/B
  axes share one statistic.
- **M2 — cross-call capture + extraction (P2). ✅ shipped (foundation).** Scope:
  `LlmCallCaptured` gains `tool_calls: Vec<String>` (additive, `#[serde(default)]`,
  `system_prompt_hash` precedent); the worker accumulates tool names across its
  loop and surfaces them on `AgentStepResult.tool_calls` (`agent.rs`), which the
  executor stamps onto the event (`runtime.rs`); `tool_sequence(events)` in
  `event.rs` concatenates them in step order. So any `run_task`/Session
  trajectory now records its cross-call tool use, queryable + scorable with
  `trajectory_match`. Verified by `tool_sequence_concats_*` + no worker
  regression. **Remaining for full CUJ-4:** `eval run` driving an actual agent
  loop (so eval cases *produce* multi-call trajectories) and scoring a recorded
  trajectory against `expected_tools` — that's a larger "eval runs tool-using
  agents" surface, split below. Depends: M0.
- **M2' — score a recorded trajectory. ✅ shipped.** `tars trajectory score <id>
  --expected <names|@file.json> [--mode] [--threshold] [--json]` replays a
  trajectory, extracts its cross-call `tool_sequence` (M2), scores it with
  `score_names` (M1), and exits non-zero below threshold (CI gate). This closes
  the loop for any agent run (`tars run-task` etc.) without eval changes.
  Verified by `score_passes_on_matching_cross_call_sequence_*` +
  `parse_expected_*` in `trajectory.rs`.
- **M2'' — eval drives a tool-using agent loop.** Remaining half of CUJ-4:
  `eval run` runs each case through a `WorkerAgent::with_tools` in a sandboxed
  cwd (read-only tools only — no `bash`/write side effects from corpus cases),
  captures the trajectory, and feeds its `tool_sequence` to the trajectory-match
  check. The heavy, security-sensitive part; deferred. Depends: M2, M2'.
- **M3 — args / judge modes (P3).** Scope: F7 (`args` exact + LLM-judged arg
  equivalence) + first-class `ToolDispatched` event (actual dispatch + result
  class, not just requested). Delivers: stricter matching. Depends: M2.
