# two_sum_v4 — the PULL / context-request interface

**This is an interface experiment, not an intelligence test.** The
question is not whether the model solves Two Sum; it's whether a *pull*
context protocol functions, what it costs, and what its failure mode is.

Same harness / tools / World / input as `two_sum`..`two_sum_v3`. The one
change is **who controls context**. The earlier examples auto-carry the
whole trajectory. v4 inverts that: the harness **drops all history**, and
each turn the model sees only

```
system + task + the results of its IMMEDIATELY PRECEDING turn
```

To get anything older back, the model must **pull** it. Every observation
is auto-saved to a memory store, and two tools expose it:

- `recall_keys()` — list what is saved,
- `recall(key)` — bring one past observation back into context.

If the model doesn't recall what it needs, it simply doesn't have it.

Run:

```bash
set -a; source .env; set +a
cargo run -p tars-examples --example two_sum_v4
POC_DUMP=1 cargo run -p tars-examples --example two_sum_v4   # watch context reset each turn
```

## Result: the mechanism functions; the model never drove it → thrash → MAX_TURNS

Two real runs against `gemini_flash`, both **hit MAX_TURNS (24) with a
failure, zero recall calls**:

| run | recall/recall_keys calls | array_get calls | store entries | outcome |
|---|---|---|---|---|
| 1 | **0** | 7 | 30 | ✗ MAX_TURNS, no submit |
| 2 | **0** | 20 (≈5 full re-scans) | 24 | ✗ MAX_TURNS, no submit |

**Harness side of the pull protocol works** — the `POC_DUMP` trace shows
the context correctly collapsing to 3 messages every turn, and the store
faithfully accumulating 24–30 observations. The `recall_keys` / `recall`
tools are advertised (turn-1 dump below) and their serve logic is sound.

**But the model never issued a single pull request.** So the end-to-end
`request → serve → re-enter` path was never traversed by the agent — and
that is the headline. With its own history dropped and no recall, the
model **lost its cursor and thrashed**: it re-read `array_len`,
`array_get(0)`, `array_get(1)`, then looped back to `array_len` /
`array_get(0)` again and again, never advancing to index 2/3, never
computing a sum, never calling `submit_answer` — until it exhausted
MAX_TURNS.

Turn 1 — the pull tools ARE offered and the system prompt demands them:

```
┌─ POC_DUMP: context for turn 1 ─
│ SYSTEM: … you have NO memory of earlier turns. Each turn you see ONLY the
│         results of your MOST RECENT tool calls … call recall_keys() to list
│         what is saved, and recall(key) to bring a past observation back …
│ TOOLS advertised: … recall_keys — List the keys of every observation …
│                   recall — Retrieve a saved observation from memory by key …
│ MESSAGES (1 in context):
│   [0] user: Solve Two Sum …
└─
── Turn 1 ──  array_len({})           → 4     (context msgs sent: 1)
```

Context stays minimal every turn (proof the history is dropped):

```
┌─ POC_DUMP: context for turn 3 ─
│ MESSAGES (3 in context):
│   [0] user: Solve Two Sum …
│   [1] assistant: tool_calls: array_get({"index":0})
│   [2] tool: [array_get@…] 2
└─
── Turn 3 ──  array_get({"index":1})  → 7     (context msgs sent: 3)
```

…and the thrash (history dropped, no recall → cursor lost):

```
── Turn 18 ──  array_get({"index":0})  → 2
── Turn 19 ──  array_get({"index":1})  → 7
── Turn 20 ──  array_get({"index":0})  → 2     ← re-reading the same cells
── Turn 21 ──  array_get({"index":1})  → 7
… (loops until MAX_TURNS 24, no submit_answer)

✗ FAILURE MODE — agent hit MAX_TURNS (24) without a terminal submit_answer:
  with history dropped it did not pull what it needed.
```

## The interface finding

**The pull interface's cost is real and lands entirely on the agent: it
must know what to ask for, and ask, or it fails.** A minimal-context /
pull design does not "just work" because the tools exist — this model,
even when explicitly instructed that its memory is gone and told the exact
tools to recover it, did not switch from *acting* to *retrieving*. It kept
issuing forward actions against a context that no longer remembered them,
and looped. Dropping history without the agent reliably pulling is not
economy; it's a cursor-loss trap.

## One line for the production runner

If you want a pull / minimal-context design, the runner must own **the
recall protocol AND minimal-context assembly** — and, because the model
won't reliably pull on its own, it must also own **cursor/progress state
itself** (what's been done, what's next), injected each turn, rather than
trusting the agent to reconstruct it via recall.
