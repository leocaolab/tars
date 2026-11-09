# two_sum_v5 — the PUSH / full-memory interface

**This is an interface experiment, not an intelligence test.** The
opposite pole of [`two_sum_v4`](../two_sum_v4). The question is the
behavior and COST of retaining *everything*.

Same harness / tools / World / input as `two_sum`..`two_sum_v4`. Here the
harness keeps an explicit `Memory` — an append-only list of ALL
observations (every tool call, every result, plus the model's own notes)
— and re-injects the WHOLE thing, in full, every turn, rendered as one
growing block:

```
system + [ task + "=== MEMORY (full, unabridged — N observations) === … " ]
```

Nothing is dropped, nothing summarised. Two things to watch: the **cost**
(context growth) and the **bypass** (does the model, given everything up
front, solve in its head and ignore the instrumented tools, like
`two_sum_v3` did?).

Run:

```bash
set -a; source .env; set +a
cargo run -p tars-examples --example two_sum_v5
POC_DUMP=1 cargo run -p tars-examples --example two_sum_v5   # watch MEMORY balloon
```

## Result 1 — the cost: context grows unbounded (visible balloon)

The protocol functions and the model solves the task, but the per-turn
context grows monotonically for the entire run. From one real run (the
program prints this curve at the end):

```
── context growth curve (turn: MEMORY chars / provider input_tokens) ──
  turn  1:   347 chars    461 in-tok  ███████████
  turn  4:  1352 chars    722 in-tok  ██████████████████
  turn 10:  1654 chars    851 in-tok  █████████████████████
  turn 17:  1983 chars   1010 in-tok  █████████████████████████
  turn 18:  4059 chars   1566 in-tok  ███████████████████████████████████████
  turn 20:  4646 chars   1772 in-tok  ████████████████████████████████████████████
```

Across runs it reaches ~2600 input-tokens / ~7100 chars by turn 20–24.
For a **4-element** array that is already multi-kilobyte context; the
curve has no ceiling — at realistic scale this is the unbounded-context
problem in miniature. The step jumps (e.g. turn 17→18) are the model's
own large reasoning `text` blocks being retained as memory notes: memory
grows from results *and* from the model's narration, in bursts.

## Result 2 — the bypass did NOT happen here (contrary to the hypothesis)

The prediction was that full memory might invite the v3 behavior — solve
in-head, leave the hashmap tools untouched. **It didn't.** Across five
real runs the model **always drove the instrumented tools**:

| run | hashmap tool calls | final HashMap state | outcome |
|---|---|---|---|
| 1 | 7 | `{2:0, 7:1, 11:2, 15:3}` | ✓ PASS (turn 20) |
| 2 | 8 | `{2:0, 7:1, 11:2, 15:3}` | ✓ PASS (turn 18) |
| 3 | 6 | `{2:0, 7:1, 11:2}` | ✓ PASS (turn 12) |
| 4 | 6 | `{2:0, 7:1, 11:2}` | ✗ MAX_TURNS (24) |
| 5 | 6 | `{2:0, 7:1, 11:2}` | ✓ PASS |

`hashmap tool calls > 0` and a non-empty final map every run — the
opposite of v3's frequent empty map. Rendering memory as an explicit
**log of prior tool calls** appears to *encourage* tool use by pattern
continuation: the model sees a running transcript of tool calls and keeps
issuing them. So this PUSH shape did not invite the bypass; instead it
invited **redundant re-work** — the model re-reads and re-puts values it
already has in memory, taking 12–24 turns for a 1-line problem and once
failing to converge inside MAX_TURNS — while the context balloons.

## The interface finding

Full-memory push is the "nothing is lost" guarantee, and it buys
reliability of *information* at the cost of **unbounded context growth**
and **no pressure toward efficiency**. Everything the agent ever saw is
always present, so it never has to be economical — and it isn't: it
re-does work and narrates into the same growing buffer. The failure pole
here is not amnesia (that was v4); it's cost + dilution: a context that
only grows, with the salient facts increasingly buried in a transcript of
redundant actions.

## One line for the production runner

A push / full-retain design needs the runner to own **context budget
management — compaction / summarisation / eviction** — because "retain
everything" has no ceiling and no built-in incentive for the agent to
stop re-deriving what it already knows; without compaction the context
grows until it's expensive, slow, and diluted.
