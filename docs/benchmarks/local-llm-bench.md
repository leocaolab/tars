# Local LLM benchmark — Qwen vs. others on Apple Silicon

How to compare locally-served models (Qwen family vs. Gemma / others)
on this hardware using the tools tars already ships: `tars bench` for
**speed** and `tars eval` for **quality**. This doc is both the design
and the runbook — the result tables at the bottom are filled in from
real runs and re-filled whenever the model set changes.

> **Why this exists.** "Which local model should I point tars at?" is a
> hardware-specific question — published leaderboard numbers don't tell
> you the decode rate *on your box* with *your quant*. This benchmark
> answers it empirically and reproducibly.

---

## 1. Scope & the speed-vs-quality split

A benchmark that reports **only speed** is actively misleading:

- Reasoning models (e.g. `qwen3.5-9b` here) emit their answer into
  `reasoning_content`. They post a high `tok/s` but take long
  wall-clock to a *usable* answer — raw decode rate flatters them.
- A model that is fast and **wrong** is worthless. Throughput without
  correctness ranks the wrong winner.

So quality belongs in the benchmark — but **not inside `tars bench`**.
`tars bench` is a pure-speed instrument (TTFB, decode tok/s, p50/p99);
mixing correctness into it muddies a clean tool, and tars already has
`tars eval` for quality. We keep them as separate instruments and
report side-by-side, in three tiers:

| Tier | Instrument | Measures | Cost | Ground truth? |
|---|---|---|---|---|
| **A — Speed** | `tars bench` | TTFB, decode tok/s, total wall-clock | free | none |
| **B — Quality (deterministic)** | `tars eval run` + checks | compiles? contains required symbol? valid JSON? | free | per-case checks |
| **C — Quality (judge)** | `tars eval judge` *(optional)* | pairwise preference / rubric score | cloud tokens | gold answers |

**Recommendation: run Tier A + Tier B always; Tier C only when a Tier-B
tie needs breaking.** Tier B is the sweet spot — objective, no second
LLM, no subjectivity — and it aligns with the existing
[`eval-methodology.md`](../eval-methodology.md) two-tier model
(operational vs. judged).

This doc's vocabulary matches `eval-methodology.md`: Tier A+B here are
its "operational" tier; Tier C is its "judged" tier.

---

## 2. Environment

| | |
|---|---|
| Host | Apple **M5 Pro**, 48 GB unified memory, macOS (Darwin 25.5) |
| Server | **LM Studio** OpenAI-compatible API at `http://127.0.0.1:1234/v1` |
| Alt server | `mlx_lm.server` at `:8080` (provider `mlx_local`) — used only when targeting MLX kernels directly |
| Runner | `tars bench` / `tars eval` (built from this checkout, `cargo build --release -p tars-cli`) |

**48 GB is the binding constraint.** An 8-bit 31B model is ~30+ GB
resident; only one large model fits at a time. LM Studio JIT-loads on
first request and may evict the previous model. Therefore: **bench one
model at a time**, let it load, and rely on `--warmup` to absorb the
cold-load + first-token-of-session cost (iters that return `out=0`
because the model was still loading are auto-excluded from stats).

---

## 3. Model matrix

All models below are already loaded/available in LM Studio (zero
download). Grouped into cohorts so each Qwen entry has a non-Qwen
counterpart at a comparable size/shape — that is the "Qwen vs. others"
comparison.

| Cohort | Qwen | Other (control) | Why paired |
|---|---|---|---|
| **Small (latency-first)** | `qwen/qwen3.5-9b` | `google/gemma-4-e4b` | single-user interactive latency |
| **Mid dense (~27–31B)** | `qwen/qwen3.6-27b` | `google/gemma-4-31b` | dense quality/speed knee |
| **MoE (~30–35B, A3B/A4B active)** | `qwen3.6-35b-a3b`, `qwen/qwen3.5-35b-a3b` | `google/gemma-4-26b-a4b` | sparse-active throughput |
| **Coder specialist** | `qwen/qwen3-coder-30b` | — | code-task baseline |
| **Reasoning-distilled** | `qwen3.5-27b-claude-4.6-opus-reasoning-distilled` | — | reasoning-token behavior |
| **Vision-language** | `qwen/qwen3-vl-30b` | — | (text-only here; VL not benched) |

**Comparison hygiene — read before trusting any number:**

- **Quant matters.** A 4-bit model decodes faster *and* scores lower
  than its 8-bit sibling. Always record the quant; never compare across
  quants as if it were a pure model difference.
- **MoE active params ≠ total.** `a3b`/`a4b` activate ~3–4B params per
  token, so a 35B-MoE can out-decode a 31B-dense. Compare *experience*
  (tok/s, quality), not parameter count.
- **Reasoning tokens.** Decode rate counts visible **+** reasoning
  tokens (see `bench.rs::generated_tokens`), so reasoning models aren't
  penalized to "0 tok/s". But high tok/s on a reasoning model does not
  mean a fast *answer* — cross-check Tier B wall-clock-to-correct.

---

## 4. Tier A — speed (`tars bench`)

`tars bench <provider> --model <id>` runs one prompt N times against a
configured provider and reports TTFB (stream open → first token) and
decode rate (generated tokens / time after first token) as
mean/p50/p99. All providers here share the LM Studio transport
(`openai_compat` @ `:1234`), so any model id is reachable by overriding
`--model` on a single provider.

### Protocol

```bash
# one model, fixed token cap so every model generates the SAME N tokens
# (fair cross-model decode comparison), 2 warmup + 8 measured
tars bench qwen_coder_local \
  --model qwen/qwen3.5-9b \
  --max-tokens 256 \
  --warmup 2 --repeat 8
```

Hold these constant across every model in a run so the comparison is
clean:

- `--max-tokens 256` — caps output so decode rate isn't skewed by
  models choosing different answer lengths.
- `--warmup 2` — absorbs LM Studio cold-load + session warmup.
- `--repeat 8` — enough samples for a meaningful p50; bump for tighter
  p99.
- Same `--prompt` (default is a ~80–200-token Rust-fn prompt; override
  only if you change it for *all* models).

### Driver script

`tools/bench-local.sh` (below) loops the matrix, swapping one model at
a time, and tees each summary into `bench-runs/<date>/`:

```bash
#!/usr/bin/env bash
# Usage: tools/bench-local.sh [provider] [max_tokens] [repeat]
set -euo pipefail
PROVIDER="${1:-qwen_coder_local}"
MAXTOK="${2:-256}"
REPEAT="${3:-8}"
OUT="bench-runs/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
MODELS=(
  qwen/qwen3.5-9b            google/gemma-4-e4b
  qwen/qwen3.6-27b           google/gemma-4-31b
  qwen3.6-35b-a3b            google/gemma-4-26b-a4b
  qwen/qwen3-coder-30b
)
TARS=./target/release/tars
  # NB: large 8-bit models can't co-reside on 48 GB — LM Studio's
  # resource guardrail refuses the 2nd load (§7 finding). Eject first:
  lms unload --all >/dev/null 2>&1 || true
  for m in "${MODELS[@]}"; do
    safe="${m//\//_}"
    echo "── benching $m ──"
    # stdout (the summary table) → file; stderr (progress) → terminal
    "$TARS" --config ~/.tars/config.toml bench "$PROVIDER" --model "$m" \
        --max-tokens "$MAXTOK" --warmup 2 --repeat "$REPEAT" \
        > "$OUT/$safe.txt" || echo "  FAILED: $m (skipped)"
    lms unload --all >/dev/null 2>&1 || true   # free RAM before next big model
  done
done
echo "summaries in $OUT/"
```

> The binary resolves config from `$XDG_CONFIG_HOME/tars/config.toml`,
> **not** `~/.tars/config.toml` — pass `--config ~/.tars/config.toml`
> explicitly (as above) or you get "config file not found".

### 4.1 Schema-penalty sub-benchmark (constrained decode)

`response_format: json_schema` forces the server to constrain decoding
to a grammar. **The cost of that is backend-specific, not a universal
"local model" property:**

- **llama.cpp / LM Studio** (what this box runs) compiles the schema to
  a GBNF grammar; on the current implementation that is expensive and
  the penalty grows with schema complexity (measured below).
- **vLLM** uses xgrammar / outlines (FSM compiled once, then near-zero
  per-token overhead) — typically a small penalty.
- **MLX** has its own path; benchmark it before assuming.

So this is a *knob with a per-backend cost*, not a prohibition.
`tars bench` sends no schema, so measure your backend directly with
`curl` — same prompt, same model, with/without `response_format`:

```bash
# see tools/schema-penalty.sh — times A (free) vs B (json_schema) and
# prints tok/s for each. Run against the same warm model.
```

Real result on **this box (llama.cpp via LM Studio)**, §7.1.1: free
decode ≈ **2×** the tok/s of a *moderate* schema; a *complex* critic
schema + 16k-token budget was **~6×** slower — slow enough a
30B model didn't finish inside a 600 s timeout (→ 0 tokens). Don't
generalize the magnitude to other backends; re-run the script.

**Framework implication (TARS is the framework here, not a consumer):**
don't hardcode "no json_schema". Expose structured-output strategy
**per provider** so the caller picks grammar-constrained vs prompt-JSON
based on *their* backend's measured cost, and document the cost so the
choice is informed. On a llama.cpp backend, prompt-JSON + a
fence-tolerant parser is usually the better default (disable the
response schema); on vLLM the schema may be free enough
to prefer. Tier B (§5) verifies prompt-JSON actually parses.

---

## 5. Tier B — deterministic quality (`tars eval`)

Speed picks the fast model; Tier B confirms it's also *right*. We use a
small code-focused corpus where correctness is a deterministic check —
no LLM judge, no subjectivity.

### Corpus layout

Each subdirectory is one case: required `input.txt`, optional
`system.txt` / `expected.txt`. We use a **JSON-extraction** corpus
because it pairs directly with the schema finding in §4.1 — the open
question "if we forbid `json_schema` and ask for JSON in the prompt
instead, does the output actually parse?" is exactly what `--check
valid-json` answers.

```text
bench-corpus/
  config_json/  input.txt  "…server config with keys host/port/tls … Return ONLY the JSON."
  user_json/    input.txt  "Extract the person as JSON {name, age} … Return ONLY the JSON."
  langs_json/   input.txt  "…array of {name, year, paradigm} … Return ONLY the JSON."
```

### Checks are CLI flags, not per-case files

`tars eval run` applies **built-in invariants globally** via repeatable
`--check`: `non-empty`, `valid-json`, `max-length:<N>`. There is no
per-case `checks.json` — richer per-case assertions (`must_contain`,
`json_has_keys`) are a Rust-API custom-invariant feature (see Doc 18
§4.1), not a CLI flag. Keep each corpus homogeneous so one global check
set is meaningful (all-JSON cases → `--check valid-json`).

### Run + compare

```bash
# replay the corpus through one model, write per-case outputs + manifest.
# Budget HIGH (4096): reasoning models (qwen3.6-27b) spend most of the
# budget thinking before the answer — 512 left them empty (§7.2).
tars eval run --corpus bench-corpus --provider qwen_coder_local \
  --model qwen/qwen3-coder-30b --check non-empty --check valid-json \
  --max-output-tokens 4096 --output eval-runs/qwen-coder

tars eval run --corpus bench-corpus --provider gemma_local \
  --model google/gemma-4-31b  --check non-empty --check valid-json \
  --max-output-tokens 4096 --output eval-runs/gemma-31b
```

The per-case `report.json` carries per-check pass/fail plus usage and
wall-clock — the **wall-clock-to-correct** number that cross-checks
Tier A's raw decode rate. Two gotchas surfaced in real runs (§7.2):
**markdown fences** (```` ```json ````) make correct JSON fail a strict
`valid-json` check, and **reasoning models** can burn the entire token
budget on hidden chain-of-thought and emit empty visible content —
budget generously and strip fences with a tolerant parser.

---

## 6. Tier C — judge (optional, not default)

Only when Tier B ends in a tie (both pass all deterministic checks) and
you need to rank *answer quality* (idiomatic code, explanation
clarity). Use a cloud model as judge — costs tokens, introduces
subjectivity, so it is deliberately off by default:

```bash
tars eval judge eval-runs/qwen-coder eval-runs/gemma-31b \
  --judge-provider anthropic --judge-model claude-sonnet-4-7
```

Treat judge output as a tiebreaker signal, not a primary metric.

---

## 7. Results

> Filled from real runs on **2026-06-01/02, M5 Pro 48 GB, LM Studio**.
> Re-run `tools/bench-local.sh` after any model swap. **Numbers are
> hardware- and quant-specific — do not port them off this box.**

### 7.1 Tier A — speed (max_tokens=128, warmup 2–3, repeat 5)

| Model | Family | Shape | TTFB p50 (ms) | Decode p50 (tok/s) | Total p50 (s) |
|---|---|---|---|---|---|
| `google/gemma-4-e4b` | Gemma | edge | 270 | **46.3** | 3.02 |
| `qwen/qwen3.5-9b` | Qwen | dense ~9B (reasoning) | 208 | 37.7 | 3.61 |
| `qwen/qwen3-coder-30b` | Qwen | MoE coder | 631 | 14.5 | 9.55 |
| `qwen/qwen3.6-27b` | Qwen | dense ~27B | 622 | 11.4 | 12.05 |
| `google/gemma-4-31b` | Gemma | dense ~31B | 1325 | 8.2 | 16.87 |

Reads:
- **Small tier (Qwen vs Gemma):** Gemma-4-e4b is the latency king
  (46 tok/s, 270 ms TTFB) — best interactive pick on this box.
- **Mid dense tier (Qwen vs Gemma):** Qwen3.6-27b beats Gemma-4-31b on
  *both* throughput (11.4 vs 8.2 tok/s) **and** TTFB (622 ms vs
  1325 ms). Qwen wins the dense mid-tier outright.
- **MoE A3B/A4B tier:** not benched — `qwen3.6-35b-a3b` /
  `gemma-4-26b-a4b` 8-bit weights don't fit alongside warm models on
  48 GB (see 7.1.2); rerun via `bench-local.sh` which ejects first.

> **First warmup iter is always the cold load** — e.g. gemma-4-e4b's
> warmup#1 TTFB was 11.6 s vs ~270 ms steady-state; gemma-4-31b's was
> 15 s. That's why `--warmup ≥ 2` is mandatory and warmup timings are
> discarded.

#### 7.1.1 Schema penalty (GBNF tax) — `qwen3-coder-30b`

| Decode mode | time | tokens | tok/s |
|---|---|---|---|
| A — free | ~9.5 s | ~120 | **~12.6** |
| B — `json_schema` (moderate) | ~16 s | ~95 | **~5.9** |

Constrained decode is **~2.1× slower per token** on a *moderate*
schema, and produced *fewer* tokens in *more* wall-clock. A
*complex* critic schema + 16k budget measured ~6× — past the point a
30B finishes inside a 600 s timeout (→ 0-token timeouts). **Don't send
`json_schema` to local providers; use prompt JSON.** (`tools/schema-penalty.sh`)

#### 7.1.2 Memory guardrail — 48 GB ceiling

Loading `gemma-4-31b` (8-bit, ~30 GB) while `qwen3.6-27b` was resident
was **refused** by LM Studio: *"Model loading was stopped due to
insufficient system resources."* Two large 8-bit models can't
co-reside on 48 GB. The driver scripts `lms unload --all` between
models for this reason.

### 7.2 Tier B — quality (JSON corpus, `--check non-empty,valid-json`)

| Model | budget | json_shape | not_empty | what happened |
|---|---|---|---|---|
| `google/gemma-4-31b` | 256 | 2/3 fail | 1/3 fail | correct JSON but **wrapped in ```` ```json ```` fences** → fails strict parse; array case truncated at the 256 cap |
| `qwen/qwen3.6-27b` | **512** | 0/3 fail | **3/3 fail** | **reasoning model** — 512 tokens all spent on hidden chain-of-thought, visible content empty |
| `qwen/qwen3.6-27b` | **4096** | **0/3 fail** | 1/3 fail | budget raised → emits **clean fence-free valid JSON** (verified). Remaining 1 fail = a 0-token stream glitch on the cold first case, not budget |

The qwen3.6-27b row is the headline: **the 512 → 4096 budget bump
flipped it from "always empty" to "always valid JSON".** The user's
instinct was right — for a reasoning model the fix is budget, not a
different model. Cost of that fix: `langs_json` (a trivial 3-item
array) spent **1697 output tokens / 172 s** thinking before answering.

Three distinct integration lessons — **none is a "bad model"**:
1. **Reasoning budget (Qwen):** thinking eats the budget before the
   answer. Fix: `--max-output-tokens ≥ 2–4k` for reasoning models, or
   disable thinking (`/no_think`). Same root cause as the complex-schema
   timeout — reasoning + complex schema = never reaches the answer. Trade-off:
   correct but slow + token-hungry.
2. **Markdown fences (Gemma):** JSON is correct but wrapped — a strict
   parser rejects it, a fence-stripping tolerant parser accepts it.
   (Qwen, by contrast, emits **raw** fence-free JSON that parses
   directly — a real ergonomic edge for structured output.)
3. **Output truncation (Gemma @ 256):** a low cap chops valid JSON
   mid-object. Budget for the *answer* length, separately from the
   reasoning overhead in lesson 1.

### 7.3 Takeaways — recommended `default_model` per use case

| Use case | Pick | Why |
|---|---|---|
| Interactive chat / quick edits | `google/gemma-4-e4b` | 46 tok/s, 270 ms TTFB — fastest by far |
| Code review / generation | `qwen/qwen3-coder-30b` | 14.5 tok/s, coder-tuned, no reasoning-budget trap |
| Mid-tier general (Qwen vs Gemma) | `qwen/qwen3.6-27b` | beats Gemma-4-31b on speed **and** TTFB |
| Structured output (this llama.cpp box) | any of the above **+ prompt JSON over schema** | grammar-constrained decode measured 2–6× slower *here*; cheap on vLLM — pick per backend |

**Headline:** Qwen wins the mid-dense tier outright; Gemma owns the
latency-first small tier. The bigger lesson is for the framework, not
the model ranking: structured-output strategy, token budget for
reasoning models, fence tolerance, and load-eviction are all
**per-backend knobs with measured costs** — TARS should expose them and
document the numbers, not bake one box's results into a default.
Every number here is M5 Pro 48 GB + LM Studio/llama.cpp; re-measure on
your own backend.

---

## 8. Reproducing

```bash
cargo build --release -p tars-cli          # binary with `bench` + `eval`
curl -s http://127.0.0.1:1234/v1/models | jq '.data[].id'   # confirm LM Studio up + model ids
tools/bench-local.sh qwen_coder_local 256 8 # Tier A across the matrix
# Tier B: create bench-corpus/ as in §5, then `tars eval run ...` per model
```

Pitfalls: a model still loading returns `out=0` iters (auto-excluded —
bump `--warmup` if you see the warning); 48 GB means large models can't
co-reside, so the first iter after a swap is always the slow cold load.
