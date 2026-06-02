#!/usr/bin/env bash
# schema-penalty.sh — quantify the GBNF/constrained-decoding tax on a
# local OpenAI-compatible server (LM Studio / llama.cpp).
#
# Sends the SAME prompt to the SAME model twice: (A) free decode and
# (B) with response_format=json_schema, and reports tok/s for each.
# tars bench can't do this (it never sends a schema), hence raw curl.
#
# Usage: tools/schema-penalty.sh [model] [base_url]
set -euo pipefail
M="${1:-qwen/qwen3-coder-30b}"
URL="${2:-http://127.0.0.1:1234/v1}/chat/completions"
P="List three programming languages with a name, year created, and primary paradigm. Return as JSON."
SCHEMA='{"type":"json_schema","json_schema":{"name":"langs","strict":true,"schema":{"type":"object","properties":{"languages":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"year":{"type":"integer"},"paradigm":{"type":"string"}},"required":["name","year","paradigm"],"additionalProperties":false}}},"required":["languages"],"additionalProperties":false}}}'

run() { # $1=label  $2=extra-json
  local body=/tmp/schema-penalty.json
  local t ct tps
  t=$(curl -s -o "$body" -w '%{time_total}' -m 300 "$URL" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"$M\",\"messages\":[{\"role\":\"user\",\"content\":\"$P\"}],\"max_tokens\":300,\"stream\":false$2}")
  ct=$(jq -r '.usage.completion_tokens // 0' "$body")
  tps=$(awk -v c="$ct" -v s="$t" 'BEGIN{printf "%.1f", (s>0?c/s:0)}')
  printf "%-22s time=%6.2fs  tokens=%4s  %5s tok/s\n" "$1" "$t" "$ct" "$tps"
}

echo "schema-penalty: model=$M"
run "A free decode"      ""
run "B json_schema(GBNF)" ",\"response_format\":$SCHEMA"
echo "→ ratio = (A tok/s) / (B tok/s); >1 means the schema is taxing decode."
