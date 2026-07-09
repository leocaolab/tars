# Doc 25 — Agent / DAG / Workflow E2E Testing: Mock & Golden LLM

> Scope: how a consumer writes a **deterministic, offline, zero-LLM-cost**
> end-to-end test of an agent, a multi-step DAG (`run_task`), or a workflow —
> driving real orchestration code while the model is either **scripted** (hand-
> authored canned responses) or **golden** (real recorded responses, replayed by
> request fingerprint). The harness that makes this *easy* instead of a
> per-test hand-roll.
>
> Upstream (reference practice): A.R.C.'s golden-replay suite
> (`docs/internal/arc-multilang-golden-e2e-design.md`,
> `arc-regression-tests-design.md`) — the most mature consumer pattern; every
> mechanism below has an arc analog cited inline.
>
> Downstream (dependencies): Doc 17 Pipeline Event Store (`request_fingerprint`
> + `bodies.db` CAS — the golden substrate already exists), Doc 18 Agent Testing
> (the behavioral dimensions a golden is checked on), Doc 04 Agent Runtime
> (`run_task` / Worker / Critic), Doc 19/24 (replay reuse).

---

## 1. The problem: e2e testing an agent pipeline is too hard today

tars has the pieces but not the ergonomics. Two grades of test exist:

**Single-call, single-response — easy.** `MockProvider::new("mock",
CannedResponse::text("bug"))` (`tars-provider/src/backends/mock.rs:70`) gives an
agent one canned answer. `examples/testing/main.rs` shows the blessed pattern:
build the agent's `LlmService` over a swappable `Arc<dyn LlmProvider>`, inject
the mock provider. This works and is documented.

**Multi-call DAG / workflow — hand-rolled, every time.** The moment a test
drives `run_task` (Orchestrator + N steps × {worker, critic, refine}) each call
needs a *different* response shape (a `Plan`, a `WorkerResult`, a `Verdict`).
`MockProvider` holds one. So `tars-runtime/tests/run_task.rs:60` open-codes a
local `QueuedProvider` whose own comment is the indictment:

> *"The default MockProvider only holds one canned response; run_task fires off
> many LLM calls (Orchestrator + 3 per step) and each needs a different shape …
> This local helper pops the next text off a FIFO per call."*

`MockProvider::with_responses` (`mock.rs:88`) was added to cover the simplest
case, but every non-trivial DAG test still re-rolls the queue. Three structural
problems with the FIFO approach:

1. **Order-coupled and parallel-fragile.** A FIFO assumes a fixed call order.
   A DAG with parallel workers (Doc 19's whole point) has *no* deterministic
   call order — worker A's call and worker B's call race. A FIFO test either
   pins concurrency to 1 (not testing the real thing) or flakes.
2. **Hand-scripted JSON drifts from real model shapes.** The same defect arc
   called out: *"the mock replies are hand-scripted strings, so they don't
   reflect real model shapes — the messy outputs that caused this week's
   bugs."* `issue_id` collisions, empty messages, pagination, weird `rule_id`s
   — a human writing the mock writes the *clean* shape and misses exactly the
   one that breaks the pipeline.
3. **No coverage guarantee.** "Do we have an e2e test for the resume-after-
   crash workflow?" is answered by hoping someone wrote one.

The fix is **two providers and a registry**, none of which a consumer should
have to write: a **matcher-scripted** provider (replaces the hand-rolled FIFO),
a **golden record/replay** provider (real responses, deterministic), and a
**corpus/coverage registry** (Doc 18's spine, made a checked invariant).

---

## 2. Design goals

| Goal | Description |
|---|---|
| **One injection seam, any model** | The agent/DAG under test holds a concrete `LlmService`; the swap point beneath it is its `Arc<dyn LlmProvider>`. A test plugs in a scripted, golden-replay, or (rarely) live provider — nothing else changes. The seam already exists (`examples/testing/main.rs`); the harness supplies the things you plug into it. |
| **Match, don't count** | A scripted response is selected by *which call this is* (agent id, model, a predicate on the request), not by FIFO position — so parallel DAGs are deterministic without serializing them. |
| **Golden = recorded real responses, replayed by fingerprint** | The "golden LLM" is real model output captured once and replayed offline. Reuse Doc 17's `request_fingerprint` as the key and `bodies.db` as the store — **no new golden format**, exactly as arc reuses its critic cache. |
| **Fail-closed replay** | In golden-replay mode a cache miss is a hard test failure ("golden absent, re-record"), never a silent live call. |
| **Curation gate, not `cp`** | A recorded response becomes golden only by passing a gate (non-degenerate, recall=1 on planted oracle, stability across K records, human approval) — freezing a *bad* response enforces wrong behavior forever. |
| **Coverage is a checked invariant** | A workflow/CUJ registry + a meta-test: every declared journey has a golden; no orphan test, no uncovered journey. |
| **Behavioral asserts, not text-equality** | A golden mismatch is reported as *which field of which step drifted* (Doc 18 §2 behavioral diff), with volatile fields (ids, timestamps) normalized. |

**Anti-goals**

- Not model-quality eval. Frozen responses test the **pipeline** (parse →
  validate → persist → entity → render), never the model. Quality is Doc 16/18's
  live dogfood. A golden test passing means "the orchestration still behaves";
  it says nothing about whether the model is good.
- Not a new mock for every provider. The harness sits at the provider seam,
  beneath the `LlmService` middleware — one harness, every backend.
- Not deleting `MockProvider`. The matcher-scripted provider is `MockProvider`
  leveled up; the single-response form stays for unit tests.

---

## 3. The three modes (pick by what you're testing)

```
            ┌──────────────────────────────────────────────────────────┐
agent /     │  Arc<dyn LlmProvider>  ◀── the one injection seam          │
DAG  ──────▶│  (the agent's LlmService wraps whichever you plug in)       │
under test  │     ├─ ScriptedProvider   (mode A: matcher → canned)       │
            │     ├─ GoldenProvider     (mode B: fingerprint → recorded) │
            │     └─ real LlmService     (mode C: record / live)          │
            └──────────────────────────────────────────────────────────┘
```

| Mode | Provider | When | Determinism source |
|---|---|---|---|
| **A — Scripted** | `ScriptedProvider` | Logic/control-flow tests: does the DAG branch, retry, skip, deadlock-to-human correctly? You're asserting *orchestration*, and a terse hand-authored shape is enough. | You author the responses. |
| **B — Golden replay** | `GoldenProvider` | Pipeline-fidelity tests: does parse→persist→render still produce the same artifact given a *realistic* model response? | Recorded real bytes, keyed by `request_fingerprint`. |
| **C — Record / live** | real `LlmService` + `EventEmitterMiddleware` | One-time golden capture, or a gated live dogfood. | The real model (the only mode that calls it). |

A is the workhorse for **behavior**; B is the workhorse for **regression
fidelity**; C runs rarely and on purpose. arc uses exactly this split: `cuj.rs`
(72 hand-scripted = mode A) for journey logic, golden-from-cache (mode B) for
pipeline regression.

---

## 4. Mode A — `ScriptedProvider`: match, don't count

A first-class provider (promote `run_task.rs`'s local `QueuedProvider` into
`tars-provider`, generalized from FIFO to matcher dispatch):

```rust
// tars-provider/src/backends/scripted.rs (new)
pub struct ScriptedProvider { rules: Vec<Rule>, history: Mutex<Vec<ChatRequest>>, /* … */ }

pub struct Rule {
    /// Selects which calls this rule answers. First matching rule wins;
    /// `times` bounds reuse (None = unbounded).
    when: Matcher,
    then: CannedResponse,   // reuse the existing enum (Text | Sequence | Error)
    times: Option<u32>,
}

pub enum Matcher {
    Any,
    Agent(AgentId),                 // "the Critic call" — not "the 3rd call"
    Model(String),
    SystemContains(String),
    UserMatches(Regex),
    RequestSchema(SchemaName),      // "the call that asked for a Plan"
    And(Vec<Matcher>), Or(Vec<Matcher>),
    Predicate(Arc<dyn Fn(&ChatRequest) -> bool + Send + Sync>),
}
```

Builder, so a DAG test reads as a script:

```rust
let llm = ScriptedProvider::builder()
    .on(Matcher::RequestSchema("Plan"),   plan_json(["scan", "fix"]))
    .on(Matcher::Agent("worker:scan"),    worker_result("found 2"))
    .on(Matcher::Agent("critic"),         verdict("approve"))
    .otherwise(CannedResponse::Error("unexpected call".into()))  // fail loud
    .build();
run_task(rt, LlmService::of(llm, "mock-model"), task).await?;
```

Why matcher over FIFO (the §1.1 fragility, fixed):

- **Parallel-safe.** Worker A and worker B race; each matches its *own* rule
  regardless of which hit the provider first. No `--test-threads=1`, no flake.
- **Reads as intent.** `Agent("critic")` survives adding a step before it; the
  FIFO's "3rd response" silently shifts to the wrong call.
- **Fails loud.** `.otherwise(Error)` turns "test fed the wrong number of
  responses" (today: a hang or a confusing dry-queue panic) into a precise
  "unexpected call: agent=worker:fix" at the offending call.

`history()` (already on `MockProvider`) lets the test assert *what was asked*:
"the fix worker's prompt contained the scan's findings" — i.e. that the
blackboard hand-off (Doc 19) actually flowed.

---

## 5. Mode B — `GoldenProvider`: the substrate is already built

The insight that makes golden-LLM cheap in tars: **Doc 17 already defines the
key and the store.** `request_fingerprint` (Doc 17 §Q3 = sha256 of model +
messages + tools + temperature + max_tokens + response_schema, tenant-agnostic)
is exactly the golden lookup key. `bodies.db` (CAS by `ContentRef`) is exactly
the golden store. `EventEmitterMiddleware` already writes both on every call.

So golden = **a provider that, instead of calling the model, looks up the
response body whose request fingerprint matches the incoming request, and
fails closed on miss.** This is the precise analog of arc's *"the golden store
IS arc's existing critic cache … recording = running review live once (cache
fills); replaying = shipping that cache so review cache-hits with no network. No
new golden format — the cache is the format."*

```rust
// tars-provider/src/backends/golden.rs (new)
pub struct GoldenProvider {
    fixtures: GoldenStore,   // request_fingerprint -> response body (from bodies.db / a committed dir)
    on_miss: MissPolicy,     // FailClosed (default in tests) | PassThrough(real) (record mode)
}
```

### 5.1 Record → replay flow (CUJ-2 / CUJ-1 in arc terms)

```
RECORD (mode C, once, live):
  run the DAG with the real Pipeline + EventEmitterMiddleware + tag "golden:<cuj>"
  → every LlmCallFinished lands (request_fingerprint, response_ref→bytes) in the events DB
  → `tars golden export --tag golden:<cuj> --out tests/golden/<cuj>/`
     freezes {fingerprint → body} as a committed fixture

REPLAY (mode B, every CI run, offline):
  GoldenProvider::from_dir("tests/golden/<cuj>/")  with MissPolicy::FailClosed
  → each call's request_fingerprint hits the frozen body; 0 live calls
  → a miss = hard failure "golden absent for <fingerprint>, re-record"
```

`tars golden export` / `import` are thin CLI verbs over the Doc 17 store — no
new persistence. Replay determinism is total: same fingerprint → same bytes →
the pipeline is the only free variable, so a failure means **the pipeline
changed**, never "the model said something different today" (arc CUJ-3).

### 5.2 Fingerprint stability — the one real constraint

A golden keyed by `request_fingerprint` is only stable if the request is
reproducible. If the prompt embeds a timestamp, a temp dir path, or a random
id, the fingerprint changes every run and every replay misses. This is a
**feature** surfaced as a constraint: the harness's fail-closed miss *forces*
prompt determinism, catching "we leak `now()` into the prompt" the first time.
For genuinely variable inputs the matcher of mode A (predicate on the stable
part) is the right tool, not golden.

> Cross-link: this is the same recorded-body replay Doc 24 §5.4 (`tars
> blackboard replay`) and Doc 19 §8 use. One capture mechanism (Doc 17), three
> consumers (golden test, blackboard replay, eval offline-replay Doc 16 §5).

---

## 6. The curation gate — a recorded response is a *candidate*, not a golden

Freezing a degenerate response (truncated, missed the planted bug,
hallucinated) bakes wrong behavior into the regression net forever. So
record → golden passes a gate, lifted directly from arc
(`arc-regression-tests-design.md` Q1, `tests/support/golden_support.rs::
gate_candidate`):

| Gate | Check | Rejects |
|---|---|---|
| **Non-degenerate** | schema-valid, `stop_reason = EndTurn` (not `MaxTokens`), no >200-char repeat run | truncation / loops |
| **Recall = 1** | the fixture plants known oracle facts (a review CUJ: planted issues `{rule, line}`; a tool CUJ: the tool that must be called); the recorded response must hit all of them | a golden that misses the bug |
| **Precision** | extra findings ≤ `allow_extra` | a noisy golden |
| **Stability** | record K=3; finding-set Jaccard ≥ 0.8; freeze the modal one. Ambiguity ⇒ tighten the fixture, don't lower the bar | a flaky prompt frozen at one of its faces |
| **Human approval** | the golden diff is read + approved in the PR | a silently-wrong freeze |

This is where the "golden LLM" earns trust: the oracle is the **fixture's
declared expectation** (Doc 18 §4.5 gold standard), not the model's say-so. The
gate runs in mode C at record time; replay (mode B) never re-checks it (the
golden is already blessed).

---

## 7. Coverage as a checked invariant — the registry + meta-test

Doc 18 §6's corpus, made fail-closed (arc Q2):

```toml
# tests/workflows.toml — the enumerated set of journeys that must never regress
[[workflow]]
id     = "run_task.happy_path"
kind   = "dag"
golden = "tests/golden/run_task.happy_path/"

[[workflow]]
id     = "run_task.worker_crash_resume"
kind   = "dag"
golden = "tests/golden/run_task.worker_crash_resume/"
```

A meta-test (`workflow_coverage_is_complete`) fails if:

- a registry row has no golden fixture dir (a declared journey with no test), **or**
- a `#[test]` e2e journey exists with no registry row (an orphan test).

Both directions. You cannot land a new workflow without its golden, and you
cannot delete a golden out from under a registered journey. Each `id` traces to
a CUJ in the relevant design doc, so "every designed CUJ has an e2e test" is the
same join (arc's traceability rule).

---

## 8. The assertion: behavioral diff, not byte-equality (Doc 18 §2)

Two outputs are compared on **test dimensions**, with volatile fields
normalized — never raw text equality (arc CUJ-3 / Q4 "volatile-aware structured
diff"):

- **Normalize volatiles** before compare: `run_id`, `trajectory_id`, wall-clock
  timestamps, sqlite `rowid`, worktree paths → canonical placeholders. (arc
  FR-3: "byte-equal modulo run_id, timestamps, rowid → normalized".)
- **Diff structure, report the field.** On mismatch, say *"step `fix`:
  finding F-2 `commit_sha` golden=`ece08c8` actual=`<base HEAD>`"* — not "not
  equal" (arc F5). The reuse with Doc 24 is exact: a golden DAG test's failure
  is read with the same blackboard-delta vocabulary as `tars blackboard replay`.
- **Layer in Doc 18 dimensions** for free, no oracle: closed-set membership
  (every produced `rule_id` ∈ the rubric), determinism (replay twice → byte-
  identical post-normalization), schema-conformance. These catch a regression
  class even where no golden is pinned.

---

## 9. What tars supplies vs. what the consumer binds

| tars supplies | consumer (arc, or any DAG) binds |
|---|---|
| `ScriptedProvider` + `Matcher` + builder (`tars-provider`) | the matcher rules for its agents |
| `GoldenProvider` + `GoldenStore` + `MissPolicy` (`tars-provider`) | `tests/golden/<cuj>/` fixtures (recorded once) |
| `tars golden export/import` over the Doc 17 store | the record-time tag convention |
| `gate_candidate` (the curation gate, generalized from arc) | the fixture's planted oracle (`expected.toml`) |
| `workflow_coverage_is_complete` meta-test scaffold | `tests/workflows.toml` rows |
| volatile-normalizer + structured-diff reporter | the volatile field list for its schema |

### Reuse map

| Symbol | `file:line` | Role |
|---|---|---|
| `MockProvider` / `CannedResponse` / `with_responses` | `tars-provider/src/backends/mock.rs:70,88` | the single-response base `ScriptedProvider` generalizes |
| `QueuedProvider` (local) | `tars-runtime/tests/run_task.rs:60` | the hand-roll this doc promotes + replaces with matchers |
| the `Arc<dyn LlmProvider>` seam | `examples/examples/testing/main.rs:58` | the one injection point all three modes share |
| `request_fingerprint` | Doc 17 / `tars-types/pipeline_events.rs` | the golden lookup key (no new key) |
| `bodies.db` CAS / `ContentRef` | `tars-storage/body_store.rs` | the golden store (no new store) |
| `EventEmitterMiddleware` | `tars-pipeline/event_emitter.rs` | record-mode capture (no new capture) |
| Doc 18 `Invariant` / `MetamorphicRelation` | Doc 18 §4 | the behavioral-diff dimensions §8 layers in |
| arc `gate_candidate` / `cuj.rs` / golden-from-cache | arc `tests/support/golden_support.rs`, `cuj.rs` | the proven reference the gate + registry copy |

---

## 10. E2E verification (the harness, tested on itself)

1. **Scripted parallel determinism:** a 2-worker DAG with matcher rules passes
   100× with `--test-threads` unset (no order coupling); swap to a FIFO and it
   flakes — proving the matcher fix.
2. **Golden round-trip:** record `run_task.happy_path` live once (mode C),
   `tars golden export`, replay (mode B) → 0 live calls, artifacts match.
3. **Fail-closed:** delete one fixture body; replay → hard error naming the
   missing `request_fingerprint`, never a network call.
4. **Gate rejects junk:** feed `gate_candidate` a truncated (`MaxTokens`)
   recording → rejected; a recall<1 recording → rejected.
5. **Coverage meta-test bites:** add a `#[test]` workflow with no registry row →
   `workflow_coverage_is_complete` fails; add a registry row with no golden dir →
   fails.
6. **Behavioral diff pinpoints:** plant the Doc 19 deferral regression (scan
   writes the row, not the `found` event); the golden DAG test fails with the
   per-field delta (step `scan`, `finding_events +0 expected +N`), the same
   message Doc 24 §8 assertion #4 produces.

Assertions #5 and #6 are the keystones: coverage cannot silently rot, and the
exact retro-class regression fails the build with a field-level diagnosis.

---

## 11. Roadmap (phased)

**Phase 1 — `ScriptedProvider` (kills the hand-roll).**
1. Promote `QueuedProvider` → `tars-provider::ScriptedProvider` with `Matcher` +
   builder + `.otherwise(fail-loud)`.
2. Port `run_task.rs` and the other `tars-runtime/tests/*` DAG tests onto it;
   delete the local copies.

**Phase 2 — `GoldenProvider` + CLI (the golden substrate).**
3. `GoldenProvider` + `GoldenStore` reading the Doc 17 `bodies.db` by
   `request_fingerprint`; `MissPolicy::FailClosed`.
4. `tars golden export --tag … --out …` / `tars golden import` over the events DB.
5. One golden DAG test end-to-end (record → freeze → replay).

**Phase 3 — gate + coverage (the regression net).**
6. `gate_candidate` (generalized from arc) + a fixture `expected.toml` oracle.
7. `tests/workflows.toml` + `workflow_coverage_is_complete` meta-test scaffold.
8. volatile-normalizer + structured-diff reporter (shared with Doc 24's diff).

**Phase 4 — behavioral dimensions (Doc 18 layered in).**
9. wire `Invariant` / determinism / closed-set checks into the golden compare so
   un-pinned regressions still fail.

Phase 1 alone removes the per-test `QueuedProvider` tax and makes parallel DAG
tests deterministic. Phase 2 turns "test the pipeline with realistic model
output, offline, free" from an arc-only capability into a tars primitive any
consumer (and tars' own `run_task` suite) gets.

---

## 12. Relationship to other docs

- **Doc 17 (Pipeline Event Store)** is the golden substrate — `request_finger
  print` is the key, `bodies.db` the store, `EventEmitterMiddleware` the
  recorder. This doc adds *replay-by-fingerprint*, no new persistence.
- **Doc 18 (Agent Testing)** supplies the *oracle theory* (golden = approved
  reference; behavioral diff, not text) and the dimensions §8 layers in. This
  doc is its concrete e2e harness.
- **Doc 24 (Pipeline Investigation)** shares the recorded-body replay (§5.4) and
  the structured-delta vocabulary (§8): a *failing golden test* and a *blackboard
  replay diff* read identically — one is CI-time, one is debug-time, same engine.
- **Doc 19 (Blackboard Pipeline)** supplies the §10/#6 keystone regression and
  the parallel-DAG case that breaks FIFO mocks.
- **Doc 16 (Evaluation)** is the *quality* counterpart: same recorded bodies,
  but scored for model quality (live judge) rather than replayed for pipeline
  fidelity. Golden tests gate the pipeline; eval gates the model.
