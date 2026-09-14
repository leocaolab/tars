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

jq '[.findings[] | select(.finding.kind | startswith("shipped_"))]' \
  "$run/report.json" >"$run/drift.json"
drift_count="$(jq length "$run/drift.json")"
if [[ "$drift_count" == 0 ]]; then
  log "provider.toml matches the APIs — nothing to propose"
  exit 0
fi
log "provider.toml drift: $drift_count finding(s)"

open_pr="$(gh pr list --repo "$gh_repo" --state open --json number,headRefName \
  --jq '[.[] | select(.headRefName | startswith("bot/provider-refresh-"))][0].number // empty')"
if [[ -n "$open_pr" ]]; then
  log "PR #$open_pr from an earlier refresh is still open — not opening another"
  exit 0
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
  exit 0
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
  exit 0
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
