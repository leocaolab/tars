# two_sum — loop-react agent driving in-process software objects (incubator)

Sibling of `loop_react`. Same loop harness, native tool-calling, and
two-event / `thought_signature` handling — but the tools are **not file
I/O**. They are a **fn-access-map over live in-process program state**,
and the agent uses them to solve an *algorithmic* task. This exercises
the "app-as-world" idea: the running program IS the agent's environment.

## The software capabilities under test (native Rust tools)

All six operate on a single `World` struct owned by the harness:

| Tool | Backing in-process object |
|---|---|
| `hashmap_put(key, value)` / `hashmap_get(key) -> value \| null` | a **live `Mutex<HashMap<i64,i64>>`** (the capability under test) |
| `array_get(index) -> i64` / `array_len() -> usize` | a read-only `Vec<i64>` — the **object-access-map**: `nums` is NOT in the prompt, the agent must read it through these |
| `println(text)` | real stdout — an **output capability** the agent narrates to |
| `submit_answer(i, j)` | writes the answer into the harness — the **terminal act** |

## The task

Two Sum. Harness holds `nums = [2, 7, 11, 15]`, `target = 9` (answer:
indices `[0, 1]`). The prompt tells the agent the one-pass hashmap
algorithm but the LLM drives every step itself — nothing about the scan
is hardcoded. Correctness is checked against the harness's own `nums`
after the run.

## How to run

```bash
set -a; source .env; set +a      # provides GEMINI_API_KEY
cargo run -p tars-examples --example two_sum
# other native tool-calling provider:
TARS_POC_PROVIDER=openai cargo run -p tars-examples --example two_sum
```

## Verbatim run (gemini_flash, gemini-2.5-flash)

The agent drove the live HashMap + array to a correct one-pass solution.
Note turn 7: the model **ended a turn empty** (0 output tokens, no text,
no tool call) right when it should have looked up the complement — the
harness did NOT treat that as "done" (the terminal `submit_answer` had
not fired), nudged it, and it recovered.

```
harness world: nums=<hidden, 4 elems, tool-only>  target=9
provider: gemini_flash   model: gemini-2.5-flash

── Turn 1 ──  tool_use: array_len({})                    → 4
── Turn 2 ──  tool_use: println({"text":"Array length is 4."})
      [agent stdout] Array length is 4.                  → printed
── Turn 3 ──  tool_use: array_get({"index":0})           → 2
── Turn 4 ──  tool_use: hashmap_get({"key":7})           → null
── Turn 5 ──  tool_use: hashmap_put({"key":2,"value":0}) → ok: map[2] = 0
── Turn 6 ──  tool_use: array_get({"index":1})           → 7
── Turn 7 ──  LLM: stop=EndTurn  tokens in/out=801/0  tool_calls=0
              (no tool_use but submit_answer NOT called yet — nudging to continue)
── Turn 8 ──  tool_use: hashmap_get({"key":2})           → 0
── Turn 9 ──  tool_use: submit_answer({"i":0,"j":1})     → recorded answer: indices [0, 1]
── Turn 10 ─  LLM: stop=EndTurn  tool_calls=0            (answer submitted — done)

══ Result ══
final HashMap state: {2: 0}
submitted answer: indices [0, 1]
✔ PASS — nums[0] + nums[1] = 2 + 7 = 9 == target 9 (valid Two Sum pair)
```

The agent genuinely ran the algorithm through the tools: it stored
`2→0` in the live map, then on seeing `7` looked up `target-7 = 2`, got
index `0` back from the map, and submitted `[0, 1]`.

## New findings for the production loop-react runner

1. **"No tool_use" is NOT "done" — the terminal act is.** This provider
   deterministically ended a turn completely empty (0 output tokens, no
   text, no tool call) mid-solve, at the *same* decision point across
   runs. The naive `empty tool_calls ⇒ done` heuristic (the exact one
   `loop_react` already flagged) drops the answer here. The runner needs
   a **task-completion signal tied to the terminal tool** (submit fired)
   plus a **bounded nudge/continue on an incomplete stop** — an empty
   assistant turn is a stall to recover from, not a finish. This POC is
   the concrete reproduction of that finding.

2. **No parallel tool-calling emerged — the agent stayed strictly
   sequential (`max tool_calls per turn = 1`).** Even though the harness
   loop already supports N calls per turn (it appends one `Message::Tool`
   per call), gemini_flash issued exactly one tool per turn for a task
   whose reads (`array_get(0)`, `array_get(1)`) are independent and could
   have been batched. So per-turn latency ≈ number of primitive ops. A
   production runner shouldn't *assume* batching; if it wants
   fan-out it may need to prompt for it, and it must still be correct
   when the model serializes.

3. **A stateful tool is only correct if the running conversation is
   replayed faithfully.** The map lookup on turn 8 only returned `0`
   because the `hashmap_put(2,0)` from turn 5 actually mutated the shared
   `World` AND the turn-5 assistant/tool messages were replayed verbatim
   (incl. `thought_signature`) so the model's world-model stayed in sync
   with the real object. The runner's message-history discipline
   (write-at-source, echo opaque tokens) is load-bearing for stateful
   tools, not just cosmetic.
