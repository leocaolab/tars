Python (PyO3, abi3) bindings for tars — a drop-in Python LLMClient over the Rust pipeline; Layer 1 `Provider` (raw backend) + Layer 2 `Pipeline` (middleware-wrapped), agent runtime deferred. (Placement contract; the user-facing README.md is separate.)

- Role (hex): shell (language-FFI driving adapter + its own composition root: config → registry → LlmService assembled per handle)
- Effect budget: none of its own — everything effectful is reached through the composed layers; owns the process-wide tokio runtime and the GIL-release bridge (py.allow_threads), which is threading policy, not an effect
- Deps: may depend on [tars-types, tars-utils, tars-config, tars-provider, tars-pipeline, tars-cache, tars-melt, tars-runtime, pyo3, tokio]; MUST NOT import [rusqlite → tars-storage/tars-cache/tars-melt; reqwest → tars-provider; napi → tars-node owns the Node bridge]
- Owns concepts: [the Python API surface (tars.Provider, tars.Pipeline, complete(), role spine init/provider(role)/pipeline(role)), Python-error mapping for ConfigError/ProviderError, the TOKIO OnceLock runtime, python/tars/__init__.py re-exports]
- Reason to change (the ONE): the Python-facing surface or the Rust↔Python bridging changes
- Belongs here: a #[pyfunction]/#[pyclass] exposing an existing Rust capability; camelCase/snake_case DTO mapping; GIL/runtime bridging
- Does NOT belong: new domain behavior implemented in the binding → the owning tars-* layer first, binding second (the binding is a projection, never the home); Node-flavored API decisions → tars-node (the two bindings mirror each other one-for-one — divergence is a smell); middleware policy → tars-pipeline
