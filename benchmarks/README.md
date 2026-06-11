# benchmarks/

Everything benchmark- and eval-related lives here, so the repo root stays
clean. Two activities, one tree:

- **speed** — throughput/latency of a provider (`tars bench`). No corpus; just
  hammer a model and report TTFB / decode tok/s / total.
- **eval** — replay an input **corpus** through a pipeline and score the outputs
  (`tars eval run` / `diff` / `judge`). Tests the *pipeline*, not raw speed.

```
benchmarks/
  corpus/          inputs — committed fixtures. One dir per case, each with input.txt
    config_json/   (+ optional system.txt, expected.txt — see `tars eval` --help)
    langs_json/
    user_json/
  scripts/         the runners
    bench-local.sh      speed sweep across the local model matrix → runs/speed/<ts>/
    schema-penalty.sh   the GBNF/constrained-decode tax (free vs json_schema)
  baselines/       curated reference results — COMMITTED. The numbers `eval diff`
    speed/<date>/<model>.txt        compares a fresh run against. Promote a keeper
    eval/<model>/{manifest,...}     here by hand; don't dump every run.
  runs/            scratch output — GITIGNORED. `tars eval` and bench-local.sh
                   write here by default; regenerable, never committed.
```

## The rule: inputs & baselines are tracked, runs are not

| Dir | Tracked? | What it is |
|---|---|---|
| `corpus/` | ✅ | the eval inputs — stable, versioned fixtures |
| `scripts/` | ✅ | the runners |
| `baselines/` | ✅ | a small, curated set of reference results to diff against |
| `runs/` | ❌ (`.gitignore`) | every-day scratch output; promote keepers into `baselines/` |

This is why generated output stopped accreting in git: a run lands in
`runs/` (ignored); only when you decide "this is the number we compare against"
do you copy it into `baselines/`.

## Quick start

```bash
# build the binary that has `bench` + `eval`
cargo build --release -p tars-cli

# SPEED — sweep the local matrix (run from repo root)
benchmarks/scripts/bench-local.sh qwen_coder_local 256 8
#   → benchmarks/runs/speed/<ts>/<model>.txt

# EVAL — replay the corpus through a model, write a run
./target/release/tars eval run \
  --corpus benchmarks/corpus --provider qwen_coder_local \
  --model qwen/qwen3-coder-30b --check non-empty --check valid-json \
  --max-output-tokens 4096
#   → benchmarks/runs/eval/<ts>/   (omit --output to get the default)

# COMPARE a fresh run against a committed baseline
./target/release/tars eval diff \
  benchmarks/baselines/eval/qwen3.6-27b/ benchmarks/runs/eval/<ts>/
```

## See also

- [`../docs/benchmarks/local-llm-bench.md`](../docs/benchmarks/local-llm-bench.md) — the local-LLM benchmark runbook + findings
- [`../docs/eval-methodology.md`](../docs/eval-methodology.md) — what `eval diff` measures and why
- [`../docs/architecture/16-evaluation-framework.md`](../docs/architecture/16-evaluation-framework.md) — the eval framework design
