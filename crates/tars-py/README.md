# tars

Rust-backed multi-provider LLM runtime exposed as a Python package.

This is the Python binding for [TARS](https://github.com/leocaolab/tars). The compiled extension wraps TARS's middleware pipeline (cache, retry, telemetry, validation) so Python code can use one Rust-backed handle for any provider — `claude` / `openai` / `gemini` / `vllm` / `mlx` / `llamacpp` / `claude_cli` / `gemini_cli` / `codex_cli`.

## Build

```bash
# one-time per env
pip install maturin   # or: uv tool install maturin

# build + install in current Python env (development)
cd crates/tars-py
maturin develop --release

# build a redistributable wheel
maturin build --release   # → target/wheels/tars-*.whl
```

## Use

```python
import tars

# Pipeline = provider + middleware (telemetry / cache / retry).
# Layer-1 raw `Provider` also available if you want to bring your own.
p = tars.Pipeline.from_default("anthropic")

resp = p.complete(
    model="claude-sonnet-4-5",
    system="You are a precise technical reviewer.",
    user="Review this Rust function for race conditions: ...",
    max_output_tokens=2000,
    thinking=True,
)

print(resp.text)
print(resp.usage)        # input / output / cached / thinking tokens
print(resp.telemetry)    # cache_hit, retry_count, layer trace, latency
```

### Output validators

Attach Python callbacks that run after the model reply, before the response reaches your code. Validators can pass, reject, filter the text, or annotate metrics — the chain runs in order, each validator sees the previous one's filtered output.

```python
import tars

def must_be_json(req, resp):
    import json
    try:
        json.loads(resp["text"])
        return tars.Pass()
    except ValueError as e:
        return tars.Reject(reason=f"not JSON: {e}")

def strip_pii(req, resp):
    cleaned = resp["text"].replace(USER_EMAIL, "[REDACTED]")
    return tars.FilterText(text=cleaned, dropped=["email"])

p = tars.Pipeline.from_default(
    "anthropic",
    validators=[
        ("strip_pii", strip_pii),
        ("must_be_json", must_be_json),
    ],
)

# tars.Reject(...) → TarsProviderError(kind="validation_failed",
#                                       is_retriable=False)
# Always Permanent (W4 cut the retriable flag). RetryMiddleware does
# not retry on validation failures — caller handles model resampling
# at their own layer with prompt variation.
```

A buggy validator that raises a Python exception is caught by the
adapter and surfaced as a permanent `TarsProviderError(kind="validation_failed")` — the worker is never crashed by user-side bugs.

> **Note:** inside a validator, `req` / `resp` are **dict views**
> (`resp["text"]`, `resp["tool_calls"]`, …). The `Response` object that
> `complete()` *returns* is different — there you use attribute access
> (`resp.text`, `resp.usage`).

### Validating against a JSON Schema

The Rust `decode::<T>` seam (envelope/fence-scrape → typed `T`) is
Rust-only; from Python there are two complementary ways to get a
schema-valid result:

1. **Decode-time enforcement — `response_schema=`.** Hand the JSON Schema
   to the provider's structured-output mode. A strict-capable provider is
   *forced* to emit conforming JSON, so `resp.text` is clean by
   construction:

   ```python
   resp = p.complete(
       model="claude-sonnet-4-5",
       user="Rate this diff.",
       response_schema={
           "type": "object",
           "properties": {"severity": {"type": "integer"}, "summary": {"type": "string"}},
           "required": ["severity", "summary"],
       },
       response_schema_strict=True,   # False → schema is a hint, not enforced
   )
   review = json.loads(resp.text)     # clean → parse straight through
   ```

2. **Post-hoc validator (defense in depth).** A validator callback that
   parses + shape-checks and `Reject`s on mismatch. There is **no** built-in
   schema validator on the Python side — write the check with plain Python,
   or `jsonschema` / pydantic:

   ```python
   def validate_schema(req, resp):
       try:
           data = json.loads(resp["text"])          # dict view → resp["text"]
       except json.JSONDecodeError as e:
           return tars.Reject(reason=f"not JSON: {e}")
       if not isinstance(data.get("severity"), int):
           return tars.Reject(reason=f"severity must be int; raw={resp['text'][:80]!r}")
       return tars.Pass()

   p = tars.Pipeline.from_default("anthropic", validators=[("schema", validate_schema)])
   ```

Runnable end-to-end example (record-once / replay-forever via a cassette,
so it needs no live model):
[`examples/python/structured-output/schema_validation.py`](../../examples/python/structured-output/schema_validation.py).

### Pre-flight capability check

Verify each agent role's configured provider can satisfy its needs at startup, instead of failing at runtime on first request:

```python
roles = {
    "planner":  tars.CapabilityRequirements(requires_thinking=True),
    "executor": tars.CapabilityRequirements(requires_tools=True,
                                             estimated_max_output_tokens=8000),
    "reviewer": tars.CapabilityRequirements(requires_structured_output=True),
}

for role, reqs in roles.items():
    p = tars.Pipeline.from_default(provider_for(role))
    r = p.check_capabilities(reqs)
    if not r:
        print(f"role={role!r} can't satisfy: {[x.kind for x in r.reasons]}")
        sys.exit(1)
```

### Typed errors, not strings

```python
try:
    p.complete(model="...", user="...")
except tars.TarsRoutingExhaustedError as e:
    # e.skipped_candidates: list[tuple[provider_id, list[CompatibilityReason]]]
    for pid, reasons in e.skipped_candidates:
        log.warn(f"{pid} skipped: {[r.kind for r in reasons]}")
except tars.TarsProviderError as e:
    if e.kind == "rate_limited":
        await asyncio.sleep(e.retry_after or 30)
    elif e.kind == "validation_failed" and not e.is_retriable:
        log.fatal(f"validator rejected output permanently: {e}")
    elif e.kind == "unknown_tool":
        log.fatal(f"register tool {e.tool_name}")
```

Hierarchy: `TarsError` → `TarsConfigError` / `TarsProviderError` / `TarsRuntimeError`. Subclasses (e.g. `TarsRoutingExhaustedError`) for variants needing structured access; generic catch-all (`except TarsProviderError`) still matches.

## Tests

Integration tests live in `python/tests/`. They expect a local LM Studio (or any other `qwen_coder_local`-compatible) provider on `127.0.0.1:1234`:

```bash
maturin develop --release
uv run --with pytest python -m pytest crates/tars-py/python/tests
```

## Status

`Provider`, `Pipeline`, `Session`, `CapabilityRequirements`, `CompatibilityResult`, output validators (Pass / Reject / FilterText / Annotate), and the typed-error hierarchy are live. See the workspace [CHANGELOG](../../CHANGELOG.md) for per-milestone shipped detail.
