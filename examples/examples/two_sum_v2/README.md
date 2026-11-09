# two_sum_v2 — no algorithm given (reasoning vs execution)

Identical harness, tools, and `World` to [`two_sum`](../two_sum). The
**only** change is the task prompt: it states the *problem*, the
input-access rule (`nums` is not in the prompt — read it via
`array_len` / `array_get`), and the *available data structures* (a live
integer→integer HashMap, `println`, `submit_answer`) — but gives **no
algorithm**. `two_sum` spells out the one-pass trick ("check
`hashmap_get(target - nums[i])`, else `hashmap_put(nums[i], i)`"); this
one says only *"solve it however you see fit."*

So this tests whether the model **derives** an approach (reasoning) or
merely **executes** a recipe it was handed (`two_sum`).

## How to run

```bash
set -a; source .env; set +a      # provides GEMINI_API_KEY
cargo run -p tars-examples --example two_sum_v2
```

## Result: it invented the optimal one-pass HashMap

With no hint, gemini-2.5-flash chose the **canonical value→index
one-pass hashmap** strategy — the same optimal approach `two_sum` had to
spell out. It did **not** fall back to a naive O(n²) pairwise
`array_get` scan.

Verbatim trace (gemini_flash):

```
harness world: nums=<hidden, 4 elems, tool-only>  target=9
provider: gemini_flash   model: gemini-2.5-flash

── Turn 1 ──  println({"text":"Starting Two Sum problem solving."})
── Turn 2 ──  array_len({})                    → 4
── Turn 3 ──  println({"text":"Array length is 4. Iterating through the array."})
── Turn 4 ──  array_get({"index":0})           → 2
── Turn 5 ──  hashmap_get({"key":7})           → null      # complement of 2 (=9-2=7)
── Turn 6 ──  hashmap_put({"key":2,"value":0}) → ok: map[2] = 0   # store value→index
── Turn 7 ──  array_get({"index":1})           → 7
── Turn 8 ──  stop=EndTurn tokens out=0 tool_calls=0   (empty stall — nudged)
── Turn 9 ──  hashmap_get({"key":2})           → 0        # complement of 7 (=9-7=2) → HIT
── Turn 10 ─  stop=EndTurn tokens out=0 tool_calls=0   (empty stall — nudged)
── Turn 11 ─  submit_answer({"i":0,"j":1})     → recorded answer: indices [0, 1]
── Turn 12 ─  stop=EndTurn tool_calls=0        (answer submitted — done)

final HashMap state: {2: 0}
submitted answer: indices [0, 1]
✔ PASS — nums[0] + nums[1] = 2 + 7 = 9 == target 9
```

**Strategy chosen:** (a) invented one-pass HashMap (store value→index,
look up complement). Correct answer `[0, 1]`.

## Efficiency: v1 (hinted) vs v2 (no hint)

| | core algorithmic tool calls | total tool calls | turns | empty-stall nudges |
|---|---|---|---|---|
| `two_sum` (algorithm given) | 7 | 8 | 10 | 1 |
| `two_sum_v2` (no algorithm) | 7 | 9 | 12 | 2 |

The **algorithmic footprint is identical** — 7 core ops
(`array_len`, `array_get`×2, `hashmap_get`×2, `hashmap_put`×1,
`submit_answer`). Deriving the approach cost only a little extra
narration (`println`) and one more empty-turn stall; it did **not** cost
a worse algorithm. Removing the hint did not degrade the solution here.

## One sentence for the production runner

On a small, well-known problem this model reasons its way to the optimal
strategy without being told it, so the runner can lean on the model to
*choose* the approach rather than hard-coding it — but the two empty
mid-solve stalls (vs one in v1) reinforce that completion must be gated
on the terminal act plus a nudge, never on "no tool_use."
