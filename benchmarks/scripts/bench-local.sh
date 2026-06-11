#!/usr/bin/env bash
# bench-local.sh — Tier A speed sweep across the local model matrix.
# Swaps one model at a time (48 GB can't co-reside large 8-bit models),
# tees each `tars bench` summary into benchmarks/runs/speed/<ts>/.
#
# Usage: benchmarks/scripts/bench-local.sh [provider] [max_tokens] [repeat]
# Run from the repo root. Outputs are gitignored scratch; promote a keeper
# into benchmarks/baselines/speed/ to track it.
set -euo pipefail
PROVIDER="${1:-qwen_coder_local}"
MAXTOK="${2:-128}"
REPEAT="${3:-5}"
OUT="benchmarks/runs/speed/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
TARS=./target/release/tars
CFG="$HOME/.tars/config.toml"
MODELS=(
  qwen/qwen3.5-9b            google/gemma-4-e4b
  qwen/qwen3.6-27b          google/gemma-4-31b
  qwen3.6-35b-a3b           google/gemma-4-26b-a4b
  qwen/qwen3-coder-30b
)
command -v lms >/dev/null && lms unload --all >/dev/null 2>&1 || true
for m in "${MODELS[@]}"; do
  safe="${m//\//_}"
  echo "── benching $m ──"
  "$TARS" --config "$CFG" bench "$PROVIDER" --model "$m" \
      --max-tokens "$MAXTOK" --warmup 3 --repeat "$REPEAT" \
      > "$OUT/$safe.txt" 2> >(tail -3 >&2) || echo "  FAILED: $m (skipped)"
  command -v lms >/dev/null && lms unload --all >/dev/null 2>&1 || true
done
echo "summaries in $OUT/"
grep -H -A6 "── stats" "$OUT"/*.txt 2>/dev/null || true
