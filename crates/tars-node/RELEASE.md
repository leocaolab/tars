# Releasing `@leocaolab/tars-node`

The native addon ships as a main package (`@leocaolab/tars-node`, JS +
type defs) plus one **per-platform** package per supported target
(`@leocaolab/tars-node-<platform>`) that carries the compiled `.node`.
npm/pnpm installs only the package matching the consumer's `os`/`cpu`/
`libc` (via `optionalDependencies`), and `index.js` `require`s it.

This is what lets a consumer (e.g. the `essay` app) deploy on Linux even
though development happens on macOS — the right binary is pulled at
install time instead of relying on a checked-in, single-platform `.node`.

## Supported targets

| Target | Package | Built on |
|---|---|---|
| `aarch64-apple-darwin` | `@leocaolab/tars-node-darwin-arm64` | macOS (native) |
| `x86_64-unknown-linux-gnu` | `@leocaolab/tars-node-linux-x64-gnu` | Linux + `--zig` (portable glibc) |

To add a target: append its triple to `package.json` → `napi.triples`,
add the matching `optionalDependencies` entry, run
`npx napi create-npm-dir -t .`, and add a matrix row to
`.github/workflows/release-tars-node.yml`.

## One-time prerequisites

1. The npm scope **`@leocaolab`** must exist (npm org or user scope) with
   publish rights.
2. Add an npm **automation token** as the repo secret **`NPM_TOKEN`**
   (Settings → Secrets and variables → Actions).

## Cutting a release

1. Bump the version **in two places, kept identical**, in
   `crates/tars-node/package.json`:
   - the top-level `"version"`, and
   - every entry under `"optionalDependencies"`.
   (The per-platform `npm/*/package.json` stubs are regenerated from the
   main version by CI, so they don't need a manual edit.)
2. Commit, then tag and push:
   ```bash
   git tag tars-node-v0.3.2
   git push origin tars-node-v0.3.2
   ```
3. The `release-tars-node` workflow builds each platform binary and
   publishes the per-platform packages **then** the main package.

`workflow_dispatch` runs the same flow manually (publishes whatever
version is in `package.json`).

## Switching a consumer off the `file:` dependency

While iterating locally, a consumer can depend on
`"@leocaolab/tars-node": "file:../../tars/crates/tars-node"` — `index.js`
loads the local `./tars-node.<platform>.node` directly, so no publish is
needed and the unpublished `optionalDependencies` are skipped.

**For deploy**, that `file:` path doesn't exist in the build context
(the consumer repo is checked out alone). After the first successful
publish, switch the consumer to the registry version:

```jsonc
// before (local dev only)
"@leocaolab/tars-node": "file:../../tars/crates/tars-node"
// after (deployable)
"@leocaolab/tars-node": "^0.3.1"
```
