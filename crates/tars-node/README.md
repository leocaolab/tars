# @leocaolab/tars-node

Rust-backed multi-provider LLM runtime exposed as a Node.js native addon.

The Node binding for [TARS](https://github.com/leocaolab/tars). Same shape as
[`tars-py`](../tars-py): a compiled extension wrapping TARS's middleware
pipeline (cache, retry, telemetry, validation) so TypeScript / Node code can
use one Rust-backed handle for any provider — `claude` / `openai` / `gemini` /
`vllm` / `mlx` / `llamacpp` / `claude_cli` / `gemini_cli` / `claude_sdk` /
`codex_cli`.

> **Status — `0.2.0-alpha.0` (scaffold, M1)**
>
> The build pipeline (cargo + napi-build → `libtars_node.dylib` →
> `napi build` → `.node` + `index.js` + `index.d.ts`) is in place and
> the JS-side smoke test rounds-trips through the napi boundary. The
> milestone after this (M2) wires `Pipeline.complete()` against the
> real `Arc<dyn LlmService>` — until M2 lands, `complete()` returns a
> synthetic echo so the marshalling can be exercised without a
> provider. **Do not depend on this for production calls yet.**

## Build

```bash
# one-time per env
npm install   # installs @napi-rs/cli devDependency

cd crates/tars-node

# build a debug .node for the current platform
npm run build:debug

# release build (strips symbols, smaller binary)
npm run build
```

That produces `tars-node.<triple>.node` (e.g. `tars-node.darwin-arm64.node`)
plus `index.js` and `index.d.ts` — the auto-generated TypeScript types
that mirror every `#[napi]` annotation in `src/lib.rs`.

## Use

```ts
import { Pipeline } from '@leocaolab/tars-node';

// Construct from a .arc/config.toml-shaped file. Same TOML schema
// `tars-py` reads; share one config across both bindings.
const pipeline = Pipeline.fromConfigPath('.arc/config.toml', 'gemini_pro');

const resp = await pipeline.complete({
    model: 'gemini-3.1-pro-preview',
    system: 'You are a precise technical reviewer.',
    user: 'Review this Rust function for race conditions: ...',
    maxOutputTokens: 2000,
    temperature: 0.0,
    responseSchema: {
        type: 'object',
        properties: {
            findings: { type: 'array', items: { type: 'object' } },
        },
        required: ['findings'],
    },
    responseSchemaStrict: true,
});

console.log(resp.text);              // assistant text
console.log(resp.usage.inputTokens); // tokens billed in
console.log(resp.usage.outputTokens);
console.log(resp.stopReason);        // "end_turn" / "max_tokens" / ...
```

## Why a separate binding?

Same answer as `tars-py`: every layer above the provider — cache, retry,
fallback routing, telemetry, validation, circuit breaker, budget — is
Rust code we don't want to re-implement in TypeScript. The binding
hands a single handle (`Pipeline`) across the FFI boundary; the Node
caller never sees the middleware chain, just `complete(opts) → Promise<result>`.

## Roadmap

| Milestone | What lands |
|-----------|------------|
| **M1** (this release) | scaffold, build pipeline, smoke test |
| **M2** | real `Pipeline.complete()` wired to `Arc<dyn LlmService>` |
| **M3** | `npm publish` of platform-tagged tarballs (darwin x64/arm64, linux gnu x64/arm64) |
| M4 | streaming (`stream()` → AsyncIterator over ChatEvent) |
| M5 | `run_task(...)` over the DAG executor (Doc 04 §4) |
| M6 | tool calling, tool_choice, structured tool results |
| M7 | per-call cancellation token |

Out of scope for the napi binding: spawning agents (use the Rust
runtime in-process); training / fine-tuning (not in TARS scope at all).

## Internals

`src/lib.rs` mirrors `crates/tars-py/src/lib.rs` one-for-one:

- Process-wide tokio runtime, lazy `OnceLock<Runtime>` — same pattern
  as `tars-py::TOKIO`. Each `complete()` blocks on this runtime via
  `napi-rs`'s async glue; the JS caller's event loop is never
  serialised.
- `Pipeline` napi class holds `Arc<dyn LlmService>` once `from_config_path`
  resolves the provider through `ProviderRegistry::from_config` +
  `tars_pipeline::Pipeline::builder_with_inner`.
- Request building (`CompleteOptions` → `ChatRequest`) is in-line —
  the napi-friendly camelCase struct maps 1:1 to the snake_case
  ChatRequest fields. JSON Schemas pass through as opaque
  `serde_json::Value` (no per-shape TS types yet).
- Errors map `ProviderError` / `RuntimeError` / `ConfigError` to napi's
  `napi::Error`, which surfaces to JS as a rejected `Promise`.
