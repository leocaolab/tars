# Contributing to tars

tars follows **SemVer**: breaking changes land on a major version, never a minor.

## How a change gets in

This repository is published from the maintainers' working tree: each release is
one commit here. A pull request is welcome, and it is taken as a **proposal**:

1. Open the pull request here, with tests for any behavior change.
2. A maintainer reviews it here. When it is accepted, the maintainer carries the
   change into the working tree, where it runs through the full test gate.
3. It ships in the next release commit. The pull request is closed with a link to
   that commit, and the CHANGELOG entry for the release thanks you by name.

So the pull request is not merged here, but the change and the credit are yours.

## What is welcome

- Bug reports with a reproduction (open a GitHub issue).
- New provider backends following the `LlmProvider` trait
  (`crates/tars-llm/src/provider/backends/`).
- Built-in validators (`crates/tars-llm/src/validation/`).
- Documentation fixes, especially where the reason behind a choice is missing.
- Performance or correctness fixes with tests.

Open an issue with a sketch **before** writing a new crate, a new middleware
layer, a rename that touches every call site, or a config format change.

Security problems are reported privately: see [`SECURITY.md`](SECURITY.md).

## Build and test

Prerequisites: the Rust toolchain pinned in `rust-toolchain.toml`.

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs the same commands. The Node binding (`crates/tars-node`) builds with
`npm install && npm run build` in that directory and tests with `npm test`.

## Pull request conventions

- One concern per pull request.
- Commit messages in the form `fix(tars-llm): …`, `feat(tars-cli): …`,
  `docs: …`; the scope is usually a crate name.
- Tests for every behavior change; documentation-only changes are the exception.
- A CHANGELOG entry under `unreleased` for anything a user can see, saying what
  changed and why.

## Code style

- `rustfmt` with the repository's `rustfmt.toml`, and clippy with `-D warnings`.
- Comments say **why**, not what: an outside constraint, a bug worked around, an
  invariant the types cannot express.

## License

By contributing, you agree your contributions are licensed under the Apache
License 2.0, the project's license (section 5 of the license covers
contributions).
