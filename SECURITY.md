# Security Policy

## Reporting a vulnerability

Email **hucao1@gmail.com** with subject prefix `[tars security]`.
Please include:

- Affected version (commit hash or release tag)
- Reproduction (minimal code or trace)
- Impact assessment (who is affected, what's exposed)
- Any disclosure timeline you'd like us to honor

Expect an acknowledgement within 72 hours and a remediation plan
within 7 days for confirmed issues.

Do **not** open a public GitHub issue for vulnerabilities until a
fix has shipped. Coordinated disclosure helps every consumer of
this codebase land a fix before details go public.

## In scope

- Code in this repository under `crates/`
- Build and supply-chain assets: `Cargo.toml`, `Cargo.lock`,
  `pyproject.toml`, GitHub Actions workflows when added
- Released wheels (`tars-*.whl`) once distributed
- Documentation in `docs/` that prescribes a security-relevant
  pattern (Doc 06 multi-tenant isolation, Doc 10 security model,
  Doc 17 tenant-scoped body store)

## Out of scope

- Provider-side issues (Anthropic, OpenAI, Gemini, etc.) — report
  those to the respective vendor
- Issues in consumer applications built on top of `tars` (those go
  to that consumer's repo)
- Issues that require the attacker to already have local code
  execution as the running user

## Hardening notes for consumers

- `tars` does **not** persist provider API keys. Keys live in your
  config file (typically `~/.tars/config.toml`) or env vars; loss
  of those files = loss of credentials. `chmod 600` recommended.
- `tars-storage` body store is tenant-scoped at the SQL key level
  but is not encrypted at rest. If your bodies contain secrets,
  configure your filesystem encryption or upgrade to an
  encryption-at-rest backend (designed but not shipped — see
  Doc 17 §6).
- Output validators (Doc 15) are caller-supplied code that runs
  in-process. Treat them as trusted code, not user input.
