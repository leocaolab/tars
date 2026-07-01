# case-002 — Onion-conformance sweep: an adapter dependency leaked into the type leaf

**Date**: 2026-06-30
**Component**: `tars-types` (the pure leaf crate) — plus two smaller findings in `tars-model`/`tars-runtime` and the FFI bindings
**Outcome**: audit only — three structural findings recorded, **not yet fixed** (the leaf → `reqwest` coupling is the one worth acting on)
**Lesson**: a `# for ProviderError::from(reqwest::Error)` comment in a *-types crate's `Cargo.toml` is a confession, not a note — the type leaf should not know its transport.

> **Companion / provenance**: the methodology here was built by the consumer
> (A.R.C.) as its "onion-conformance" analysis — a codebase-agnostic sweep for
> onion/hexagonal violations (dependency direction, adapter placement,
> ADT-vs-primitive boundaries, cross-name duplication). This file is the
> tars-side artifact from running that same analysis against TARS. The rules
> are not consumer-specific; they are general architecture conformance, which is
> exactly why they port here unchanged.

## TL;DR

Ran an onion-conformance sweep over the 17 TARS crates (206 `.rs` files, 322
public types). TARS is **structurally clean** — genuinely onion-shaped
(`tars-types` leaf, `tars-storage`/`tars-provider` adapters), only 3 cross-name
duplicate-shape pairs across 81 candidate structs (a comparable consumer sweep
found 54). But the sweep surfaced one real dependency-direction violation and two
smaller ones:

1. **`reqwest` (an HTTP client) is a dependency of `tars-types`, the pure leaf.**
   Because every one of the other 16 crates depends on `tars-types`, they all
   transitively pull in `reqwest` — and the leaf's error type is coupled to a
   specific HTTP client. This is a textbook adapter-dependency-in-the-leaf: the
   transport concern belongs in `tars-provider` (the crate that actually does
   HTTP), not in the type leaf.

2. **The `Agent*` type family is defined twice** — `AgentContext`, `AgentError`,
   `AgentOutput`, `AgentRole` each appear in **both** `tars-model` and
   `tars-runtime`.

3. **FFI bindings hand-copy domain types** — `UsageJs` (tars-node) is a
   field-for-field twin of `Usage` (tars-types); `Session` (tars-py) and
   `Pipeline` (tars-node) are similar binding-side re-declarations.

## Finding 1 — `reqwest` leaked into the type leaf (the one to fix)

Evidence:

- `crates/tars-types/Cargo.toml:21` — `reqwest.workspace = true  # for ProviderError::from(reqwest::Error)`
- `crates/tars-types/src/error.rs:263` — `impl From<reqwest::Error> for ProviderError { .. }`
- `crates/tars-types/src/http_extras.rs:26` — re-exports `reqwest::header::{HeaderMap, HeaderName, HeaderValue}`

Why it matters:

- **Direction**: `tars-types` is the inward leaf; nothing should point it *outward*
  at an IO library. `reqwest` is the concern of the HTTP **adapter**
  (`tars-provider`), which is where the actual `reqwest::Client` calls live.
- **Blast radius**: the leaf is depended on by all 16 other crates, so
  `reqwest` (and its transitive tree — `hyper`, `tokio`, TLS) is pulled into every
  crate's build graph whether it does HTTP or not. Compile-time + coupling cost.
- **Coupling**: `ProviderError` — a domain error type — is welded to one specific
  HTTP client. A second transport (a CLI provider, a local-inference provider) can
  never construct a `ProviderError` the same way; the `From<reqwest::Error>` is a
  transport detail masquerading as a domain conversion.

Recommended shape:

- `ProviderError` in `tars-types` stays transport-agnostic — a variant like
  `Transport { kind: TransportErrorKind, detail: String }` (or carrying a boxed
  `std::error::Error`), with NO `reqwest` in scope.
- The `From<reqwest::Error> for ProviderError` conversion moves to
  `tars-provider` (the HTTP adapter), at the boundary where the `reqwest` call is
  made — that is the only place that should name `reqwest`.
- The `http_extras.rs` header re-exports move with it (or callers use
  `reqwest::header::*` directly from the adapter).
- Result: `tars-types/Cargo.toml` drops `reqwest`; the leaf is transport-pure; the
  adapter owns its client.

## Finding 2 — the `Agent*` family is defined in two crates

`AgentContext`, `AgentError`, `AgentOutput`, `AgentRole` each have a `pub`
definition in **both** `tars-model` and `tars-runtime` (same-name, cross-crate).
Either `tars-runtime` re-declares domain types that already live in `tars-model`
(duplication — one concept, two definitions, drift risk), or the two are
genuinely distinct and the shared names are misleading. Needs a read to confirm;
if they are the same concept, collapse to one home (`tars-model`, the domain
crate) and have `tars-runtime` import.

*(This finding is from the automated same-name-cross-crate sweep; it has not been
hand-verified field-by-field like Finding 1.)*

## Finding 3 — FFI bindings hand-copy domain types

- `UsageJs` (`tars-node`) is a 1.00 field-Jaccard twin of `Usage` (`tars-types`).
- `Session` (`tars-py`) and `Pipeline` (`tars-node`) are binding-side
  re-declarations of runtime/pipeline concepts.

FFI boundaries (napi / pyo3) legitimately need their own owned, `#[napi]`/`#[pyclass]`
types — this is not automatically a bug. The smell is **hand-copied fields**:
`UsageJs` should be *derived from* / *convert from* `Usage` (a `From<Usage>` at the
binding edge), not a parallel struct that silently drifts when `Usage` gains a
field. Treat as a lint, not a defect: the binding type is allowed, the manual
field duplication is what rots.

## Method (for reproducing / extending)

Deterministic, no LLM — greps + a field-Jaccard clustering pass:

- per-crate public-type census + leaf-purity check (`grep` for `std::fs` / `reqwest`
  / `tokio::net` in the crates that should be pure);
- same-name-cross-crate type map;
- cross-name field-overlap (`≥3` fields, Jaccard `≥ 0.5`) for shape-duplicates that
  same-name detection misses.

The structural pass gives *recall*; a semantic-role label + a judge give *precision*
(e.g. it correctly rejects FFI-projection false positives). Finding 1 needed no
judge — a transport crate in the type leaf's dependency list is a violation on its
face.

## Status

- Finding 1 (reqwest-in-leaf): **recorded, unfixed** — the actionable one.
- Findings 2–3: **recorded, need a confirming read** before acting.
- No code changed by this audit.
