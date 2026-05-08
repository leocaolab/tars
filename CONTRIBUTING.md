# Contributing to tars

> **Status:** pre-1.0. The public surface (Rust crates, `tars-py`
> Python API, CLI) is **not yet stable**; expect breaking changes
> on minor version bumps until 1.0. SemVer-strict commitment kicks
> in at 1.0.

## What contributions are welcome

- Bug reports + reproductions (open a GitHub issue)
- New provider backends following the `LlmProvider` trait
  (`crates/tars-provider/src/backends/`)
- Built-in validators (`crates/tars-pipeline/src/validation/builtin.rs`)
  — see Doc 15 §5 for the philosophy of what belongs here vs what
  stays consumer-side
- Doc improvements — especially places where the rationale ("why")
  is missing from a design choice
- Performance or correctness fixes with tests

What we're **not** taking right now:

- New top-level crates (changes the workspace shape)
- New middleware layers without a documented use case (per Doc 02
  trigger-or-delete contracts in `TODO.md` §O-1..O-10)
- API renames that touch every callsite
- Changes that require config file format breaks

If you want to do any of the "not right now" items, **open an issue
first** with a design sketch.

## Build + test

Prerequisites: stable Rust 1.85+, Python 3.10+ (for `tars-py`
wheel), `cargo`, `maturin`.

```bash
# Rust workspace
cargo build --workspace
cargo test --workspace --exclude tars-py
cargo clippy --workspace --all-targets -- -D warnings

# tars-py wheel (requires Python)
cd crates/tars-py
maturin develop --release    # installs into current env
# OR
maturin build --release      # → target/wheels/tars-*.whl

# pytest (after wheel install)
pytest crates/tars-py/python/tests
```

CI runs the same three Rust commands plus pytest, against the
versions of Rust + Python pinned in `rust-toolchain.toml` and
`crates/tars-py/pyproject.toml`. Local green ≠ CI green if you
deviate from those, so keep the pins matching.

## Pull request conventions

- **One concern per PR.** "Fix X + refactor Y" in one PR makes
  review harder; split it.
- **Conventional commit messages**: `feat(scope): ...`,
  `fix(scope): ...`, `docs(scope): ...`, `test(scope): ...`,
  `chore(scope): ...`. Scope is usually a crate name
  (`tars-pipeline`, `tars-py`, etc.) or a milestone tag (`B-20.W4`).
- **Sign-off line** in every commit:
  ```
  Signed-off-by: Your Name <you@example.com>
  ```
  Set once: `git config commit.gpgsign true` and use `git commit -s`.
- **Tests required** for any behavior change. Doc-only changes are
  the only PRs that ship without test changes.
- **Run clippy before pushing.** CI gates on `-D warnings`.
- **Update CHANGELOG.md** for user-facing changes — under the
  `<unreleased>` section, with the same shape as existing entries.
- **Update relevant Doc** if the change alters a documented design
  decision. Don't let docs drift; the W4 audit story
  (`docs/audit-stories/case-001-cache-validator-audit.md`) is what
  happens when they do.

## Repo conventions

- **17 design docs** in `docs/00-overview.md` through
  `docs/17-pipeline-event-store.md` are the source of truth for
  *why* the code is shaped the way it is. Read the relevant doc
  before touching a layer.
- **Trigger-or-delete contracts** in `TODO.md` mark deferred work
  with an explicit condition for when to revisit. Don't add a
  feature that's "we might want this someday" — add a TODO entry
  with a trigger.
- **Audit stories** in `docs/audit-stories/` capture moments where
  the system caught itself being wrong. New entries when you hit
  one of those moments are highly welcome — see the index README
  for the format.
- **CHANGELOG.md** is per-feature with rationale, not just a
  one-line bullet. We log *why* changes shipped, not just *what*.

## Architectural decision flow

For non-trivial changes, the typical path is:

1. **Open an issue** describing the problem (not the solution)
2. **Sketch a design** in the issue or as a `docs/NN-...md` draft
3. **Get review** from a maintainer before writing code
4. **Implement** with tests
5. **CHANGELOG entry** + relevant doc update
6. **Submit PR**

For trivial changes (typo, doc clarification, obvious bug fix):
PR directly. We'll tell you if it should have been an issue first.

## Code style

- `rustfmt` (default config) — CI checks this
- `clippy --all-targets -- -D warnings` — no warning bypasses
  unless extensively justified in a comment
- Comments document **why**, not **what** (well-named identifiers
  do the latter). One short line max for trivial WHY; longer for
  "this is the workaround for behavior X in dependency Y".
- No backwards-compatibility shims for unshipped API. Pre-1.0
  means we change things; SemVer doesn't apply yet.

## License

By contributing, you agree your contributions are licensed under
Apache License 2.0 (the project's existing license).

## Questions

Open an issue or email the address in `SECURITY.md` for non-public
matters. We don't have a chat / Discord / mailing list yet — small
project, low volume; GitHub issues are sufficient.
