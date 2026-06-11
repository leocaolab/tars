# Python examples

Runnable Python demos built on the tars binding (`crates/tars-py`). Build it once
with `maturin develop --release` from that crate, then follow each demo's README.

| Example | What it shows |
|---|---|
| [`interview-sim/`](./interview-sim/) | A blackboard multi-agent loop (system-design interview simulator) — provider-agnostic completion, native structured output (`response_schema=`), and the Doc 19 blackboard pattern. The canonical "build a real agent system on tars" demo. |
| [`agent-engine-proto/`](./agent-engine-proto/) | An early, framework-only sketch of a blackboard agent engine (store / event bus / evaluator interfaces). Reference reading, not wired to tars — kept for the design lineage. |

See [`../../docs/USER-GUIDE.md`](../../docs/USER-GUIDE.md) for the API these use.
