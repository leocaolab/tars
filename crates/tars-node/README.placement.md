Node.js (napi-rs) bindings for tars — TypeScript-callable `Pipeline.complete()`, (one tokio runtime per process, the role spine, the layer split). (Placement contract; the user-facing README.md is separate.)

- Role (hex): shell (language-FFI driving adapter + its own composition root: config → registry → LlmService assembled per handle)
- Effect budget: none of its own — everything effectful is reached through the composed layers; owns the process-wide tokio runtime (TOKIO OnceLock, napi tokio_rt dispatch)
- Deps: may depend on [tars-types, tars-utils, tars-config, tars-llm, tars-storage, napi/napi-derive, tokio]; MUST NOT import [rusqlite → tars-storage/tars-melt; reqwest → tars-llm]
- Owns concepts: [the TS API surface (Pipeline.fromConfigPath/fromStr, complete(opts), role spine init/provider(role)/pipeline(role)), CompleteOptions camelCase ↔ ChatRequest mapping, JS error mapping, index.d.ts/index.js/npm packaging]
- Reason to change (the ONE): the TS-facing surface or the Rust↔Node bridging changes
- Belongs here: a #[napi] export projecting an existing Rust capability; napi-friendly DTOs; .node build/packaging plumbing
- Does NOT belong: new domain behavior implemented in the binding → the owning tars-* layer first, binding second; provider wire handling → tars-llm
