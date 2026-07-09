# Testing agents, DAGs & workflows — mock and golden LLM

How to write a **deterministic, offline, zero-cost** end-to-end test of an
agent, a multi-step DAG (`run_task`), or a workflow — without ever calling a
real model in CI.

The design rationale is [`architecture/25-agent-dag-testing.md`](./architecture/25-agent-dag-testing.md);
this is the how-to. Pick the mode that matches what you're actually testing:

| You're testing… | Mode | Tool |
|---|---|---|
| **Orchestration logic** — does the DAG branch / retry / skip / deadlock correctly? | Scripted | `ScriptedProvider` (§2) |
| **Pipeline fidelity** — does parse→persist→render still produce the right artifact for a *realistic* model response? | Golden replay | `GoldenProvider` (§3) |
| **Model quality** — is the model itself good? | (not this) | live dogfood / eval — Doc 16 |

> The golden line is the important distinction: a golden test tells you **the
> pipeline still behaves**. It says *nothing* about whether the model is good —
> the response is frozen. To test the model, you want eval, not this.

---

## 0. The one thing every test does: inject at the seam

Every agent or DAG under test should take its model as a concrete
`LlmService` — built over a swappable `Arc<dyn LlmProvider>`, never a provider
constructed internally. That seam is the whole game: in production you build the
service over a real provider; in a test you build it over a scripted or golden
provider and nothing else changes.

```rust
// production: wrap the resolved provider in the canonical middleware chain
let agent = ReviewAgent::new(LlmService::default_chain(provider, model, opts));

// test: a leaf service over a scripted/golden provider
let agent = ReviewAgent::new(LlmService::of(scripted_provider, "mock-model"));
```

If your agent doesn't have this seam yet, add it first — see the worked example
in `examples/examples/testing/main.rs`. Everything below plugs into
this one point.

---

## 1. The simplest case — one call, one answer

For a unit test of a single-call agent, `MockProvider` is enough:

```rust
use tars_provider::{MockProvider, CannedResponse};

let llm = MockProvider::new("mock", CannedResponse::text("found a bug"));
let agent = ReviewAgent::new(LlmService::of(llm, "mock-model"));
let out = agent.run(input).await?;
assert_eq!(out.verdict, Verdict::Bug);

// assert on what the model was ASKED, too:
assert!(llm.history_snapshot()[0].user_contains("race condition"));
```

`CannedResponse` has three shapes:

- `CannedResponse::text("…")` — a plain completion.
- `CannedResponse::Sequence(vec![…])` — a verbatim `ChatEvent` stream, for
  tool-use and structured-output paths.
- `CannedResponse::Error("…")` — the provider call fails (test your error
  handling).

For a fixed multi-call sequence, `MockProvider::with_responses(id, vec![…])`
pops one per call. **But** the moment your DAG runs calls in parallel or you
care *which* call gets *which* answer, reach for §2 instead — a positional queue
is fragile.

---

## 2. Mode A — `ScriptedProvider`: match the call, don't count it

For a real DAG (`run_task`: Orchestrator + N steps × {worker, critic}), each
call needs a different shape and the calls may race. `ScriptedProvider` selects
a response by **what the call is** — agent id, model, a predicate on the request
— so the test is order-independent and reads as intent:

```rust
use tars_provider::{ScriptedProvider, Matcher, CannedResponse};

let llm = ScriptedProvider::builder()
    .on(Matcher::RequestSchema("Plan"),  plan_json(["scan", "fix"]))
    .on(Matcher::Agent("worker:scan"),   worker_result("found 2 issues"))
    .on(Matcher::Agent("worker:fix"),    worker_result("patched both"))
    .on(Matcher::Agent("critic"),        verdict("approve"))
    .otherwise(CannedResponse::Error("unexpected LLM call".into()))  // fail LOUD
    .build();

let outcome = run_task(&rt, LlmService::of(llm, "mock-model"), task).await?;
assert!(matches!(outcome, TaskOutcome::Completed { .. }));
```

Why match instead of a FIFO queue:

- **Parallel-safe.** Two workers racing each match their own rule — no
  `--test-threads=1`, no flake. A FIFO can't tell which worker's call arrived
  first.
- **Robust to edits.** `Matcher::Agent("critic")` keeps pointing at the critic
  even after you insert a step before it. "The 3rd response" silently shifts to
  the wrong call.
- **Fails precisely.** `.otherwise(Error)` turns "I fed the wrong number of
  responses" into `unexpected LLM call: agent=worker:fix` at the exact offending
  call — not a hang or a confusing dry-queue panic.

Matchers compose: `Matcher::And(vec![Matcher::Agent("critic"),
Matcher::UserMatches(r"F-12")])`, `Matcher::Predicate(|req| …)`, `Matcher::
SystemContains("…")`, bounded reuse with `.times(n)`.

Assert on the hand-off, too — that the blackboard actually flowed between steps:

```rust
// the fix worker's prompt must carry the scan's findings
let fix_call = llm.history_for("worker:fix");
assert!(fix_call.user_contains("found 2 issues"));
```

Use mode A whenever you're testing **logic**. A terse hand-authored shape is the
right amount of fidelity for "does the DAG branch correctly."

---

## 3. Mode B — `GoldenProvider`: real recorded responses, replayed offline

When you're testing **pipeline fidelity** — does parse → validate → persist →
render still produce the same artifact given a *realistic, messy* model
response — hand-authored shapes lie (they're too clean). Record a real response
once, freeze it, replay it forever with zero live calls.

The golden store is not a new format: it's [Doc 17](./architecture/17-pipeline-event-store.md)'s
`bodies.db`, keyed by `request_fingerprint`. Record mode fills it; replay mode
reads it.

### Record once (live, on purpose)

```bash
# Run the DAG for real, tagging the calls, so every response lands in the events DB.
TARS_GOLDEN_TAG=golden:run_task.happy_path \
  cargo test --test run_task happy_path -- --ignored --record

# Freeze the recorded {fingerprint → body} pairs as a committed fixture.
tars golden export --tag golden:run_task.happy_path --out tests/golden/run_task.happy_path/
```

### Replay forever (offline, in CI)

```rust
use tars_provider::{GoldenProvider, MissPolicy};

let llm = GoldenProvider::from_dir("tests/golden/run_task.happy_path/")
    .miss(MissPolicy::FailClosed);          // a miss is a test failure, never a live call
let outcome = run_task(&rt, LlmService::of(llm, "mock-model"), task).await?;
assert_golden(&outcome, "tests/golden/run_task.happy_path/expected/");
```

Each call's `request_fingerprint` hits the frozen body. **Zero** network calls.
A failure now means *the pipeline changed* — never "the model said something
different today."

### Fail-closed is the point

In replay, a cache miss is a hard error naming the missing fingerprint
("golden absent, re-record"), not a silent live call. This also catches a real
bug class: if your prompt embeds a timestamp / temp path / random id, its
fingerprint changes every run and replay misses — the failure tells you you're
leaking nondeterminism into the prompt. (For genuinely variable input, use a
mode-A predicate on the stable part instead of golden.)

---

## 4. Don't freeze a bad golden — the curation gate

A recorded response is a **candidate**, not automatically a golden. Freezing a
truncated / bug-missing / hallucinated response bakes wrong behavior into the
regression net forever. A candidate becomes golden only if it passes the gate:

| Gate | Rejects |
|---|---|
| **Non-degenerate** — schema-valid, `stop_reason=EndTurn`, no long repeat run | truncation, loops |
| **Recall = 1** on the fixture's planted oracle (`expected.toml`: the issues/tools that MUST appear) | a golden that missed the bug |
| **Precision** — extra outputs within `allow_extra` | a noisy golden |
| **Stability** — record K=3, finding-set Jaccard ≥ 0.8, freeze the modal one | a flaky prompt frozen at one face |
| **Human approval** — the golden diff read + approved in the PR | a silently-wrong freeze |

The oracle is **your fixture's declared expectation**, not the model's say-so.
The gate runs at record time; replay never re-checks it. If a candidate can't
pass stability, tighten the fixture (make the input less ambiguous) — don't
lower the bar.

---

## 5. Guarantee coverage — the workflow registry

"Do we have an e2e test for the resume-after-crash workflow?" should be a
*checked* fact, not a hope. Declare every critical journey:

```toml
# tests/workflows.toml
[[workflow]]
id     = "run_task.happy_path"
kind   = "dag"
golden = "tests/golden/run_task.happy_path/"

[[workflow]]
id     = "run_task.worker_crash_resume"
kind   = "dag"
golden = "tests/golden/run_task.worker_crash_resume/"
```

A meta-test (`workflow_coverage_is_complete`) fails both ways: a registry row
with no golden dir (a declared journey with no test), **or** a `#[test]` journey
with no registry row (an orphan). You can't land a workflow without its golden,
and you can't delete a golden out from under a registered journey.

---

## 6. Assert behavior, not bytes

A golden comparison normalizes volatile fields first, then diffs *structure* and
reports the field that drifted:

- **Normalized before compare:** `run_id`, `trajectory_id`, timestamps, sqlite
  `rowid`, worktree paths → placeholders. Byte-equality on raw output flakes;
  on normalized output it's stable.
- **The failure names the field:** `step "scan": finding_events +0, expected
  +12 (found events deferred)` — not "not equal". You read it like a `tars
  blackboard replay` delta (that's the same engine — see
  [blackboard-investigation.md](./blackboard-investigation.md)).
- **Free dimensions, no oracle:** layer in closed-set membership (every produced
  `rule_id` ∈ the rubric), determinism (replay twice → identical post-
  normalization), schema-conformance. These catch regressions even where no
  golden is pinned. See [Doc 18](./architecture/18-agent-testing.md) for the
  full dimension set.

---

## Quick reference

| I want to… | Do |
|---|---|
| Unit-test a single-call agent | `MockProvider::new(id, CannedResponse::text(…))` |
| Test a DAG's branch/retry/skip logic | `ScriptedProvider` with `Matcher::Agent(…)` rules + `.otherwise(Error)` |
| Make a parallel DAG test deterministic | matcher rules (mode A) — never a positional FIFO |
| Regression-test the pipeline with realistic output | record once → `tars golden export` → `GoldenProvider` replay |
| Stop a flaky/bad golden landing | the curation gate (§4) — recall=1 + stability + approval |
| Know every journey has a test | `tests/workflows.toml` + `workflow_coverage_is_complete` |
| Debug *why* a golden test's artifact is wrong | [`tars blackboard replay`](./blackboard-investigation.md) — same delta vocabulary |

## See also

- [`architecture/25-agent-dag-testing.md`](./architecture/25-agent-dag-testing.md) — the design + roadmap
- [`architecture/18-agent-testing.md`](./architecture/18-agent-testing.md) — oracle theory, behavioral diff, test dimensions
- [`architecture/17-pipeline-event-store.md`](./architecture/17-pipeline-event-store.md) — the `request_fingerprint` + `bodies.db` golden substrate
- [`blackboard-investigation.md`](./blackboard-investigation.md) — debug the durable state a failing test points at
- [`observability.md`](./observability.md) — the three live observability grains
