# Daily model-catalog refresh

tars's model catalog has two layers (`crates/tars-config/src/model_catalog.rs`):
the shipped `data/provider.toml` (prices, thinking mode, limits — from the
providers' docs) and each machine's `$TARS_HOME/models.json` (the ids and limits
the providers' list APIs report, written by `tars models update`). This job keeps
both current on a schedule:

1. **Refresh this machine's live layer.** `tars models update` — ids and limits
   from the APIs. Every tars process started afterwards sees them, including
   `<series>@latest` picking a newly released model.
2. **Propose provider.toml changes.** When the refresh report says the shipped
   file is behind — a series' newest model has no row, a row the API no longer
   lists, a limit the API contradicts — `claude -p` researches the official docs,
   tests thinking behavior live, edits `provider.toml` (and nothing else), and the
   script opens a PR with the evidence. The PR goes through review and the merge
   queue like any other change; nothing lands on its own.

The refresh only calls list-models APIs; it never scrapes pages. Claude reads
the official docs only for what those APIs don't report (prices, thinking mode,
retirement dates).

## Guard rails in `refresh.sh`

- Claude's tools are scoped: edit `provider.toml` and the PR-body file only;
  Bash only for `curl`, `jq` and `cargo test -p tars-config`.
- The script refuses to push if anything besides `provider.toml` changed, if
  there is no PR body, or if `cargo test -p tars-config` fails.
- One open `bot/provider-refresh-*` PR at a time: while one is open, later runs
  only refresh the live layer.
- Each run leaves `report.json`, `drift.json`, the prompt, `claude.log`, the diff
  and the PR body under `~/.local/state/tars-refresh/<timestamp>/`.
- **A provider that could not be asked is not a provider that agrees.** Drift is
  a diff against what each API reports, so a provider that returns nothing
  produces an empty diff — which used to read as "matches the APIs". Any
  provider whose `entry.status` is neither `ok` nor `skipped` (local servers
  excepted — a down llamacpp/mlx/vllm is this machine's resting state) is logged
  as `BLIND <name>: <status> — <reason>` and makes the run **exit non-zero**,
  even when it opened a PR for the providers that did answer. Without that, the
  only signal is a green timer.

  Measured 2026-09-18: `anthropic` came back `no_key` with 0 models and the run
  reported "nothing to propose", while `provider.toml` was missing
  `claude-opus-5` — the model 93% of this org's calls run on. The status was
  recorded the whole time; nothing read it. **A blind provider usually means a
  missing key in the `provider-keys` credential** (see below) — the note says
  which env var.

## Install (bluewhale)

```bash
git clone git@github.com:leocaolab/tars.git ~/projects/tars-models

# Provider keys as an encrypted user credential, KEY=VALUE lines, piped from
# wherever they live — never written to disk in the clear:
grep -E '^(GEMINI|DEEPSEEK|OPENAI|ANTHROPIC)_API_KEY=.+' ~/.tars/.env \
  | ssh bluewhale 'umask 077; mkdir -p ~/.config/tars-refresh &&
      systemd-creds encrypt --user --name=provider-keys - ~/.config/tars-refresh/provider-keys.cred'

cp ~/projects/tars-models/tools/model-refresh/tars-model-refresh.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tars-model-refresh.timer
```

Run once by hand: `systemctl --user start tars-model-refresh.service`, then
`journalctl --user -u tars-model-refresh.service`.

Dry run (everything except push and PR):

```bash
systemd-run --user --wait --pipe \
  -p LoadCredentialEncrypted=provider-keys:$HOME/.config/tars-refresh/provider-keys.cred \
  -E TARS_REFRESH_REPO=$HOME/projects/tars-models -E TARS_REFRESH_DRY_RUN=1 \
  -E PATH=$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin \
  bash ~/projects/tars-models/tools/model-refresh/refresh.sh
```

## What the credential does and does not protect

`systemd-creds --user` keeps the keys off disk in the clear and out of every
other unit's environment. It is not a boundary against code running as the same
user: that user can ask systemd to decrypt the file. On a machine whose merge
gate runs PR code as the same user, a hostile PR could reach the keys. Real
isolation means running this job as a separate Linux user.
