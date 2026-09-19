#!/usr/bin/env bash
# Daily model-catalog refresh — see README.md.
#
#   1. `tars models update` refreshes this machine's $TARS_HOME/models.json
#      (ids + the limits each provider's API reports) — the live layer every
#      tars process on the machine reads.
#   2. When the report says the shipped data/provider.toml is behind the APIs
#      (a series' newest model has no row, a row the API no longer lists, a
#      limit the API contradicts), Claude researches the official docs, tests
#      the behavior live, edits provider.toml, and this opens a PR for review.
#
# Runs from a dedicated clone ($TARS_REFRESH_REPO) that the unit resets to
# origin/main before each run. Provider keys arrive as the systemd credential
# `provider-keys` (KEY=VALUE lines).
#
# TARS_REFRESH_DRY_RUN=1 does everything except push and open the PR.
set -euo pipefail

repo="${TARS_REFRESH_REPO:?TARS_REFRESH_REPO is the dedicated tars clone}"
gh_repo="${TARS_REFRESH_GH_REPO:-leocaolab/tars}"
state="${STATE_DIRECTORY:-$HOME/.local/state/tars-refresh}"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
run="$state/$stamp"
mkdir -p "$run"
here="$(cd "$(dirname "$0")" && pwd)"

log() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$run/run.log"; }

if [[ -n "${CREDENTIALS_DIRECTORY:-}" && -f "$CREDENTIALS_DIRECTORY/provider-keys" ]]; then
  set -a
  # shellcheck disable=SC1091
  . "$CREDENTIALS_DIRECTORY/provider-keys"
  set +a
else
  log "no provider-keys credential — providers needing a key will report no_key"
fi

cd "$repo"
log "tars $(git rev-parse --short HEAD); building the CLI"
cargo build --release -q -p tars-harness --bin tars
tars="$repo/target/release/tars"

log "refreshing the model library"
"$tars" models update --json >"$run/report.json" 2>"$run/update.stderr"
jq -r '.findings[].message' "$run/report.json" | tee -a "$run/run.log"

# A provider we could not ASK is not a provider that AGREES with us.
#
# Drift is computed by diffing provider.toml against the models each API
# reports. When an API reports nothing, the diff is empty — and the old code
# read that empty diff as "matches the APIs". Measured 2026-09-18: anthropic
# came back `no_key`, 0 models, and the run said "nothing to propose" while the
# file was missing claude-opus-5 (the model 93% of this org's calls run on) and
# claude-fable-5-1. openai/gemini/deepseek answered that same morning, so the
# green was for the seven providers it could see and silence for the one it
# could not.
#
# `status` was recorded all along (`entry.status`, with `entry.note` giving the
# reason) — nothing read it. run.log only prints `findings[].message`, and
# `no_key` is not a finding, so it never surfaced anywhere either.
#
# Local servers (llamacpp / mlx / vllm) being unreachable is the normal resting
# state of a machine that does not run them, and `skipped` is how a CLI provider
# declares it has no list API — neither is a failure. Everything else means this
# run is BLIND for that provider, and the run says so and ends non-zero, so the
# timer's own status stops reading green.
blind="$(jq -r '[.providers[]
  | select((.entry.type | IN("llamacpp", "mlx", "vllm")) | not)
  | select(.entry.status != "ok" and .entry.status != "skipped")
  | "\(.name): \(.entry.status) — \(.entry.note // "no reason recorded")"] | .[]' \
  "$run/report.json")"
if [[ -n "$blind" ]]; then
  while IFS= read -r line; do log "BLIND $line"; done <<<"$blind"
  log "the drift result below covers only the providers that answered"
fi

jq '[.findings[] | select(.finding.kind | startswith("shipped_"))]' \
  "$run/report.json" >"$run/drift.json"
drift_count="$(jq length "$run/drift.json")"
if [[ "$drift_count" == 0 ]]; then
  if [[ -n "$blind" ]]; then
    log "no drift among the providers that answered — but some did not, see BLIND above"
    exit 1
  fi
  log "provider.toml matches the APIs — nothing to propose"
  exit 0
fi
log "provider.toml drift: $drift_count finding(s)"

open_pr="$(gh pr list --repo "$gh_repo" --state open --json number,headRefName \
  --jq '[.[] | select(.headRefName | startswith("bot/provider-refresh-"))][0].number // empty')"
if [[ -n "$open_pr" ]]; then
  log "PR #$open_pr from an earlier refresh is still open — not opening another"
  [[ -z "$blind" ]]; exit
fi

branch="bot/provider-refresh-$stamp"
git checkout -q -b "$branch"
# Inside the repo's ignored target/, so Claude's write permission can name it
# by a repo-relative path.
body_rel="target/model-refresh/$stamp/pr-body.md"
body="$repo/$body_rel"
mkdir -p "$(dirname "$body")"

log "asking Claude to research and edit provider.toml (log: $run/claude.log)"
{
  cat "$here/prompt.md"
  printf '\n\n## This run\n\n- Report: `%s`\n- Write the PR body to: `%s`\n- Drift findings:\n\n```json\n' \
    "$run/report.json" "$body"
  cat "$run/drift.json"
  printf '\n```\n'
} >"$run/prompt.md"
claude -p "$(cat "$run/prompt.md")" \
  --allowedTools "Read,Grep,Glob,WebFetch,WebSearch,Edit(./crates/tars-config/data/provider.toml),Edit(./$body_rel),Bash(curl:*),Bash(jq:*),Bash(cargo test -p tars-config:*)" \
  >"$run/claude.log" 2>&1 || log "claude exited non-zero — checking what it left"

changed="$(git status --porcelain --untracked-files=all | grep -v '^?? target/' || true)"
if [[ -z "$changed" ]]; then
  log "Claude changed nothing (see claude.log) — no PR"
  [[ -z "$blind" ]]; exit
fi
if [[ "$changed" != " M crates/tars-config/data/provider.toml" ]]; then
  log "refusing to push: the edit touched more than provider.toml:"
  log "$changed"
  exit 1
fi
if [[ ! -s "$body" ]]; then
  log "refusing to push: no PR body with the evidence at $body"
  exit 1
fi

log "cargo test -p tars-config"
cargo test -q -p tars-config >"$run/test.log" 2>&1 || {
  log "tars-config tests fail on the edit — not pushing (see $run/test.log)"
  exit 1
}

git diff >"$run/provider.toml.diff"
if [[ "${TARS_REFRESH_DRY_RUN:-}" == 1 ]]; then
  log "dry run: not pushing. diff: $run/provider.toml.diff  body: $body"
  [[ -z "$blind" ]]; exit
fi

git -c user.name="tars model refresh" -c user.email="tars-model-refresh@bluewhale" \
  commit -q -am "data(provider.toml): refresh from the provider APIs ($stamp)

Drift found by tars models update; researched and edited by Claude. Evidence
in the PR description."
git push -q origin "$branch"
cp "$body" "$run/pr-body.md"
url="$(gh pr create --repo "$gh_repo" --head "$branch" --base main \
  --title "data(provider.toml): 按线上 API 刷新（$stamp）" --body-file "$body")"
log "opened $url"

# A run that could not ask every provider ends non-zero even when it did open a
# PR: the PR covers what answered, and the timer's status is the only place the
# silence would otherwise show.
[[ -z "$blind" ]]
