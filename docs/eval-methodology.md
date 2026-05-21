# Eval methodology — what tars produces and how to compare runs

This doc answers two questions about tars's eval framework:

1. **What metrics can you actually get** out of a corpus replay + judge pass?
2. **How do you compare two runs** correctly — and what's the
   statistical research behind the comparison?

It's the methodology companion to the tooling described in
[`eval-and-arc-llm-roadmap.md`](./eval-and-arc-llm-roadmap.md) (§1.1
RunReport, §1.2 Judge, §1.3 corpus replay) and the usage recipes in
[`recipes/`](./recipes/).

> **Scope note**: this is *paired offline evaluation*, **not**
> production A/B testing. We run the **same fixed corpus** through two
> configurations and compare. A real A/B test splits live traffic by
> user with random assignment and measures business outcomes — that
> needs traffic-routing + identity infrastructure tars doesn't have
> (it'd come after the M6/M7 multi-tenant + dashboard milestones). The
> two share vocabulary (two variants, significance testing) but are
> different tools for different jobs.

---

## 1. Two tiers of metrics

### Tier 1 — operational (free, no judge needed)

Produced directly by `tars eval run` (corpus replay), in
`manifest.json` per case + aggregate:

| Metric | Source | Answers |
|---|---|---|
| **error rate** | `error_count / case_count` | Does B fail more cases? |
| **token cost** | `total_usage.{input,output}` | Does B burn more tokens → $? |
| **latency** | per-case `wall_clock_ms` (→ p50/p99) | Is B slower? |
| **output length** | `output_chars` | Is B more verbose / terser? |

These need no ground truth and no second LLM. They answer "is B
cheaper / faster / more reliable" — but **not** "is B more correct".

### Tier 2 — quality (needs a judge pass)

Produced by `tars eval judge` over a run's outputs, in `JudgeReport`:

| Metric | Answers |
|---|---|
| **precision** = TP / (TP + FP), with Wilson CI | Are B's outputs more correct? |
| **unsure rate** = Unsure / n | Is the judge less confident on B (calibration signal)? |
| **per-item verdicts** | *Which specific cases* flipped right↔wrong |

---

## 2. How to compare — and the one thing most people get wrong

### Operational: plain deltas + ratios

```
tokens in:   12.3k → 16.1k   (+31%)
latency p50:  1.20s → 0.95s  (−21%)
errors:        2/50 → 1/50   (−1)
```

No statistical subtlety. Report the delta and move on.

### Quality: the data is PAIRED — don't compare independent CIs

⚠️ The single most common mistake: computing a Wilson CI for run A's
precision and run B's precision separately, then eyeballing whether
they overlap.

That's **wrong for this design**, because `eval diff` runs the **same
corpus** through both configs — the per-item outcomes are *paired*.
Comparing two independent 1-sample CIs:

- throws away the pairing (loses statistical power), and
- gives a conservative, sometimes misleading answer (non-overlapping
  CIs imply significance, but *overlapping* CIs do **not** imply
  non-significance — a widely-documented fallacy).

### The right test: McNemar (for binary correctness)

Build the 2×2 paired contingency table over the shared cases:

```
                 B correct   B wrong
   A correct  │  concordant │   b      │   ← b: A right, B wrong  (REGRESSION)
   A wrong    │     c       │ concordant│   ← c: A wrong, B right  (IMPROVEMENT)

   McNemar χ² = (b − c)² / (b + c)        (1 d.o.f.)
   H0: b = c   (the change has no systematic effect)
```

Only the **discordant** cells (b, c) carry information — the cases
both configs agree on tell you nothing about the *difference*. This is
exactly why pairing matters: McNemar is more sensitive than an
unpaired two-proportion test at the same sample size, because it
conditions on the agreements.

For small discordant counts (b + c < ~25), use the **exact binomial**
version instead of the χ² approximation.

### Alternative: paired bootstrap (for non-binary metrics)

If the per-item metric isn't binary (e.g. a 1–5 quality score, or a
continuous severity), McNemar doesn't apply. The NLP-standard
alternative is **paired bootstrap resampling**: resample the corpus
with replacement N times, recompute the metric delta on each
resample, and read the significance off the bootstrap distribution.
This is the de-facto method in machine-translation evaluation and
works down to a few hundred items.

tars's V1 judge is binary (TP/FP/Unsure), so McNemar is the natural
fit. A paired-bootstrap path is a clean extension if/when scored
(non-binary) verdicts land.

---

## 3. What `tars eval diff` produces (target shape)

```
$ tars eval diff eval-runs/baseline/ eval-runs/candidate/

operational:
  cases:        50 → 50
  errors:        2 → 1          (−1)
  tokens in:  12.3k → 16.1k     (+31%)
  tokens out:  8.1k → 7.4k      (−9%)
  latency p50:  1.20s → 0.95s   (−21%)

quality (judge: anthropic:claude-opus-4-7):
  precision:  0.72 [0.58, 0.83] → 0.81 [0.68, 0.90]   (+0.09)
  unsure:        3 → 2

  paired changes (same 50 cases):
    improved (FP→TP):   7   [case_003, case_011, …]
    regressed (TP→FP):  2   [case_022, case_041]
    unchanged:         41

  McNemar: b=2, c=7  →  χ²=2.78, p=0.095
  → improvement NOT significant at α=0.05 (need more cases or a bigger effect)
```

The `p=0.095` line is the decision-driver: precision rose 0.09, but at
n=50 that's not yet distinguishable from luck. The named regression
cases (`case_022`, `case_041`) are the actionable output — go read
what got worse.

---

## 3.5 But how do you actually know which is *better*?

The machinery above computes "B has higher precision than A, and the
gap is/isn't significant." But precision against **what**? This is the
question everything hinges on, and it has no free answer: **someone has
to define what "correct" means.** There are three regimes, in
descending order of trustworthiness.

### Regime 1 — gold standard exists (most trustworthy)

The corpus has `expected.txt` per case — human-written reference
answers. "Correct" = the output matches the gold standard, and the
judge's job is the narrow, reliable task of *comparing output to
reference* (not deciding correctness in a vacuum).

This is what makes the **Cando-Peter** setup work: their LLM-as-judge
compared new risk-assessment drafts against **historic gold-standard
assessments**. The judge wasn't inventing "correct" — it was checking
against human-authored truth. "B is better" = B matches gold more
often, McNemar-significant. **Unambiguous.**

### Regime 2 — no gold, judge decides absolute correctness (weaker)

No `expected.txt`. The judge rules TP/FP on its own opinion of whether
the output is right. Now "B is better" means **"the judge prefers B's
outputs"** — which is only as reliable as the judge. Per Zheng 2023
that's ~80% human agreement: good, not ground truth.

**This regime is only valid if you've validated the judge** (next).

### Regime 3 — both often correct (binary saturates)

When A and B are both ~95% correct, binary precision can't separate
them — you've hit the ceiling. To rank "both acceptable, but which is
*better*" you need either:

- **pairwise preference** (judge sees A and B, picks the better) — but
  this reintroduces position bias (Zheng 2023), so you must randomize
  + average both orderings; or
- **scored judging** (1–5 quality) — noisier, needs paired bootstrap
  instead of McNemar.

Both are heavier and noisier than binary. tars V1 is binary on purpose;
these are documented extensions, not V1.

### The non-negotiable: validate the judge itself

In Regime 2 and 3 you are trusting the judge, so you must **spot-check
the judge against humans**. Label ~20–30 items yourself, run the judge
on the same items, compute judge-vs-human agreement. If the judge
agrees with you 85%+, the judge's verdicts are worth acting on. If it
agrees 60%, the eval is measuring the judge's confusion, not the
configs' quality.

This is **meta-evaluation** — evaluating the evaluator — and it's the
step that's easiest to skip and most expensive to skip. The
`anti-incest` rule and Wilson CIs make the judge's *output* honest;
only a human-labeled calibration sample makes the judge itself
*trustworthy*.

### Decision rule, in one line

> **"B is better"** = B is more correct against a *defined* notion of
> correct (gold standard, or a judge you've validated against humans),
> the delta is **McNemar-significant** on the shared corpus, and the
> **operational cost** (tokens / latency) is acceptable for that
> quality gain.

If you can't fill in "a *defined* notion of correct," you don't yet
have an eval — you have two outputs and an opinion.

---

## 4. Research basis

These aren't novel choices — they're the established methods for the
problems we have. Verified references (2026-05-21):

### Paired comparison of two systems on one test set

- **McNemar's test** for paired binary outcomes:
  McNemar, Q. (1947), *"Note on the sampling error of the difference
  between correlated proportions or percentages"*, Psychometrika.
- **Recommended specifically for comparing two classifiers you can
  only run once**:
  Dietterich, T. G. (1998), *["Approximate Statistical Tests for
  Comparing Supervised Classification Learning
  Algorithms"](https://sebastianraschka.com/blog/2018/model-evaluation-selection-part4.html)*,
  Neural Computation. Dietterich finds McNemar on the misclassification
  matrix as powerful as the 5×2cv t-test when repeated runs aren't
  feasible — exactly our "one corpus, two configs" situation.

### Significance testing for NLP-style metrics

- **Bootstrap resampling** as the field-standard for system comparison:
  Koehn, P. (2004), *["Statistical Significance Tests for Machine
  Translation Evaluation"](https://aclanthology.org/W04-3250.pdf)*,
  EMNLP. Validated down to ~300-sentence test sets.
- **Comprehensive practitioner guide**:
  Dror, R. et al. (2018), *"The Hitchhiker's Guide to Testing
  Statistical Significance in Natural Language Processing"*, ACL.

### Binomial proportion confidence intervals

- **Wilson score interval** (what `precision_with_ci` implements):
  Wilson, E. B. (1927), *"Probable Inference, the Law of Succession,
  and Statistical Inference"*, JASA. Chosen over the normal
  approximation because it stays in [0, 1] and behaves at the edges
  (precision = 0 or 1) — common when n is small, as eval corpora
  usually are.

### LLM-as-judge validity and biases

- **The methodology + its limits**:
  Zheng, L. et al. (2023), *["Judging LLM-as-a-Judge with MT-Bench and
  Chatbot Arena"](https://arxiv.org/abs/2306.05685)*, NeurIPS.
  Strong judges (GPT-4) reach **>80% agreement with humans** — the
  same level humans agree with each other — but the paper documents
  **position bias**, **verbosity bias**, and **self-enhancement bias**.
- **Self-preference, the basis for our anti-incest rule**:
  Panickssery, A. et al. (2024), *["LLM Evaluators Recognize and Favor
  Their Own Generations"](https://arxiv.org/abs/2404.13076)*, NeurIPS.
  Shows a causal link between an LLM's self-recognition ability and a
  bias toward its own outputs. This is precisely why
  `ensure_anti_incest` refuses to let a judge grade a critic that
  shares its provider.

---

## 4.5 Oracle-free testing: property-based + metamorphic

Everything in §1–§4 needs an **oracle** — a definition of "correct"
(gold standard, or a validated judge). But §3.5 Regime 2/3 is exactly
the case where the oracle is expensive or absent. Software testing hit
this wall decades ago and built two paradigms specifically for it.
They apply directly to LLM/agent eval and are a **third axis**,
orthogonal to operational metrics and judge-precision.

### The key shift: test *relations*, not *answers*

Instead of "is output X correct?" (needs an oracle), ask "do two
**related** runs satisfy a relation we know must hold?" (needs no
oracle). You never need to know the right answer — only how the right
answer must *behave* under a transformation.

### Invariance (property-based) — output must NOT change

Perturb the input in a way that shouldn't matter; the output must stay
equivalent. Borrowed straight from property-based testing (QuickCheck:
"for all inputs, this invariant holds").

| Relation | For an LLM agent |
|---|---|
| paraphrase invariance | reword the prompt → semantically same answer |
| order invariance | reorder independent list items → same findings |
| rename invariance | rename a variable in code → arc's critic flags the *same* bug |
| typo robustness | inject typos → answer unchanged |
| determinism | same input twice at temp 0 → byte-identical output |

A failure here means the agent is **flaky / non-robust** — e.g. "the
critic finds 5 bugs on one run and 3 on a re-run of the same code."
Precision can't catch that; an invariance check catches it with **no
gold standard**.

### Directional (metamorphic) — output must change a KNOWN way

Transform the input so the output *must* move in a predictable
direction, even though you don't know the exact answer.

| Relation | For an LLM agent |
|---|---|
| add a constraint | "...and keep it under 50 words" → output must shrink |
| inject a known bug | plant a real defect → critic must flag *at least* that |
| negate the question | flip a yes/no question → answer should flip |
| strengthen severity | make a vuln clearly worse → severity rating must not drop |

A failure means the agent ignored a change it should have respected —
again caught **without an oracle**.

### Research basis

This isn't ad-hoc. The established work:

- **CheckList** — Ribeiro et al. (2020), *["Beyond Accuracy:
  Behavioral Testing of NLP Models with
  CheckList"](https://aclanthology.org/2020.acl-main.442/)*, ACL best
  paper. Brings software-testing structure to NLP with three test
  types that map exactly onto the above: **MFT** (minimum
  functionality = unit test), **INV** (invariance = property test),
  **DIR** (directional expectation = metamorphic relation).
- **Metamorphic testing for MT (oracle-free)** — He, Meister, Su
  (2020), *["Structure-Invariant Testing for Machine
  Translation"](https://arxiv.org/abs/1907.08710)*, ICSE. Detects
  translation defects *without reference translations* by checking
  that similar source sentences yield structurally similar outputs —
  a pure metamorphic relation. Same group has follow-ups (referential
  transparency, terminology) and the idea generalizes:
  e.g. *MTTM: Metamorphic Testing for Textual Content Moderation*
  (ICSE 2023).
- **Metamorphic testing, origin** — Chen, Cheung, Yiu (1998),
  introduced the technique for the general oracle problem; "new
  visions after a quarter century" (FSE 2021) surveys the field.

### Where this fits tars (proposed)

A metamorphic case is a triple: `(transform, relation, base_input)`.

```rust
// sketch — not yet implemented
struct MetamorphicCase {
    base: ChatRequest,
    transform: fn(&ChatRequest) -> ChatRequest,   // paraphrase / add-constraint / …
    relation: fn(&ChatResponse, &ChatResponse) -> bool, // invariance / directional check
}
```

Run `base` and `transform(base)` through the **same** pipeline, apply
`relation` to the two outputs. No gold standard, no judge required for
the structural relations (an invariance like "same set of finding ids"
is a pure string/set check); a judge is only needed for *semantic*
relations ("are these two answers equivalent?").

This would be a fourth eval mode — `tars eval metamorphic` — sitting
beside corpus replay (§1.3), judge (§1.2), and diff (#2). **Not yet
scoped**; flagged here because it's the principled answer to "how do
you test when you have no ground truth," which §3.5 Regime 2/3 leaves
open.

### The honest caveat

Metamorphic / invariance testing proves the agent is **consistent and
robust** — it does **not** prove the agent is **correct**. A model can
be perfectly invariant under paraphrase and consistently, robustly
*wrong*. So this axis is **complementary** to gold/judge eval, not a
replacement: it catches a bug class (flakiness, ignored constraints,
non-determinism) that precision is blind to, and is blind to a bug
class (systematic wrongness) that precision catches.

---

## 4.6 Two robustness axes with their own research

Two specific oracle-light eval techniques worth calling out, both
asked about directly and both well-studied.

### Axis A — semantic stability under AI-generated paraphrase

The idea: have an LLM **generate semantically-equivalent rewrites** of
each input, run all of them, and measure how much the *outputs* drift.
Low drift = stable; high drift = the model is sensitive to surface form
(a known failure mode). The paraphrase generator and the equivalence
checker can both be LLMs — so this is fully automatable, no gold
standard.

The clean part: you're measuring **consistency**, which needs no oracle
— you never need the right answer, only "did N paraphrases of the same
question produce the same answer?"

Established work:

- **METAL** — *["METAL: Metamorphic Testing Framework for Analyzing
  Large-Language Model Qualities"](https://arxiv.org/abs/2312.06056)*
  (2023). Exactly this: the metamorphic relation is "same-meaning
  prompts → same-semantics outputs," generated via NLP paraphrasing,
  auto-producing hundreds of MRs from templates. Detects flaws by
  detecting *inconsistency* across paraphrases.
- **Metamorphic prompt testing for code** — *["Validating
  LLM-Generated Programs with Metamorphic Prompt
  Testing"](https://arxiv.org/abs/2406.06864)* (2024). Paraphrase the
  spec, generate code from each, cross-check the programs agree —
  catches LLM coding errors without a reference solution.
- **Semantic entropy** — Farquhar, Kossen, Kuhn, Gal (2024),
  *["Detecting hallucinations in large language models using semantic
  entropy"](https://www.nature.com/articles/s41586-024-07421-0)*,
  **Nature**. The measurement technique: sample many generations,
  **cluster them by semantic equivalence** (an NLI model / LLM judges
  meaning, not tokens), then compute entropy *over the semantic
  clusters*. High semantic entropy = the model is unstable / making it
  up. This is the principled "use AI to judge same-semantics, then
  measure stability" answer — published in Nature, so about as
  validated as it gets.

The arc connection: a critic that finds different bugs each run on the
*same code* (or on a paraphrased rubric) has high semantic entropy —
measurable, no gold standard needed.

### Axis B — accuracy / hallucination vs context size and position

The idea: hold the task fixed, **vary how much context you stuff in**
(and *where* the relevant bit sits), measure accuracy. This is exactly
the "does giving arc's critic a bigger file make it miss more bugs?"
question.

The research is unambiguous that **more context is not free**:

- **Lost in the Middle** — Liu et al. (2024),
  *["Lost in the Middle: How Language Models Use Long
  Contexts"](https://arxiv.org/abs/2307.03172)*, TACL. **U-shaped
  performance**: models use info at the *beginning* (primacy) and
  *end* (recency) well, but accuracy degrades sharply when the
  relevant bit is in the **middle** of a long context — even for
  models explicitly built for long context.
- **RULER** — NVIDIA (2024). Only ~half of models claiming 32K+
  context hold up at that length; **effective context windows fall
  far short of advertised**, sometimes by ~99%. Degradation appears
  across 18 LLMs even with clean retrieval, tied to attention
  dilution.
- **Needle-in-a-Haystack (NIAH)** — the now-standard probe: plant a
  known fact at varying depth × context length, measure retrieval.
  The "needle" *is* the oracle, so this gives a clean accuracy curve
  vs. (length, position).

**Direct implication for arc**: arc feeds the critic a whole file. If
the file is large and a real bug sits in the middle, "lost in the
middle" predicts the critic misses it — not because the rubric is
wrong but because of context position. The metamorphic test writes
itself:

> Plant the **same** known bug. Vary (file size, bug position).
> Measure detection rate. A detection curve that sags in the middle /
> collapses past some length is the lost-in-the-middle effect biting
> your critic — and tells you to chunk the file rather than feed it
> whole.

This is the highest-value oracle-free test for arc specifically,
because the needle (planted bug) gives you ground truth for free, and
the failure mode it surfaces (context-position blindness) is one
precision-on-natural-corpus would never isolate.

---

## 5. Honest limitations

The framework addresses some biases and not others:

| Bias / risk | Addressed? |
|---|---|
| **Self-preference** (judge favors own provider) | ✅ `ensure_anti_incest` hard-refuses same-provider judge |
| **Position bias** (judge favors first/last) | ❌ not addressed — single-output judging avoids the A-vs-B ordering question, but a paired-comparison judge would need randomized order |
| **Verbosity bias** (judge favors longer answers) | ❌ not addressed — caller's prompt-engineering problem for now |
| **Single-judge variance** | ⚠️ partially — ensemble judging (priority #4) is the planned mitigation; majority vote across 2-3 judges reduces single-judge noise |
| **Small-n overconfidence** | ✅ Wilson CI + McNemar surface the uncertainty rather than hiding it behind a point estimate |

The honest summary: tars gives you the **right statistical machinery**
for paired offline eval, and encodes the one judge-bias finding
(self-preference) that's a clean runtime rule. The remaining judge
biases are real (per Zheng 2023) and stay the caller's responsibility
— mitigated by good judge prompts and, eventually, ensemble judging.

---

## See also

- [`eval-and-arc-llm-roadmap.md`](./eval-and-arc-llm-roadmap.md) — the eval framework plan + arc_llm collapse
- [`recipes/cost-and-reliability.md`](./recipes/cost-and-reliability.md) — the middleware stack eval runs against
- [`observability.md`](./observability.md) — the event stores eval reads from
