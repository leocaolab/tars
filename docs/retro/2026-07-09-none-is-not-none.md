# Retro — `None` is not none: a default hiding behind an absent value

**Date:** 2026-07-09
**Found while:** wiring arc's critic to send its own JSON Schema, after noticing that
`response_json_schema()` — built, correct, tested — had zero callers.

---

## What happened

`tars-runtime/src/worker.rs:358-363`:

```rust
if !has_tools {
    match self.persona.as_ref().and_then(|p| p.output_schema.as_ref()) {
        Some((name, schema)) => pb = pb.structured_output(name.clone(), schema.clone()),
        None => pb = pb.structured_output("WorkerResult", worker_json_schema()),
    }
}
```

`WorkerPersona.output_schema: Option<(String, Value)>`. `None` does **not** mean "send no
schema". It means tars sends its own generic one: `worker_json_schema()` (`worker.rs:843`) =
`{summary: string, confidence: number}`, `additionalProperties: false`, both required.

Four call sites in arc wrote `None`, and they did not agree on what they were asking for.
(Fourteen more, in concer, had not been looked at yet — see *The blast radius* below.)

| call site | what the comment said | what happened |
|---|---|---|
| arc's critic (`critic/agent.rs`) | `// No output schema: … would over-constrain it` | the provider was asked for `{summary, confidence}` while the critic parsed findings out of the reply |
| arc's L5 rot tribunal (`verifier/judge.rs`) | `// parse_verdict is tolerant/fail-safe, so no structured-output schema is imposed` | same — while `decode::<VerdictFields>` required `{verdict, reasoning, proposal}` |
| arc's fixer / merger | (no persona at all) | they always carry tools, so the `if !has_tools` guard skipped |
| arc's verifier (`verifier/agent.rs`) | `Some(("Verdict", verdict_output_schema()))` | correct |

Two bugs. One accidental survivor. One correct — and the correct one is correct because its
author happened to have a schema in hand, not because the type helped.

**Both bugs are silent on every provider we run today.** `[roles] critic = "deepseek"`, and
deepseek is `StructuredOutputMode::JsonObjectMode`, so `openai/adapter.rs:325` **discards the
schema** and emits only `response_format: {"type":"json_object"}`. Point either role at a
`StrictSchema` provider — `openai`, `gemini`, `vllm`, `claude_sdk` — and the provider enforces
`{summary, confidence}`:

- the critic returns **zero findings** — a false-clean code review;
- the rot tribunal's every decode fails and `parse_verdict` fail-safes to `ESSENTIAL`, so **no
  rot verdict ever fires a refactor**.

Both failures are the quiet kind. Neither crashes. Neither logs. Both look like "the model
found nothing".

## Why the fixer and merger survived

```
arc:  is_native_agent            = { claude_cli, gemini_cli, codex_cli }
                                   (arc_types/src/config.rs:259-261)
tars: StructuredOutputMode::None = { claude_cli, gemini_cli, codex_cli,
                                     opencode, antigravity }
      claude_sdk is StrictSchema — and is NOT native
```

native ⇒ `NativeLoop` ⇒ empty tool registry ⇒ `has_tools == false` ⇒ schema imposed.
non-native ⇒ `ReadWrite` ⇒ `has_tools == true` ⇒ schema not imposed.

So a native provider is always handed a schema — and every native provider happens to be one that
throws schemas away. The fixer survives on that, and **only** on that.

Note the shape of the relation. It is **not** equality: `opencode` and `antigravity` are
`None`-mode too — they route through `cli_delegate_capabilities()`
(`tars-provider/src/registry.rs:230`) → `text_only_baseline()` →
`StructuredOutputMode::None` (`tars-types/src/capabilities.rs:80`). The load-bearing fact is an
**inclusion**:

```
is_native_agent  ⊆  StructuredOutputMode::None
```

Two independent enums, in two repositories, and one is a subset of the other by coincidence.
Nothing enforces it. Nothing even *states* it. An inclusion is a weaker, quieter invariant than
an equality — an equality you might notice when you break it, because both sides move. This one
breaks the day a single native CLI gains structured output, on the other side of a repository
boundary, and the fixer silently inherits the critic's bug.

*(The inclusion is strict, and that gap is itself a live bug — in the opposite direction. arc
maps `opencode` and `antigravity` to `ProviderType::Other`, so `is_native_agent()` is `false`,
so the fixer hands them arc's tars tool registry — while `tars-provider/src/registry.rs:225-229`
says of exactly those two: "the delegate runs its OWN agent loop." Set `fixer = "opencode"` today
and it flails on tool names it cannot call. Tracked in arc's config redesign.)*

## The principle — a two-state type carrying three meanings

`Option<T>` has two states. This knob has three:

1. **send nothing**
2. **send tars's generic schema**
3. **send mine**

`Some(x)` covers (3). `None` was made to cover both (1) and (2), and the code resolves the
ambiguity by silently choosing (2). The type says *absent*; the behaviour is *a specific value
you did not pick*.

That is a sentinel. arc's own review rubric names it, in
`rubrics/rust_best_practices.yaml::default-substituted-for-absent-or-failed`:

> instead of panicking, an `Option`/`Result` is unboxed with a **DEFAULT that stands in for a
> `None`** … The tell: **the default is a SENTINEL indistinguishable from a real value** … so
> the program proceeds on fabricated data and **the failure detonates far away as corruption —
> silently, because every default compiles.**

We shipped it in the code that runs the rubric.

## The tell was written down, twice, and nobody checked it

The two broken call sites each carry a comment asserting a behaviour the code contradicts, three
lines away, in another crate:

> `// No output schema: the multi-finding output is multi-shaped and the typed decode is
> tolerant — provider-enforced structured output would over-constrain it`

> `// parse_verdict is tolerant/fail-safe, so no structured-output schema is imposed`

A comment stating what `None` does is evidence that `None` does not obviously do it. Both authors
reasoned about the *intent* of the argument and never read the *arm* that consumes it. When a
comment explains an `Option`'s `None` branch, read the `None` branch.

## The blast radius, once the compiler could see it

We knew of two bugs. The three-state enum turned every existing `None` into a compile error, and
each author was forced to re-decide once. What came out:

| call site | what the author meant | what had been happening |
|---|---|---|
| arc's critic scan | `Custom("CriticResponse", …)` | `{summary, confidence}` |
| arc's L5 rot tribunal | `Custom("JudgeVerdict", …)` | `{summary, confidence}` |
| arc's `review_stateful_async` (the reducer) | **no schema** — its `{uid: patch}` shape is decoded tolerantly | `{summary, confidence}` |
| arc's `delegate_agent` | **no schema** | `{summary, confidence}` |
| concer × **14** — writing, proofreading, and reply personas | **no schema** | `{summary, confidence}` |

**Eighteen call sites** were silently instructed to emit `{summary, confidence}`. concer is a
*writing* tool: fourteen of its prose personas were being told the only legal reply was a summary
and a confidence number.

Every one survived for the same reason: their providers throw the schema away. `deepseek` is
`JsonObjectMode`; `claude_cli` is `None`-mode. Move any of them to `gemini`, `openai`, `vllm`, or
`claude_sdk` — all `StrictSchema` — and it breaks on the first call.

Two of those eighteen we found by reading. Two more, and all fourteen of concer's, we found only
because the type stopped allowing the question to go unanswered. **A refactor cascade is not a cost
of the fix; it is the fix showing you its blast radius.**

## The fix

```rust
pub enum OutputSchema {
    None,                              // send no schema
    WorkerResult,                      // send tars's generic schema — say so deliberately
    Custom(String, serde_json::Value), // send mine
}
pub struct WorkerPersona { pub system_prompt: String, pub output_schema: OutputSchema }
```

There is no "unspecified" state left to default. Every existing `None` becomes a compile error,
and each author is forced to re-decide once. The bug becomes unrepresentable.

Note the level. `WorkerAgent.persona: Option<WorkerPersona>` stays an `Option`: *no persona* is
a real, meaningful absence — it is tars's own generic worker, for which `WorkerResult` is the
right schema. The outer `Option` is honest. Only the inner one lied.

**The second silence, fixed with it.** `if !has_tools` discards an explicitly supplied schema
when the worker carries tools. The reason is real (providers reject `response_format` alongside
tools) but the discard was invisible to the caller who wrote `Custom(..)`. Trading one silence
for another is not a fix: the drop must be surfaced, at the earliest honest point.

## The smell to watch for

When you type a knob as `Option<T>`, ask what the `None` arm *does*. If the answer is anything
other than "nothing", it is not `None` — it is a default, and it belongs in the type as a named
variant. `Option` is for absence, not for "the usual one".

And a corollary that cost us two silent bugs: **a wrong argument that the provider happens to
discard is still a wrong argument.** It works until someone changes providers. The only test
that finds this class of defect is a matrix across `StructuredOutputMode::{None, JsonObjectMode,
StrictSchema}` — which is now how the critic path is accepted.
