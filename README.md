# tars — one way to call a language model

[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-1.99%2B-orange.svg)](./rust-toolchain.toml)

A Rust library for making LLM calls, with the parts you end up writing yourself
either way: a dozen providers behind one trait, a middleware pipeline, typed
errors, and a record of every call that you can read afterwards.

It is a **caller**, not a framework. There is no agent loop here, no planner, no
orchestrator. You write the control flow; tars makes the call and tells you the
truth about what happened.

```rust
let svc = LlmService::builder(provider, "claude-sonnet-5").build();
let reply = tars_llm::complete_async(svc, ChatRequest::user("Say hi in five words"), caller).await?;
```

## What you get

**One trait, a dozen providers.** Direct API (OpenAI, Anthropic, Gemini,
DeepSeek), any **OpenAI-compatible** endpoint via `base_url` (Groq, xAI,
OpenRouter, LM Studio, Ollama, …), local models (vLLM, MLX, llama.cpp), and
**subscription CLIs** (claude / gemini / codex / opencode / antigravity).
Swapping providers does not touch your call sites.

**Sandboxed by default.** Every subscription-CLI delegate runs in an OS
write-jail (macOS Seatbelt / Linux bubblewrap). With no `[sandbox]` table in the
config a model call is read-only; `workspace-write` and `danger-full-access` are
explicit opt-ins.

**Model facts are data, not code.** Model ids, prices, context windows and
thinking-mode ship in
[`crates/tars-config/data/provider.toml`](crates/tars-config/data/provider.toml);
`tars models update` lays each provider's live model list and limits over it,
so a new model or a changed limit takes effect without a tars release. Write
`default_model = "flash@latest"` to follow the newest model in a series.

**A composable middleware pipeline.** Event recording → telemetry → validation →
retry, with budget, per-tenant budget and rate-limit layers you add when you
want them, and a circuit breaker that wraps the provider below retry. Each is a
layer you add or leave out.

**Typed all the way down.** Typed errors rather than strings, a **pre-flight
capability check** (a request the provider cannot serve fails before the round
trip), and a decode seam — hand it a `T`, get a valid `T` or a typed error.

**Every call is recorded, and the record is readable.** Each call lands one
`llm_call` row in a Parquet call log, with the request and response bodies
beside it. `tars events list` / `show` read them back. This is the part most
stacks skip and then wish they had at 3am.

**Deterministic tests.** The cassette provider records a real call once and
replays it forever, so a test suite runs with no key, no cost and no variance.
When a replay misses, you get a located diff of what changed in the request.

**Evaluation with real statistics.** `tars-eval` carries an LLM judge that may
not run on the provider under test, Wilson intervals, **McNemar's paired test**
for "did this change actually help", metamorphic relations for domains with no
ground truth, and tool-trajectory scoring.

**Node bindings** (napi-rs), and a CLI whose subcommands all *inspect*:
`probe`, `bench`, `eval`, `models`, `providers`, `init`, `events`, `config`.

## Quick start

```bash
git clone https://github.com/leocaolab/tars.git && cd tars
cargo run -p tars-cli --bin tars -- init          # writes $TARS_HOME/config.toml (default ~/.tars)
export ANTHROPIC_API_KEY=sk-ant-...               # or OPENAI_API_KEY / GEMINI_API_KEY / DEEPSEEK_API_KEY
cargo run -p tars-cli --bin tars -- providers     # configured providers + key-env health
cargo run -p tars-cli --bin tars -- probe anthropic --tenant me --prompt "Say hi in five words."
```

Local and subscription-CLI providers need no key. Then read
[`docs/USER-GUIDE.md`](docs/USER-GUIDE.md).

## What tars is not

**Not an agent framework.** It does not plan, does not loop, does not decide
what to do next. If you want an agent, write the loop — the library is designed
to be called from one, and the recording is there precisely so you can debug the
loop you wrote.

**Not a router.** It will not pick a model for you. Provider selection is a
decision you make in configuration, where you can see it: a role in `[roles]`
names one provider and one model, and a role the config does not list is an
error, never a fallback.

**Not a vector store, not a RAG kit.** Retrieval is your application's problem
and your application knows the corpus.

## Layout

```
tars-types      the shared vocabulary — requests, events, errors, ids
tars-llm        the LlmProvider trait, every backend, LlmService and its middleware
tars-melt       the event log: the call, wire and journal tables and the Parquet adapter
tars-storage    how tars opens a SQLite file
tars-telemetry  the process's tracing subscriber, with optional OTLP export
tars-tools      the Tool trait + built-ins (fs, bash, web) behind an approval sink
tars-sandbox    OS write-jail — macOS Seatbelt / Linux bubblewrap
tars-eval       cassette diff, judges, statistics, metamorphic relations, trajectory scoring
tars-config     configuration, the model library, provider definitions
tars-cli        the `tars` binary
tars-node       Node bindings
tars-utils      pure helpers
```

Each crate has a `README.md` that is a **placement contract**: its role, its
effect budget, the dependencies it may and may not take, and the one reason it
should ever change. If you are wondering where a change belongs, that is where
the answer is.

## Status

[CHANGELOG.md](CHANGELOG.md) is the record of what shipped; 2.0 regrouped the
crates (see its table). If a doc here disagrees with the code, the code is right
and the doc is a bug.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
