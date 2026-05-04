# tars

Rust-backed multi-provider LLM runtime exposed as a Python package.

This is the Python binding for [TARS](https://github.com/moomoo-tech/tars). The compiled extension wraps TARS's `LlmService` so Python code (initially: ARC and agouflow) can use one Rust-backed handle for any provider — claude / openai / gemini / vllm / mlx / llamacpp / claude_cli / gemini_cli / codex_cli — wired through TARS's full pipeline (cache, retry, circuit breaker, routing).

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
print(tars.version())
```

This skeleton commit only exposes `version()` — real `Client` API lands in the next commit.
