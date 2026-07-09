# @leocaolab/tars-node

Rust-backed multi-provider LLM runtime exposed as a Node.js native addon.

The Node binding for [TARS](https://github.com/leocaolab/tars). Same shape as
[`tars-py`](../tars-py): a compiled extension wrapping TARS's middleware
pipeline (cache, retry, telemetry, validation) so TypeScript / Node code can
use one Rust-backed handle for any provider — `claude` / `openai` / `gemini` /
`vllm` / `mlx` / `llamacpp` / `claude_cli` / `gemini_cli` / `claude_sdk` /
`codex_cli`.

> **Status — `0.2.0-alpha.0` (alpha)**
>
> The build pipeline (cargo + napi-build → `libtars_node.dylib` →
> `napi build` → `.node` + `index.js` + `index.d.ts`) is in place and
> `complete()` drives a real provider through TARS's middleware chain
> (a concrete `LlmService`). The role spine (`init` / `provider(role)` /
> `pipeline(role)`) resolves roles against the process-global config.
> The public surface is still **provisional** and may change before a
> stable release.

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

### Validating against a JSON Schema

The Rust `decode::<T>` seam is Rust-only; from Node the schema-valid path is:

1. **Decode-time enforcement — `responseSchema`.** Hand the JSON Schema to
   the provider's structured-output mode; a strict-capable provider
   (Gemini / OpenAI / Anthropic) is *forced* to emit conforming JSON, so
   `result.text` parses cleanly. (A local LM Studio model may reject
   `response_format` — fall back to a JSON-forcing prompt.)
2. **Parse + shape-check in your code.** Unlike `tars-py`, the Node binding
   has **no in-pipeline validators** yet — run `JSON.parse(result.text)`
   then validate the shape yourself (plain JS, or zod / ajv):

   ```ts
   const result = await pipeline.complete({ model, user, responseSchema: SCHEMA });
   const data = JSON.parse(result.text);
   if (!Number.isInteger(data.severity)) throw new Error('severity must be int');
   ```

Runnable end-to-end example (record-once / replay-forever via a cassette —
no live model needed):
[`examples/node/schema-validation.mjs`](../../examples/node/schema-validation.mjs).

### Deterministic tests — cassette replay

`type = "cassette"` in the config wraps a real provider and records
`(request → events)` to a JSON file, then replays it forever (VCR pattern) —
so tests/examples run with no live model. A request the recording doesn't
cover is a hard **MISS error**, never a silent re-call. Record and replay
compute the *same* request fingerprint across bindings, so a cassette
recorded from `tars-py` replays byte-identically through `tars-node`.

```
TARS_CASSETTE_RECORD=1 node examples/node/schema-validation.mjs  # record (live)
node examples/node/schema-validation.mjs                          # replay (offline)
```

## Why a separate binding?

Same answer as `tars-py`: every layer above the provider — cache, retry,
telemetry, validation, circuit breaker, budget — is
Rust code we don't want to re-implement in TypeScript. The binding
hands a single handle (`Pipeline`) across the FFI boundary; the Node
caller never sees the middleware chain, just `complete(opts) → Promise<result>`.

## Roadmap

| Milestone | What lands |
|-----------|------------|
| ✅ done | scaffold + build pipeline, real `complete()` through the middleware chain, role spine (`init` / `provider` / `pipeline`) |
| next | `npm publish` of platform-tagged tarballs (darwin x64/arm64, linux gnu x64/arm64) |
| later | streaming (`stream()` → AsyncIterator over ChatEvent) |
| later | `run_task(...)` over the DAG executor (Doc 04 §4) |
| later | tool calling, tool_choice, structured tool results |
| later | per-call cancellation token |

Out of scope for the napi binding: spawning agents (use the Rust
runtime in-process); training / fine-tuning (not in TARS scope at all).

## Internals

`src/lib.rs` mirrors `crates/tars-py/src/lib.rs` one-for-one:

- Process-wide tokio runtime, lazy `OnceLock<Runtime>` — same pattern
  as `tars-py::TOKIO`. Each `complete()` blocks on this runtime via
  `napi-rs`'s async glue; the JS caller's event loop is never
  serialised.
- `Pipeline` napi class holds a concrete `LlmService` once `from_config_path`
  resolves the provider through `ProviderRegistry::from_config_default` +
  `LlmService::default_chain`.
- Request building (`CompleteOptions` → `ChatRequest`) is in-line —
  the napi-friendly camelCase struct maps 1:1 to the snake_case
  ChatRequest fields. JSON Schemas pass through as opaque
  `serde_json::Value` (no per-shape TS types yet).
- Errors map `ProviderError` / `RuntimeError` / `ConfigError` to napi's
  `napi::Error`, which surfaces to JS as a rejected `Promise`.
