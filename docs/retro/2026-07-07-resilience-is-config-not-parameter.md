# Retro — resilience is Config, not Parameter (Environment vs Business-Context)

> 2026-07-07. Surfaced while migrating arc + concer onto the `Tars` handle.

## What happened

Retry + circuit-breaker resilience (retry: 6 attempts, 1s→30s backoff ×2, `Retry-After`
respected but capped at 30s; breaker: 4 failures / 30s cooldown) was **hand-copied**
across two consumers — arc's `arc_resilience()` and concer's `concer_resilience` — as
`PipelineOpts.retry` / `.circuit_breaker` passed at pipeline-build time. Two verbatim
copies of the same constants.

Why copies existed: **tars had no config field for resilience.** `RetryConfig` /
circuit-breaker were only settable through `PipelineOpts` in code, never through
`config.toml`. So every real consumer had to hand-code the constants — and they drifted:

- A real bug fell out of the drift — arc's `build_for` path passed `timeout=None` and the
  handle-thin build fell back to tars's *default* retry (3 attempts, no breaker), silently
  halving retries and dropping the breaker for critic/verifier.
- It **blocked** both consumers from adopting the handle: `Tars::pipeline_with` builds
  `default_chain` with only validators, so routing arc/concer through it would have
  regressed their resilience. Two migration rounds stalled on the same wall.

## The principle — one ruler

Whether a knob belongs in **Config** or in a call **Parameter** is decided by *what drives
its change*:

| Driven by… | Goes in… |
|---|---|
| **Environment** (runtime / infrastructure) | **Config** |
| **Business context** (task / intent) | **Parameter** |

### Why resilience → Config

- **Operation-driven, no recompile.** LLM 429 rates and network jitter change on their own.
  Bumping retries 6→10 when a vendor flakes must be a one-line `config.toml` edit, not a
  recompile of arc *and* concer.
- **Physical-property isolation.** Retry is an infra property, not business. Local Ollama
  (no 429, no latency) needs no retry; remote OpenAI needs heavy backoff. `[providers.ollama]`
  vs `[providers.openai]` resilience is natural in config; in code it becomes an ugly
  `if provider == "ollama" { … }`.
- **Consumer-agnostic.** arc reviews code, concer writes docs — neither should know that a
  TCP connection dropped and needs N seconds of backoff. Push it down; consumers just
  `pipeline(role).call()`.

### When it MUST be a Parameter

- **Business-bound values** — `temperature` (0.0 for code review, 0.8 for brainstorm),
  `max_tokens`. These follow the current task flow; freezing them in global config is wrong.
- **Runtime-computed values** — `session_id`, the context document bytes. Can't be static.

## The tars design law (the takeaway)

- **"How to connect and survive"** → **Config**: api-key routing, timeout, retry, circuit
  breaker.
- **"What to do and how to think"** → **Parameter**: prompt, temperature, session context.

## The fix

Move resilience into a `[resilience]` section of tars config (v1.2.3); the handle reads it
and feeds `PipelineOpts`. arc/concer then delete their hand-rolled copies and set the policy
once, in the shared `$TARS_HOME/config.toml`. This didn't just remove duplicate code — it
**returned misplaced infrastructure logic to the infrastructure.**

## The smell to watch for

When a consumer hand-codes an infrastructure constant (retry, timeout, breaker, backoff),
that is a signal the **config layer is missing a field** — not that the consumer needs its
own copy. The reflex should be "add the config field," not "copy the constants."
