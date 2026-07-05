# Doc 28 — The Bless Store (loadable reference-output assertions)

Status: **implemented (M0–M4, `v0.9.0`)** — `tars_types::bless` (types, JSONPath
subset, exact/normalized/semantic tiers, capture, `check_or_bless`) + `tars eval
bless` + py `bless_check` / node `blessCheck` + a cassette E2E. **Deferred:** the
`eval diff` blessed-drift tier (the standalone `tars eval bless` check mode
already delivers the golden loop). Grounds in the shipped cassette, decode, and
eval subsystems. Companion to [Doc 18 §4.4](18-agent-testing.md) (golden /
approval testing — designed there, given a concrete home here).

## 1. Overview & goal

A **bless** is a committed file of **field-level assertions** about a
*specific, pinned* response. A test *loads* the bless and checks the response
against it; matching → pass, drift → fail (until re-blessed). It is the
**output-side** counterpart to the cassette's **input-side** pin:

```
cassette  →  pins the model's reply   (input side: request → response)   [SHIPPED]
bless     →  pins what matters about that reply  (output side: field asserts)  [THIS DOC]
```

The cassette makes the reply *deterministic*; the bless makes *the parts you
care about* *asserted*. Both are committed files; git is the store; a change
to either is a reviewable diff.

**Why field-level, not whole-output snapshot.** A model reply is noisy prose
around a few load-bearing fields. A byte snapshot (Jest/insta) re-flags every
irrelevant wording change; that's why plain snapshotting fails for LLM output.
A bless asserts *selected fields* (`$.severity == 8`) and leaves the rest free
— the promptfoo/LangSmith "assert on a field" model, but persisted as an
*approved file* (insta/ApprovalTests) rather than inline test code.

**Non-goals.**
- Not a replacement for the cassette (input pin) or `eval diff` (run-vs-run
  comparison). Bless is the per-case *reference* those lean on.
- Not a golden **oracle** of truth — a bless is "an output a human looked at
  and approved" (Doc 18 §4.4), i.e. drift-from-blessed, not correctness.
- **Our layer is JSON.** The bless file is JSON and the v1 selector is JSONPath.
  The selector/codec sits behind a seam (C2) purely as a YAGNI-guard: *if* a
  downstream consumer spoke another wire format (proto, say) they could register
  a codec — but JSON+JSONPath is the only codec tars builds. Proto is a
  reference point, not a plan (see §14, roadmap M5).

## 1a. Related work

The design is the deliberate **union of two established lineages** — file-based
snapshot/approval testing, and field-level LLM-output assertion — plus the one
thing both assume but LLM outputs violate: determinism (which tars already has
via the cassette). Each row states what tars **borrows** and where it **differs**.

| System | Reference form + "bless" mechanic | tars borrows / differs |
|---|---|---|
| **insta** (Rust snapshots) | `.snap.new` pending → `cargo insta review` → accept promotes to committed `.snap`; `INSTA_UPDATE` env | **borrows** the pending-file → review-diff → accept UX and the env-gated update, verbatim (F5, `TARS_BLESS`). **differs**: tars blesses *selected fields*, not the whole serialized value |
| **ApprovalTests** (golden master) | `.received` vs `.approved` files; approve = rename received→approved; the file *is* the golden | **borrows** file-is-the-golden, commit = approval, `.new` never clobbers the approved copy. **differs**: assertions, not a whole-output text blob |
| **Jest** snapshots | `.snap`; `-u` updates; `--ci` refuses to auto-create | **borrows** "CI must not silently create a bless" (FR-6) |
| **promptfoo** | inline `assert: [{type,value}]` — `is-json`, `equals`, `javascript`/`python` for field access; `llm-rubric` for model-graded | **borrows** field-level + tiered (deterministic→model-graded) assertion model. **differs**: tars persists asserts as a **committed file** (promptfoo edits inline test config), and **forbids executable assertions** (no `javascript`/`python` in the file — NFR-4); promptfoo has no native JSONPath (uses JS), tars uses real JSONPath |
| **OpenAI Evals** | YAML registry + JSONL samples w/ `correct_label`; `oaieval` graders | **borrows** expected-value-per-case + graders as a tier ladder. **differs**: per-case file + pinned input, not a central dataset registry |
| **LangSmith** | dataset examples `{input, reference output}`; "golden set" of ~10–20 calibrated cases; offline eval vs reference; pairwise experiments; annotation queue | **borrows** "golden set" of a few calibrated cases and regression-as-don't-degrade. **differs**: git files as the store (not a hosted dataset service), and input pinned by cassette so a code A/B is deterministic |
| **Anthropic** (Claude eval tool / claude-evals) | Console eval cases (manual/CSV import); a 50-case golden dataset; "show evidence, don't assert success" | **borrows** small calibrated golden set + evidence-over-assertion ethos |

**Maturity map** (✅ has · ⚠️ partial · ❌ none), across the axes tars's design
spans. Every column has gaps and only tars's is full — and it is full by
*assembling* proven parts (cassette pin + insta-style approval + promptfoo-style
field asserts), not by inventing a new one:

| Capability | insta / ApprovalTests | promptfoo | LangSmith | OpenAI Evals | VCR-langchain | **tars** |
|---|---|---|---|---|---|---|
| golden = committed file | ✅ | ⚠️ baseline json | ❌ hosted | ✅ jsonl | ✅ cassette | ✅ |
| field-level assert (not whole snapshot) | ❌ | ✅ | ✅ | ✅ | ❌ | ✅ *(design)* |
| tiered match exact→semantic | ⚠️ redaction | ✅ | ✅✅ | ✅ | — | ✅ *(design)* |
| input pin (record/replay) | ❌ | ⚠️ cache | ❌ re-runs | ❌ | ✅✅ | ✅ **shipped** |
| approval loop diff→accept→commit | ✅✅ | ⚠️ | ⚠️ hosted | ❌ | ⚠️ re-record | ✅ *(design)* |
| one file across languages | ❌ | ⚠️ | ⚠️ | ⚠️ | ❌ | ✅ rs/py/ts |
| integrated with run/diff eval | ❌ | ✅ | ✅✅ | ✅ | ❌ | ✅ **shipped** |

Honest read: the **input-pin** idea is well-trodden (vcrpy / pytest-recording /
vcr-langchain / langchain-replay) — tars's cassette is a peer, not a first. The
**approval-file** loop is perfected by insta (deterministic domain). **Field +
semantic** assertion is mature in promptfoo/LangSmith. What is empty is the
*intersection*: pin + field-bless-as-committed-file + approval loop + tiered +
cross-language, in one system. That intersection is the design's contribution.

**The gap none of them close for us.** Snapshot tools (insta/Jest/ApprovalTests)
assume deterministic output — false for an LLM, so byte-snapshots thrash. LLM-eval
tools (promptfoo/LangSmith/OpenAI) assert on fields but re-run the *live* model,
so a "regression" conflates model noise with code change. tars's bless sits on
top of the **cassette** (input pinned → output deterministic) so a field
assertion isolates the *code* — snapshot-testing's ergonomics with an LLM-eval's
field granularity, made deterministic. That composition is the contribution.

Sources: [insta](https://insta.rs/docs/cli/) ·
[ApprovalTests](https://github.com/approvals/ApprovalTests.Python) ·
[promptfoo assertions](https://www.promptfoo.dev/docs/configuration/expected-outputs/) ·
[OpenAI Evals](https://github.com/openai/evals) ·
[LangSmith evaluation](https://docs.langchain.com/langsmith/evaluation-concepts) ·
[Anthropic — demystifying evals](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents).

## 2. CUJs

- **CUJ-1 — Freeze a bless.** A dev has a pinned reply (cassette recorded) and
  wants to lock its load-bearing fields. They run bless-in-record mode; the
  system writes a `*.bless.json` capturing the selected fields' current values;
  the dev reviews the git diff and commits. → a committed reference file.
- **CUJ-2 — Load a bless in a test → pass.** A test replays the cassette,
  decodes the reply, **loads the bless file**, and checks. All assertions hold
  → the test passes, offline, in CI. *(This is the journey the user named.)*
- **CUJ-3 — Drift → fail with the delta.** Code (or model) changes a blessed
  field. The test loads the bless, the check fails, and the failure names the
  selector, the blessed value, and the actual value. → red until resolved.
- **CUJ-4 — Re-bless an intended change.** The drift was intended. The dev
  re-runs bless-in-record mode (or `tars eval bless <run>`); the bless file is
  rewritten; the git diff shows exactly which fields moved; they commit. → the
  new values become the reference; the test goes green.
- **CUJ-5 — Bless a whole eval run.** After `tars eval run`, a dev blesses each
  case's output in one command; future runs `eval diff` against the blessed
  references and flag per-case drift (the missing Doc 18 §4.4 loop).

## 3. Feature list

| # | Feature | CUJ |
|---|---|---|
| F1 | Bless file format — a codec-tagged list of `{selector, expected, match}` assertions | 1,2,3 |
| F2 | Selector engine seam — resolve a path against a decoded value; JSON/JSONPath impl | 2,3 |
| F3 | Tiered matcher — exact / normalized / semantic(judge) per assertion | 2,3 |
| F4 | `Bless::load(path)` + `check(&value) -> BlessOutcome` (native rs API) | 2,3 |
| F5 | Bless record/re-bless — write `*.bless.json` from a value; `.bless.new` pending + accept | 1,4 |
| F6 | `tars eval bless` subcommand + `TARS_BLESS=1` in-test update flag | 1,4,5 |
| F7 | py/ts binding — `load_bless(path).check(dict)` over the completion result | 2,3 |
| F8 | `eval diff` integration — per-case blessed-drift tier | 5 |

## 4. Requirements

**Functional**

| FR | Requirement | Feature |
|---|---|---|
| FR-1 | A bless file is committed JSON: `{codec, source_fingerprint?, asserts:[{selector, expected, match}]}` | F1 |
| FR-2 | `check(value)` resolves every selector against the decoded value and returns per-assertion pass/fail + the (expected, actual) pair | F2,F4 |
| FR-3 | A missing selector (path not present) is a **fail with a typed reason**, never a silent pass | F2 |
| FR-4 | `match` ∈ {`exact`, `normalized`, `semantic`}; default `exact`; `semantic` routes through a judge | F3 |
| FR-5 | Record mode writes a `.bless.new` next to the target; accept promotes it to `.bless.json` (never overwrites in place) | F5 |
| FR-6 | Under CI (`--ci` / `TARS_BLESS` unset), a missing bless file is an **error**, not an auto-create | F5,F6 |
| FR-7 | The native API is codec-agnostic; the JSON/JSONPath codec is one registered impl | F2 |
| FR-8 | py/ts expose load + check over the `complete()` result's text | F7 |

**Non-functional**

| NFR | Requirement | Feature |
|---|---|---|
| NFR-1 (perf) | `check` is pure/CPU-only for exact+normalized (no I/O, no model) — sub-ms per case | F3,F4 |
| NFR-2 (reliability) | drift and missing-bless are loud signals (mirror cassette MISS); no silent overwrite of a bless | F5,F6 |
| NFR-3 (review) | a bless is a stable, diff-friendly file (sorted keys, canonical formatting) so the git diff is the review surface | F1,F5 |
| NFR-4 (security) | selector eval is read-only over an already-parsed value; no code-exec assertions in the file format (unlike promptfoo `javascript`) | F2 |
| NFR-5 (scale) | one bless file per case, content-addressed; N cases = N committed files, no shared mutable state | F1 |

## 5. Infra

| Infra | New/exists | Note |
|---|---|---|
| Committed bless files under a test-owned path (e.g. `<crate>/tests/bless/` or `examples/bless/`) | new dir | git-tracked; **not** under `benchmarks/runs/` (gitignored scratch) |
| Cassette provider (input pin) | exists | `crates/tars-provider/src/backends/cassette.rs` |
| Decode seam (text → `serde_json::Value` / typed `T`) | exists | `crates/tars-types/src/json_decode.rs` |
| JSONPath engine | **new dep** | recommend `serde_json_path` (RFC 9535). No JSONPath dep in the workspace today; Doc 15's `json_path` validators are design-only (`crates/tars-pipeline/src/validation/builtin.rs:7`) |
| Judge (semantic tier) | exists | `tars_types::JudgeVerdict` `crates/tars-types/src/judge.rs:43`; run via a provider (Doc 18 §7) |
| proto codec | *reference only, not planned* | our layer is JSON; the `Codec` seam (C2) would *allow* a proto field-path impl if a consumer ever needed one, but tars ships JSON only (§14) |

## 6. Components

### C1 — `Bless` value type + file (`tars-types`, new `bless.rs`)

Responsibility: the on-disk format + load/save + `check`.

Reuses: `serde` + `serde_json` (workspace); `write_pretty_json` pattern from
`crates/tars-cli/src/eval.rs:62` (sorted, pretty, diff-friendly). New type.

```rust
// tars-types/src/bless.rs  (NEW)
#[derive(Serialize, Deserialize)]
pub struct Bless {
    pub codec: Codec,                    // Json (v1) | Proto (future)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,   // the cassette fp this blesses (provenance)
    pub asserts: Vec<Assert>,
}
#[derive(Serialize, Deserialize)]
pub struct Assert {
    pub selector: String,                // "$.severity"  (JSONPath for Json codec)
    pub expected: serde_json::Value,     // blessed value
    #[serde(default)]
    pub match_: MatchTier,               // exact | normalized | semantic
}
#[derive(Serialize, Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MatchTier { #[default] Exact, Normalized, Semantic }

impl Bless {
    pub fn load(path: &Path) -> Result<Self, BlessError>;
    pub fn save(&self, path: &Path) -> Result<(), BlessError>;      // writes .bless.new
    pub fn check(&self, value: &serde_json::Value) -> BlessOutcome; // exact/normalized only
    pub async fn check_with_judge(&self, value: &serde_json::Value, judge: &dyn LlmProvider)
        -> BlessOutcome;                                            // resolves semantic tier
}
pub struct BlessOutcome { pub drifts: Vec<Drift> }   // empty = pass
pub struct Drift { pub selector: String, pub expected: Value, pub actual: Option<Value>, pub tier: MatchTier }
```

### C2 — Selector `Codec` seam (`tars-types`)

Responsibility: resolve a selector against a decoded value. **tars builds the
JSON impl only** — the trait exists as a one-method YAGNI-guard so the matcher
(C3) and file format (C1) never bake in JSON assumptions. It is *not* a promise
to ship other codecs; a proto/field-path impl is a hypothetical a downstream
consumer could add, not tars work.

Reuses: nothing (new trait). JSON impl wraps `serde_json_path` (new dep).

```rust
pub trait Codec {                        // NEW — one method, keeps C1/C3 codec-neutral
    fn resolve<'v>(&self, value: &'v serde_json::Value, selector: &str)
        -> Result<Option<&'v serde_json::Value>, BlessError>;
}
pub struct JsonPathCodec;                // the ONLY codec tars ships — serde_json_path
// (a proto field-path impl is possible but out of scope; see §14)
```

### C3 — Matcher (`tars-types`)

Responsibility: compare expected vs actual at a tier.
- `Exact`: `serde_json::Value` equality.
- `Normalized`: canonicalize (trim/whitespace-collapse strings, normalize
  number reprs) then equality — reuses the numeric-canonicalization idea from
  `clamp_ints_in_place` in `json_decode.rs:340`.
- `Semantic`: route (expected, actual) to a judge → `JudgeVerdict`
  (`crates/tars-types/src/judge.rs:43`); reuse the anti-incest + provider judge
  from `run_judge` (`crates/tars-cli/src/eval.rs:695`).

### C4 — Bless CLI (`tars-cli`, extend `eval.rs`)

Responsibility: `tars eval bless <run> [--select <path>...]` — read a run's
per-case `output.txt`, decode, capture selected fields, write `.bless.json`
per case. `eval diff` gains a blessed-drift tier.

Reuses: `load_manifest` (`eval.rs:812`), per-case `output.txt` reader
(`eval.rs:735`), `write_pretty_json` (`eval.rs:62`), `decode_json`
(`json_decode.rs:208`), `run_diff` (`eval.rs:912`) for the diff hook.

### C5 — Binding surface (`tars-py`, `tars-node`)

Responsibility: `load_bless(path).check(result_text) -> outcome` so a py/ts
test blesses the completion result. Reuses the `Bless` type across FFI (JSON in,
outcome out). Sits beside `Pipeline.complete` (`tars-py/src/lib.rs:468`,
`tars-node` `CompleteResult`).

## 7. Interfaces with other modules

- **← cassette**: bless consumes the *replayed* `ChatResponse.text`
  (`crates/tars-types/src/response.rs`, via `CassetteProvider::replay_from_file`
  `cassette.rs:169`). Provenance link: `Bless.source_fingerprint` =
  `request_fingerprint(req)` (`cassette.rs:67`).
- **← decode**: bless checks the *decoded* value; the test calls
  `decode_json::<serde_json::Value>(text, mode)` (`json_decode.rs:208`) or
  `resp.json::<T>(mode)` (`json_decode.rs:420`) first.
- **→ eval**: `EvalCaseReport` (`eval.rs:455`) gains an optional `bless: BlessOutcome`;
  `run_diff` (`eval.rs:912`) adds a "blessed drift" line beside the check tier.
- **→ judge**: semantic tier calls a judge provider → `mcnemar`/`JudgeVerdict`
  (`judge.rs:204`,`:43`) for aggregate significance across cases.

## 8. Main algorithms

**check(value) — the load-bless assertion (CUJ-2/3):**
```
outcome.drifts = []
for a in bless.asserts:
    actual = codec.resolve(value, a.selector)     # None if path absent
    if actual is None:                            # FR-3 missing = drift, loud
        drifts += Drift{a.selector, a.expected, None, a.match}; continue
    ok = match a.match:
            Exact      -> actual == a.expected
            Normalized -> canon(actual) == canon(a.expected)
            Semantic   -> DEFER (needs judge; check_with_judge resolves)
    if not ok: drifts += Drift{a.selector, a.expected, Some(actual), a.match}
return BlessOutcome{drifts}     # empty ⇒ pass
```
Invariant: `check` is pure for exact/normalized (NFR-1). Semantic assertions in
a plain `check` are reported as "unresolved" unless `check_with_judge` is used —
never silently passed.

**record / re-bless (CUJ-1/4), mirrors insta:**
```
value = decode(pinned_reply)
new = Bless{codec, source_fingerprint, asserts: [for sel in selectors:
        Assert{sel, expected: codec.resolve(value, sel), match: Exact}]}
write new to  <path>.bless.new                 # never clobber <path>.bless.json
if TARS_BLESS=1 / `eval bless --accept`: rename .bless.new -> .bless.json
else: leave .bless.new for the human to review + accept (git diff is the review)
```
Edge cases: CI with no bless file → error (FR-6); a selector that resolves to
nothing at record time → refuse to bless that selector (don't freeze an absent
field, echoing the CLAUDE.md "don't freeze a false invariant" rule).

## 9. Integration / E2E tests

- **E2E-1 (CUJ-2)**: commit a cassette + a `severity.bless.json` (`$.severity`
  exact 8). Test: replay cassette → decode → `Bless::load` → `check` → drifts
  empty → pass. Offline. *Extends the shipped `crates/tars-provider/tests/ab_cassette.rs`.*
- **E2E-2 (CUJ-3)**: same bless, but a transform mutates severity → `check`
  returns one `Drift{selector:"$.severity", expected:8, actual:9}` → test asserts
  the drift is reported with both values.
- **E2E-3 (CUJ-4)**: `TARS_BLESS=1` re-runs record → `.bless.json` rewritten to 9
  → git diff shows `8 → 9` → re-run without flag → pass.
- **E2E-4 (CUJ-1/5)**: `tars eval run` (cassette provider) → `tars eval bless <run>
  --select '$.severity'` writes per-case bless → second `eval run` + `eval diff`
  shows zero blessed-drift; mutate code → drift flagged per case.
- **E2E-5 (FR-3)**: bless a selector, feed a value missing that field → drift
  (missing), not a pass.
- **E2E-6 (CUJ-2, bindings)**: py `test_ab_cassette.py` + node `ab_cassette.test.mjs`
  load a bless and check the replayed result — proving the file is binding-portable.

## 10. Success criteria

- All six E2E pass offline in CI (no live model), via committed cassette+bless.
- A blessed field's drift fails the test naming (selector, expected, actual).
- Re-bless is a single command producing a reviewable git diff; no in-place
  clobber (a `.bless.new` always precedes acceptance).
- One bless file, byte-identical, is loaded+checked from rs, py, and ts.
- Missing bless under CI errors; missing selector drifts. No silent pass path
  exists (grep: no code path returns pass on a resolve-None or unresolved-semantic).

## 11. Performance considerations

Exact/normalized `check` is CPU-only over an already-parsed `Value` — target
sub-ms/case; N cases scale linearly, no I/O beyond the one file read. Semantic
tier costs one judge call/assertion — gate it behind opt-in `match: semantic`
and batch per run. Selector compilation (`serde_json_path`) is cached per bless
load.

## 12. Reliability considerations

- **Fail-closed** (NFR-2): resolve-None → drift; unresolved-semantic → not-pass;
  missing bless under CI → error. Mirrors cassette MISS-is-a-signal.
- **No in-place overwrite**: record always writes `.bless.new`; acceptance is an
  explicit rename (insta pattern) → a crashed record can't corrupt the committed
  bless.
- **Idempotent record**: re-blessing an unchanged output yields a byte-identical
  file (canonical formatting, NFR-3) → empty git diff, no churn.
- **Provenance**: `source_fingerprint` ties a bless to the cassette entry it was
  taken from; a bless whose fingerprint no longer exists in the cassette is
  flagged (stale bless) rather than silently checked against a different reply.

## 13. Security considerations

- Bless files are **declarative data only** — selector + expected + tier. No
  executable assertions (deliberately unlike promptfoo `javascript`/`python`),
  so loading a bless never runs code (NFR-4).
- Selector evaluation is read-only over an already-parsed value; JSONPath is
  bounded (no recursion-bomb: cap descendant scans, mirror the cassette scraper's
  bounded-scan discipline in `json_decode.rs:195`).
- Semantic tier sends (expected, actual) to a judge provider → same trust
  boundary + anti-incest rule as `eval judge` (`eval.rs:695`); redact per the
  existing event-body policy before logging.

## 14. Abstraction & reuse

**The abstraction.** A bless is `(codec, [assert])` where an assert is
`(selector, expected, tier)`. Two seams keep it honest and future-proof:
1. **`Codec`** (C2) decouples the *selector language + value shape* from the
   matcher. **tars's layer is JSON** (JSONPath over `serde_json::Value`); the
   seam exists only so C1/C3 don't hard-code that. A consumer on another wire
   format (e.g. proto + field-path) *could* register a codec, but that is a
   reference point, not scoped tars work — we ship JSON.
2. **`MatchTier`** decouples *how equal* from *what to compare* — the Doc 18 §4.4
   exact/normalized/semantic ladder, reusing the existing judge + stats.

**Reuse map (Phase 0).**

| Symbol | file:line | How bless uses it |
|---|---|---|
| `CassetteProvider::replay_from_file` | `crates/tars-provider/src/backends/cassette.rs:169` | pins the reply bless asserts over |
| `request_fingerprint` | `crates/tars-provider/src/backends/cassette.rs:67` | `Bless.source_fingerprint` provenance |
| `decode_json` / `decode` / `ChatResponse::json` | `crates/tars-types/src/json_decode.rs:208`/`225`/`420` | text → `Value`/`T` before check |
| `clamp_ints_in_place` (canonicalization idea) | `crates/tars-types/src/json_decode.rs:340` | number normalization in `Normalized` tier |
| `write_pretty_json` | `crates/tars-cli/src/eval.rs:62` | diff-friendly bless file writer |
| `EvalCaseReport` / `load_manifest` / per-case `output.txt` | `crates/tars-cli/src/eval.rs:455`/`812`/`735` | `eval bless` reads run outputs |
| `run_diff` | `crates/tars-cli/src/eval.rs:912` | blessed-drift tier hook |
| `JudgeVerdict` / `mcnemar` / `run_judge` | `crates/tars-types/src/judge.rs:43`/`204`, `eval.rs:695` | semantic tier + significance |
| `JsonSchema::{strict,loose}` | `crates/tars-types/src/schema.rs:41`/`49` | (adjacent) schema-shape check vs. field bless |
| shipped cassette tests | `crates/tars-provider/tests/ab_cassette.rs`, `crates/tars-py/python/tests/test_ab_cassette.py`, `crates/tars-node/__test__/ab_cassette.test.mjs` | E2E hosts to extend with load-bless |

**Genuinely new**: `Bless`/`Assert`/`MatchTier`/`BlessOutcome` types, the
`Codec` trait + `JsonPathCodec`, the `serde_json_path` dep, `tars eval bless`,
and the `load_bless` binding methods. Everything else composes shipped code.

## Roadmap

- **M0 — bless core (rs), exact tier.** C1 + C2(`JsonPathCodec`) + C3(exact) +
  `serde_json_path` dep. Deliver FR-1,2,3,4,7. Verify: **E2E-1, E2E-2, E2E-5**.
  Hardest-first: the `Codec` seam + selector semantics land here.
- **M1 — record / re-bless.** F5 + `TARS_BLESS` + `.bless.new`/accept. Deliver
  FR-5,6. Verify: **E2E-3**. Risk-up-front: the no-clobber accept flow.
- **M2 — normalized + semantic tiers.** C3 normalized + `check_with_judge`.
  Deliver FR-4. Verify: extend E2E with a normalized + a semantic assert.
- **M3 — eval integration.** C4 `tars eval bless` + `eval diff` drift tier +
  `EvalCaseReport.bless`. Deliver F6,F8. Verify: **E2E-4**.
- **M4 — bindings.** C5 py/ts `load_bless().check()`. Deliver FR-8. Verify:
  **E2E-6** (py + node load the same bless file).
tars's scope ends at M4 — **JSON is the layer**, and M0–M4 deliver the full
JSON bless. (A proto `Codec` is a reference point in §14, not a milestone; the
seam makes it *possible* for a consumer, but tars does not plan to build it.)

Each milestone is independently shippable and has a named E2E gate. M0→M1 is the
minimal "load a bless, make a test pass, re-bless a change" loop (CUJ-2→4).
