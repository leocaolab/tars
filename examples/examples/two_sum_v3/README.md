# two_sum_v3 — factual tool descriptions, zero hints (the real reasoning test)

Third in the series. Identical harness, tools, `World`, and input
(`nums=[2,7,11,15]`, `target=9`) to [`two_sum`](../two_sum) and
[`two_sum_v2`](../two_sum_v2). The one axis that changes across the three
is **how much the approach is telegraphed** — and v3 telegraphs nothing:

| | task prompt | tool descriptions |
|---|---|---|
| `two_sum`    | spells out the one-pass hashmap trick | guiding ("Use this to remember … store value→index") |
| `two_sum_v2` | no algorithm, but names the HashMap | **guiding** (still say "store value→index") |
| `two_sum_v3` | no algorithm, does **not even mention** the HashMap | **factual only** (what it is + its type, not how to use it) |

So in v3, nothing in the task *or* any tool description hints at using
the map for a complement lookup. The model learns a hash map merely
*exists* from a one-line factual description (`"Insert an integer
key→value pair into a hash map. Overwrites…"`). For it to use the map for
Two Sum, it must map *"a HashMap exists" + "this problem" → "use it"*
from its own knowledge.

**Honesty caveat, stated up front:** even when the model "reaches for the
hashmap approach," that is **recall of the canonical Two Sum solution**
(ubiquitous in training data), **not novel derivation**. The narrow
question v3 answers is: does purely *factual* capability advertising
suffice to make the model actually *use* an instrumented capability?

Run:

```bash
set -a; source .env; set +a
cargo run -p tars-examples --example two_sum_v3
POC_DUMP=1 cargo run -p tars-examples --example two_sum_v3   # dumps raw context, 2 turns
```

## Result: the model mostly IGNORED the HashMap and solved it in its head

Strategy is **(c) other**, and it is **unstable across runs**. Three real
runs against `gemini_flash`:

| run | HashMap used? | final map state | parallelism | answer |
|---|---|---|---|---|
| A | **no** | `{}` | 1 | `[0,1]` ✓ |
| B | yes, but messy | `{1:0, 2:0, 7:1, 3:0, 11:2, 15:3, 0:0}` (junk keys) | **4** | `[0,1]` ✓ |
| C | **no** | `{}` | 1 | `[0,1]` ✓ |

Dominant behavior (2 of 3): **read the whole array once** (`array_len`,
then `array_get(0..3)`), then in a single turn **recall and simulate the
hashmap algorithm entirely in reasoning tokens** — and submit the answer
directly. The advertised `hashmap_put`/`hashmap_get` tools are **never
called**; the "hash map" in the model's turn-7 monologue is imaginary.

Verbatim in-head run (POC_DUMP; the context dump confirms nothing hints
the approach — every tool description is factual and the task never says
"HashMap"):

```
┌─ POC_DUMP: context for turn 1 ─
│ TOOLS advertised to the model (name — description):
│   • array_get — Return nums[index].
│   • array_len — Return the number of elements in nums.
│   • submit_answer — Report the final answer: two indices i and j.
│   • hashmap_get — Look up a key in the hash map. Returns the stored integer value, or null if the key is absent.
│   • hashmap_put — Insert an integer key→value pair into a hash map. Overwrites the value if the key already exists.
│   • println — Print a line of text to the log.
│ MESSAGES (1 in context):
│   [0] user: Solve Two Sum: there is an integer array `nums` … Report the pair via submit_answer.
└─

── Turn 1 ──  array_len({})           → 4
── Turn 2 ──  array_get({"index":0})  → 2
── Turn 3 ──  array_get({"index":1})  → 7
── Turn 4 ──  array_get({"index":2})  → 11
── Turn 5 ──  array_get({"index":3})  → 15
── Turn 6 ──  println({"text":"Target is 9"})   → printed
── Turn 7 ──  (out=314 tokens) reasoning:
      "... for each number I will calculate the complement ... check if the
       complement exists in a hash map ... Add (2,0) to the hash map ... i=1,
       nums[1]=7, complement=2, it is in the hash map at index 0, 0 != 1 →
       solution i=0, j=1."
              submit_answer({"i":0,"j":1})   → recorded answer: indices [0, 1]
── Turn 8 ──  "The answer has been submitted."   (done)

final HashMap state: {}          ← the real HashMap was NEVER touched
submitted answer: indices [0, 1]
✔ PASS — nums[0] + nums[1] = 2 + 7 = 9 == target 9
```

## Tool-call count across the series (efficiency)

| | HashMap tool calls | array reads | total tool calls | how it "computed" |
|---|---|---|---|---|
| `two_sum` (hinted) | 3 (`put`×1,`get`×2) | 2 (streaming) | 8 | via the real HashMap |
| `two_sum_v2` (task-stripped, tools hinted) | 3 | 2 | 9 | via the real HashMap |
| `two_sum_v3` run A/C (all stripped) | **0** | 4 (full read) | ~7 | **in reasoning tokens** |

Stripping the hint didn't make the model use the map *more cleverly* — it
made it *stop using the map* and read the whole array instead, then do the
arithmetic in its head. (It scales fine at n=4; at n=10⁶ that in-context
strategy would blow the context window while the untouched HashMap sat
right there.)

## One line for the design

**Factual capability advertising is NOT enough to make the agent use a
capability** — when the task is small enough to fit in the context, the
model will happily solve it in reasoning tokens and leave your
instrumented tool untouched (and when it does use it, unguided, it uses it
sloppily). If you expose a capability because you *need* it exercised —
for auditability, for state that must outlive the context, for scale
beyond what fits in a prompt — the tool description or task framing must
*guide* its use; you cannot rely on the model to reach for it on its own.
