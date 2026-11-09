# tars documentation

| Document | What's inside |
|---|---|
| [`USER-GUIDE.md`](./USER-GUIDE.md) | Getting started: install, config, checking your setup, the call shapes, decoding a structured reply, typed errors, when not to use tars. |
| [`providers/`](./providers/) | One page per provider that needs more than an API key: Anthropic, and the subscription CLIs (claude, codex, opencode, antigravity) — how each is invoked, what it can and cannot do, how it is sandboxed. |
| [`recipes/cost-and-reliability.md`](./recipes/cost-and-reliability.md) | Retry, per-call and per-tenant budgets, rate limits and the circuit breaker, and how to stack them. |
| [`recipes/batch.md`](./recipes/batch.md) | Batch APIs (about half the price, up to a 24 h SLA) behind one trait. |
| [`observability.md`](./observability.md) | Seeing what a call did: the Parquet call log, `tars events`, and the live tracing stream. |
| [`eval-methodology.md`](./eval-methodology.md) | What a corpus replay and judge pass measure, and how to compare two runs with the right statistics. |

[CHANGELOG.md](../CHANGELOG.md) is the record of what shipped. If a document
disagrees with the code, the code is right and the document is a bug.
