# 18. Agent & LLM Testing

> **Status**: design chapter, 2026-05-21. Supersedes the per-call
> deterministic-scoring framing of [Doc 16](./16-evaluation-framework.md)
> §7.1 (see [`eval-and-arc-llm-roadmap.md`](../eval-and-arc-llm-roadmap.md)
> for why that shape was wrong). Statistical details + research
> citations live in [`eval-methodology.md`](../eval-methodology.md);
> this chapter is the architecture.

## 1. Why this is "testing", not just "eval"

What started as "how do we eval an LLM pipeline" is really the broader
discipline of **applying software-testing thinking to agents/LLMs**.
Eval (measuring output quality against a notion of correct) is *one*
mode. The full discipline also includes property-based testing,
metamorphic testing, mutation testing, and golden/approval testing —
all of which software engineering built decades ago to handle the
**oracle problem** (you can't always know the right answer), which is
exactly the LLM situation.

The driving need (from five production case studies — Fractional AI's
Cando, e-commerce doc-processing, Zapier spec-gen, Superintelligent
RAG, Sincera taxonomy): when you **change a prompt, change the data,
or change the model**, you must answer "did this make it better or
worse, and by how much, *with confidence*?" — i.e. A/B release testing
and prompt migration for LLM systems. Three of the five named "we
couldn't tell if a prompt tweak helped or hurt" as the core pain.

## 2. The core reframe: behavioral diff, not text diff

You do **not** diff the output text. You run config A and config B
over the same corpus and diff their **behavior** along test dimensions:

```
config A ─┐
          ├─► run same corpus ─► BehaviorReport A
config B ─┘                      BehaviorReport B
                                       │
                       diff = change in test-dimension outcomes
                              (with statistical significance)
```

A raw text diff of "drive vs you should drive the car over" is noise.
The behavioral diff — "invariant violation rate 2%→12%", "paraphrase
invariance 90%→70%", "precision +0.09, McNemar p=0.03" — is signal.

## 3. The oracle problem

Most LLM outputs have no cheap oracle: there's no ground-truth string
to compare against, and building gold standards is expensive (if you
had unlimited perfect outputs you wouldn't need the LLM). Software
testing's answer is to test **relations and properties** that must
hold regardless of the specific right answer. That's the backbone of
this chapter: **most test modes need no oracle.**

| Mode | Needs oracle? | Tests |
|---|---|---|
| Invariant (property) | ❌ | a single output satisfies a postcondition |
| Metamorphic | ❌ | two related runs satisfy a relation |
| Mutation | ❌ | failure surface / whether the eval itself is blind |
| Golden / snapshot | ⚠️ approved reference, not authored truth | drift from a frozen good output |
| Quality scoring (judge/gold) | ✅ | correctness against gold or a validated judge |

## 4. The five test dimensions

### 4.1 Invariant (property-based)

A postcondition every valid output must satisfy. Checkable on one
output, no oracle.

```rust
trait Invariant {
    fn holds(&self, input: &ChatRequest, output: &ChatResponse) -> CheckResult;
}
```

| Class | Example |
|---|---|
| Structural | valid JSON / conforms to schema |
| **Closed-set membership** | output category ∈ taxonomy / tool ∈ DB / endpoint path ∈ source page |
| Grounding | every cited fact/field/path appears in the input |
| Safety | no PII / does not echo injected instructions |

Closed-set membership is the highest-value generic one: **3 of 5 case
studies' hallucination metric is exactly "is the generated thing in
the source set?"** — Zapier hallucinated paths, RAG made-up tools,
Sincera invalid categories. It's a `HashSet` lookup, no judge, and
catches a whole hallucination class for free.

Diff dimension: **violation rate**. New prompt drives JSON-parse
failures 2%→12% = regression a text diff never shows.

### 4.2 Metamorphic relation

A relation between `(input, input')` and `(output, output')`. Needs a
pair of runs, no oracle.

```rust
trait MetamorphicRelation {
    fn transform(&self, base: &ChatRequest) -> ChatRequest;
    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckResult;
}
```

| Type | Transform | Relation | Catches |
|---|---|---|---|
| **INV** | paraphrase / vary distance / rename / reorder | output semantically unchanged | surface sensitivity |
| **INV** | same input, temp 0, twice | byte-identical | non-determinism / flakiness |
| **DIR** | add "under 50 words" | output shrinks | ignored constraint |
| **DIR** | "wash car" → "buy coffee" | decision flips | reasons about constraint vs pattern-matches |
| **DIR** | plant a known bug | critic must flag ≥ it | missed detection |

Diff dimension: **relation-preservation rate**. B may have higher
precision yet be *less robust* (paraphrase invariance 90%→70%) — only
the metamorphic axis surfaces that.

### 4.3 Mutation testing — two meanings, the second is the payoff

**(a) Mutate the input → map the failure surface** (fuzzing-flavored).
Systematically perturb inputs (typos, entity swaps, distractor docs,
push the relevant bit into the middle of a long context) and chart
where quality collapses. Produces the model's brittleness map — e.g.
"detection rate sags when the bug is mid-file past 8k tokens"
(lost-in-the-middle).

**(b) Mutate the system → validate the eval isn't blind** (mutation
testing proper). In software you mutate the *code* and check the
*test suite* catches it; a suite that misses mutations is weak. For
LLMs: mutate the prompt/config with a known degradation and check the
eval's relevant metric moves.

```rust
trait Mutation {
    fn apply(&self, cfg: &PipelineConfig) -> PipelineConfig;  // delete "cite sources" instruction
    fn expected_to_break(&self) -> CheckId;                   // → grounding invariant should start failing
}
```

Run the mutated system → re-run the eval → did the corresponding
check's violation rate rise?

- rose → the eval can catch this regression class ✅
- didn't rise → **your eval is blind to this dimension** ⚠️

> **A regression you can't detect is a regression that will ship.**
> Before you trust an eval to gate a prompt migration, mutation
> testing tells you whether the eval would even catch the regression.
> This is the automatable form of "validate the judge/eval itself"
> (§5, meta-evaluation).

### 4.4 Golden / snapshot / approval testing

Record a known-good output ("golden"), have a human approve it, then
guard future runs against drift from it. The software-testing
lineage: golden master, snapshot testing (Jest-style), approval
testing.

The LLM wrinkle: **exact-match golden is too brittle** (non-determinism
makes byte-equality fail constantly). So golden matching is tiered:

| Match | Mechanism | When |
|---|---|---|
| exact | string equality | deterministic outputs (temp 0, structured) |
| structural | same JSON shape / field set | structured output, content may vary |
| **semantic** | judge says "equivalent to golden" | free-text outputs (the common LLM case) |

Workflow — and this is the **most practical mode for prompt
migration**:

```
1. run prompt v1 over corpus → outputs
2. human reviews + APPROVES → outputs become goldens (frozen)
3. change to prompt v2
4. run v2 → diff each output against its golden (semantic match)
5. drift flagged per case → human re-approves (intended change)
                            or fixes (regression)
```

A golden is **not** "the correct answer" (that's §4.5 gold standard).
A golden is "an output we looked at and blessed." The test is *drift
from blessed*, which is exactly what you want when migrating a prompt
you don't want to silently change behavior.

### 4.5 Quality scoring (the only oracle-requiring mode)

Precision/recall against gold or a validated judge — the eval most
people mean by "eval". Detailed in [`eval-methodology.md`](../eval-methodology.md);
summarized in §6. This is the one mode that genuinely needs a notion
of "correct," and §5 is about where that comes from.

## 5. "Which is actually better?" — the decision

The machinery says "B's precision is higher / B violates fewer
invariants." But correct **against what**? Someone must define it.
Three regimes, descending trust:

- **Regime 1 — gold standard exists.** Hand-authored references; "B is
  better" = B matches gold more, significant. Unambiguous. (Cando-Peter
  compared drafts to historic gold assessments; this is why it worked.)
- **Regime 2 — no gold, judge decides absolute correctness.** "Better"
  = "the judge prefers B," only as reliable as the judge (~80% human
  agreement, Zheng 2023). **Valid only if you validate the judge.**
- **Regime 3 — both often correct, binary saturates.** Needs pairwise
  preference (position-bias-corrected) or scored judging (noisier).

**The non-negotiable in Regime 2/3 — validate the judge** (meta-eval):
label ~20–30 items by hand, run the judge on them, require ≥85%
judge–human agreement before trusting verdicts. Anti-incest (§7) and
Wilson CIs make the judge's *output* honest; only a human-labeled
sample makes the judge itself *trustworthy*. §4.3(b) mutation testing
is the automatable companion.

**Decision rule (one line):**

> "B is better" = B is more correct against a *defined* notion of
> correct (gold, or a validated judge), the delta is **paired-test
> significant** on the shared corpus, and the **operational cost**
> (tokens/latency) is acceptable for that quality gain.

If you can't fill in "a defined notion of correct," you don't have a
test — you have two outputs and an opinion.

## 6. Comparison statistics

Two metric tiers:

- **Operational** (free, no judge): error rate, token cost, latency
  (p50/p99), output length. Plain deltas/ratios.
- **Quality** (needs a judge/gold): precision (+ Wilson CI), recall,
  per-item verdicts.

**The one thing people get wrong**: `eval diff` runs the **same corpus**
through both configs, so per-item outcomes are **paired**. Comparing
two independent 1-sample Wilson CIs for overlap is statistically wrong
(loses power; overlapping CIs do *not* imply non-significance). The
correct test is **McNemar** on the discordant cells:

```
            B correct   B wrong
A correct  │ concordant │  b  │   b = A-right, B-wrong  (regression)
A wrong    │    c       │ conc│   c = A-wrong, B-right  (improvement)
McNemar χ² = (b−c)²/(b+c)      H0: b = c
```

Only the discordant cells carry information; that's why pairing is more
powerful. Use exact binomial for small (b+c). For non-binary metrics,
**paired bootstrap** (the NLP standard). Both report **recall and
precision** — a config that emits nothing scores precision 1.0 and
recall ~0.

## 7. The judge: native, via tars's own provider interface

**Decision: the judge is an LLM accessed through tars's `LlmProvider`
interface — not an external eval SaaS.** The concrete default is the
`claude_cli` backend (subscription auth via `claude login`; a strong
Claude model as judge). No Langsmith/Braintrust integration.

This is already supported at the library level — `LlmJudge` takes
`Arc<dyn LlmService>`:

```rust
let judge_provider = registry.get(&ProviderId::new("claude_cli")).unwrap();
let judge_pipeline = Pipeline::default_chain(judge_provider, opts);
let judge = LlmJudge::new(
    Arc::new(judge_pipeline),
    "claude_cli:claude-sonnet-4-5",          // id for anti-incest
    ModelHint::Explicit("claude-sonnet-4-5".into()),
);
```

Because the judge runs through a normal tars pipeline, it gets for
free: **event-store** (judge calls appear in `tars events`, auditable),
**cache** (re-judging the same item hits cache), **retry/fallback**,
and **budget** (cost-capped; note `claude_cli` token counts are
unreliable but it's subscription-billed, so per-call cost is moot).
The judge can be any provider — `claude_cli`, `anthropic` API, OpenAI,
local vLLM — by changing one id. **Self-contained: eval uses tars's
own LLM infra, not an external tool.**

**Anti-incest** (encoded as `ensure_anti_incest`, from Panickssery et
al. 2024, "LLM Evaluators Recognize and Favor Their Own Generations"):
a judge whose provider matches the system-under-test's provider
rubber-stamps shared blind spots. The runtime **refuses** when the
provider prefix matches. So: system-under-test on OpenAI/Gemini/local
→ `claude_cli` judges; or vice-versa. Never the same provider judging
itself.

Where the judge is used: **only the semantic legs** — §4.4 semantic
golden match and §4.5 quality scoring (and the *semantic* flavor of
§4.2 metamorphic relations, "are these two answers equivalent?").
Invariants (§4.1), structural metamorphic relations, and mutation
(§4.3) are oracle-free and never call the judge — cheap and
deterministic.

## 8. Architecture

```
Check (trait family)
 ├─ Invariant           single output, no oracle           §4.1
 ├─ MetamorphicRelation paired runs, no oracle (mostly)     §4.2
 ├─ Golden              drift from approved snapshot        §4.4
 └─ Scorer (Judge)      correctness, needs oracle           §4.5

CheckRunner:  corpus × config → BehaviorReport
                { per-check pass/violation rates, per-item results }

DiffEngine:   BehaviorReport(A) vs BehaviorReport(B)
                ├─ invariant violation-rate deltas
                ├─ metamorphic preservation-rate deltas
                ├─ golden drift counts (per case)
                ├─ quality: precision/recall + McNemar (paired)
                └─ operational: cost / latency / error deltas
              → behavioral diff (NOT text diff)

MutationHarness: apply system mutation → re-run → assert the
                 corresponding check moved (else: eval is blind)
```

The judge plugs into `Scorer` (and semantic `Golden`/`MetamorphicRelation`)
via `LlmJudge` over a tars provider (§7).

## 9. Framework vs domain — the discipline

The hard, valuable parts of testing are domain-specific and the
framework must **not** try to own them. The split:

| Framework provides (generic) | App provides (domain) |
|---|---|
| `Invariant` / `MetamorphicRelation` / `Mutation` / `Golden` / `Scorer` **traits** | the specific invariants ("∈ Shopify taxonomy") |
| `CheckRunner`, `DiffEngine`, `MutationHarness` | the specific metamorphic relations ("paraphrase a legal question") |
| Stats: McNemar, Wilson CI, paired bootstrap | the specific mutations ("delete my cite-sources instruction") |
| A few **generic built-in checks**: valid-JSON, closed-set-membership, determinism, schema-conformance | the corpus + gold + goldens |
| `LlmJudge` over any tars provider | the judge prompt + which model |

What generalizes is the **rigor** (the paradigms, the statistics, the
runner). What doesn't is the **domain content** (the actual checks).
This is why the framework can be excellent: the traits are domain-free
and the value (behavioral diff, mutation-validates-eval) is domain-free.

## 10. Positioning vs Braintrust / Langsmith

These tools are good, and most of why is **UI + hosted service + human
workflow** — eyeballing failures, labeling datasets, curating from
production traces, visual experiment comparison, drill-down from
aggregate to instance. None of that is what a Rust agent-runtime is or
should be.

tars's angle is different and defensible: **"eval as software testing"
— property / metamorphic / mutation / golden as first-class primitives**
(not just "write a scorer"), **runtime-native** (the judge is a normal
tars LLM call; checks read the same event store everything else uses),
**CLI/Rust**, and **self-contained** (no external SaaS dependency). It
**feeds into** a UI tool via JSON export if a team wants one, but never
**requires** one.

Decision: **do not integrate Langsmith/Braintrust.** Provide the rigor
+ the native judge + JSON artifacts. The human-workflow/UI layer is
explicitly out of scope (it's a product, not a library).

## 11. Worked examples

### Car wash — postcondition + metamorphic pair

*"To wash your car at a wash 50 m away, drive or walk?"* The trap:
50 m is walkable, so the surface answer is "walk" — but the car must
be at the wash. Don't test the answer string; test the **postcondition**
`car_is_at_wash ∧ car_gets_clean` (checked by a judge on goal
attainment, not abstract correctness). The real test is the
metamorphic **pair**:

- **INV**: vary distance (50 m → 50 km → "nearby") → answer must not
  change ("drive"). A model that flips near→walk keys on distance.
- **DIR**: swap object ("wash car" → "buy coffee") → answer must flip
  to "walk" (the transport constraint disappeared). A model that says
  "short distance → walk" both times never understood the constraint.

Neither single answer proves anything; the pair isolates reasoning
from pattern-matching.

### Closed-set membership — free hallucination check (3/5 cases)

When the output must come from a fixed set (taxonomy category, tool in
DB, endpoint path in source), hallucination = `output ∉ set` — a
`HashSet` lookup, no judge. The cheapest, most reliable hallucination
test there is, and it recurs across Zapier / RAG / Sincera.

### Context-size needle — arc's problem (lost-in-the-middle)

arc feeds the critic a whole file. Plant the **same** known bug, vary
(file size, bug position), measure detection rate. A curve that sags
in the middle / collapses past some length is the lost-in-the-middle
effect (Liu 2024; RULER) biting the critic — telling you to chunk the
file rather than feed it whole. The planted bug *is* the oracle, free.

## 12. Field signals — five-case convergence

From the five Fractional AI production case studies, what converges
(this is motivation, not method):

| Signal | Hits | tars status |
|---|---|---|
| Hallucination is the #1 enemy | 5/5 | — |
| Closed-set hallucination = membership check (oracle-free) | 3/5 | §4.1, generic built-in |
| Online confidence-judge → human triage | 3/5 | app-layer (confidence is domain; framework gives `NeedsReview` plumbing at most) |
| "Can't tell if a prompt tweak helped" → eval diff | 3/5 | §2, the core deliverable |
| Dedicated eval tooling (Braintrust/Langsmith) | 2/5 | building runtime-native rigor instead (§10) |
| Multi-step specialized-agent pipeline | 5/5 | ✅ Orchestrator/Worker/Critic |
| Retry / timeout / degradation | 2/5 | ✅ shipped (cost & reliability roadmap) |

## 13. Build order

1. **`Invariant` trait + generic built-ins (JSON / membership /
   determinism) + `CheckRunner`** — no judge, unblocked, immediately
   useful.
2. **Operational + invariant `eval diff`** — needs no judge; two
   `manifest.json`s already exist after corpus replay.
3. **`tars eval judge --judge claude_cli` + `JudgeReport` persistence**
   — wires the native judge; the prerequisite for the quality diff.
4. **Quality McNemar diff** — consumes (3)'s artifacts.
5. **`Golden` mode** — record/approve/diff against snapshots (semantic
   match via the §7 judge).
6. **`MetamorphicRelation` trait + INV/DIR built-ins.**
7. **`MutationHarness`** — the meta-eval payoff; built on 1–6.

Each step ships independently and is useful on its own.

## 14. Cross-references

- [`../eval-methodology.md`](../eval-methodology.md) — full statistics
  + verified research citations (McNemar/Dietterich, Koehn bootstrap,
  Wilson, Zheng MT-Bench, Panickssery self-preference, CheckList,
  Structure-Invariant Testing, semantic entropy, Lost-in-the-Middle).
- [`../eval-and-arc-llm-roadmap.md`](../eval-and-arc-llm-roadmap.md) —
  the plan + the arc_llm collapse this rides on.
- [`16-evaluation-framework.md`](./16-evaluation-framework.md) —
  superseded for §7.1 (per-call deterministic scoring); its event/
  channel plumbing concepts remain valid.
- [`../recipes/`](../recipes/) — usage cookbooks once these land.
