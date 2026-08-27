Node.js (napi-rs) bindings for tars — TypeScript-callable `Pipeline.complete()`, mirroring tars-py one-for-one (same runtime pattern, same role spine, same layer split). (Placement contract; the user-facing README.md is separate.)

- Role (hex): shell (language-FFI driving adapter + its own composition root: config → registry → LlmService assembled per handle)
- Effect budget: none of its own — everything effectful is reached through the composed layers; owns the process-wide tokio runtime (TOKIO OnceLock, napi tokio_rt dispatch)
- Deps: may depend on [tars-types, tars-utils, tars-config, tars-provider, tars-pipeline, tars-cache, tars-storage, tars-runtime, napi/napi-derive, tokio]; MUST NOT import [rusqlite → tars-storage/tars-cache/tars-melt; reqwest → tars-provider; pyo3 → tars-py owns the Python bridge]
- Owns concepts: [the TS API surface (Pipeline.fromConfigPath/fromStr, complete(opts), role spine init/provider(role)/pipeline(role), blessCheck), CompleteOptions camelCase ↔ ChatRequest mapping, JS error mapping, index.d.ts/index.js/npm packaging]
- Reason to change (the ONE): the TS-facing surface or the Rust↔Node bridging changes
- Belongs here: a #[napi] export projecting an existing Rust capability; napi-friendly DTOs; .node build/packaging plumbing
- Does NOT belong: new domain behavior implemented in the binding → the owning tars-* layer first, binding second; a Python-flavored API decision → tars-py (the bindings mirror one-for-one — divergence is a smell); provider wire handling → tars-provider
