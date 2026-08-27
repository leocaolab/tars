# tars — one way to call a language model

A Rust library for making LLM calls, with the parts you end up writing yourself
either way: a dozen providers behind one trait, a middleware pipeline, typed
errors, and a record of every call that you can read afterwards.

It is a **caller**, not a framework. There is no agent loop here, no planner, no
orchestrator. You write the control flow; tars makes the call and tells you the
truth about what happened.

```rust
let pipeline = LlmService::builder(provider, "claude-sonnet-4-5").build();
let reply = pipeline.complete(ChatRequest::user("Say hi in five words"), ctx).await?;
```

## What you get

**One trait, a dozen providers.** Direct API (OpenAI, Anthropic, Gemini,
DeepSeek), any **OpenAI-compatible** endpoint via `base_url` (Groq, xAI,
OpenRouter, LM Studio, Ollama, …), local models (vLLM, MLX, llama.cpp),
**subscription CLIs** (claude / gemini / codex / opencode), and **keyless AWS
Bedrock**. Swapping providers does not touch your call sites.

**Model facts are DATA, not code.** Model ids, prices, context windows and
thinking-mode live in
[`crates/tars-config/data/models.toml`](crates/tars-config/data/models.toml).
Refreshing a price is a data edit, not a recompile, and cost is resolved **per
model** from the reply's actual model rather than from what you asked for.

**A composable middleware pipeline.** Telemetry → budget → cache → validation →
retry → breaker → rate limit. Each is a layer you add or leave out.

**Typed all the way down.** Typed errors rather than strings, a **pre-flight
capability check** (catch tool-use against a non-tool model before the round
trip), and a decode seam — hand it a `T`, get a valid `T` or a typed error.

**Every call is recorded, and the record is readable.** `pipeline_events.db`
holds one row per call; `llm_records.db` holds the request and response bodies
verbatim. `tars events`, `tars trajectory` and `tars run-report` read them. This
is the part most stacks skip and then wish they had at 3am.

**Deterministic tests.** The cassette provider records a real call once and
replays it forever, so a test suite runs with no key, no cost and no variance.
When a replay misses, you get a located diff of what changed in the request —
not "re-record and hope".

**Evaluation with real statistics.** `tars-eval` carries an LLM judge with an
anti-incest guard (the judge's provider may not be the one under test), Wilson
intervals, **McNemar's paired test** for "did this change actually help",
metamorphic relations for domains with no ground truth, and tool-trajectory
scoring.

**Python and Node bindings** (PyO3 / napi-rs), and a CLI whose nine subcommands
all *inspect*: `probe`, `bench`, `trajectory`, `run-report`, `eval`, `models`,
`providers`, `init`, `events`.

## Quick start

```bash
cargo run -p tars-cli -- init          # writes $TARS_HOME/config.toml
export ANTHROPIC_API_KEY=sk-ant-...    # or OPENAI_API_KEY / GEMINI_API_KEY / …
cargo run -p tars-cli -- providers     # check what resolved
```

Then read [`docs/USER-GUIDE.md`](docs/USER-GUIDE.md).

## What tars is not

**Not an agent framework.** It does not plan, does not loop, does not decide
what to do next. If you want an agent, write the loop — the library is designed
to be called from one, and the recording is there precisely so you can debug the
loop you wrote.

**Not a router.** It will not pick a model for you. Provider selection is a
decision you make in configuration, where you can see it.

**Not a vector store, not a RAG kit.** Retrieval is your application's problem
and your application knows the corpus.

## Layout

```
tars-types      the shared vocabulary — requests, events, errors, ids
tars-provider   the LlmProvider trait and every backend
tars-pipeline   LlmService + the middleware layers
tars-cache      response cache
tars-melt       telemetry: metrics, the event stores, the trajectory record
tars-storage    the SQLite stores under those
tars-tools      the Tool trait + built-ins (fs, bash, web) behind an approval sink
tars-sandbox    OS write-jail — macOS Seatbelt / Linux bubblewrap
tars-eval       judges, statistics, metamorphic relations, trajectory scoring
tars-config     configuration, the model library, provider definitions
tars-cli        the `tars` binary
tars-py         Python bindings          tars-node   Node bindings
tars-bedrock    AWS SigV4 for Bedrock    tars-utils  pure helpers
```

Each crate has a `README.md` that is a **placement contract**: its role, its
effect budget, the dependencies it may and may not take, and the one reason it
should ever change. If you are wondering where a change belongs, that is where
the answer is.

## Status

[CHANGELOG.md](CHANGELOG.md) is the record of what shipped. If a doc here
disagrees with the code, the code is right and the doc is a bug.

## Licence

See [LICENSE](LICENSE).
