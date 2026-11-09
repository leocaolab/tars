# loop_react — loop-and-react agent POC (incubator)

A minimal, **real** autonomous loop-and-react agent built on the TARS
pipeline. Not a scripted demo — it calls a live provider from
`~/.tars/config.toml` and drives a genuine tool loop to completion.

## What it proves

The full react cycle, end to end:

```
offer tools → LLM decides a tool_use → dispatch to a native Rust
handler → feed the result back → loop → until the LLM emits no
tool_use (done) or MAX_TURNS.
```

Concretely:

- **Real pipeline call.** Config → `ProviderRegistry::from_config_default`
  → `LlmService::of(provider, model)` wrapped in `Telemetry + Retry`
  middleware → `pipeline.call(req, ctx)` per turn. No `ScriptedProvider`
  (contrast `multi_step_with_tools`, which cans the stream).
- **Native tool-calling.** `ToolSpec`s (name + JSON schema) go on
  `ChatRequest.tools`; the model's decision comes back as
  `ChatEvent::ToolCall{Start,End}` off the stream. No JSON-in-text
  convention.
- **A tiny `ToolRegistry` of native Rust fns** (the "app as world"
  fn-access-map): `read_file(path) -> String` and
  `write_file(path, contents) -> String`, each jailed to a scratch dir.
- **A toy task that forces the loop**: "read input.txt, count its lines,
  write the count to out.txt" — requires ≥1 tool call and ≥1 read of a
  tool result. The POC then verifies `out.txt` really holds the right
  number.

## How to run

Needs an API key for the chosen provider in the process env. Source the
repo `.env` (which holds `GEMINI_API_KEY`) first:

```bash
set -a; source .env; set +a
cargo run -p tars-examples --example loop_react

# pick a different native tool-calling provider from your config:
TARS_POC_PROVIDER=openai cargo run -p tars-examples --example loop_react
```

Config is resolved like every tars consumer: `$TARS_CONFIG` →
`~/.tars/config.toml` → XDG default.

## Verbatim run (gemini_flash, gemini-2.5-flash)

```
scratch dir: /var/folders/.../.tmpnVajel
seeded input.txt with 5 lines

loading config from ~/.tars/config.toml
provider: gemini_flash   model: gemini-2.5-flash

── Turn 1 ───────────────────────────────────────────
  LLM: stop=ToolUse  model=gemini-2.5-flash  tokens in/out=250/17
  tool_use: read_file({"path":"input.txt"})
    → dispatched read_file → apple\nbanana\ncherry\ndate\nelderberry\n

── Turn 2 ───────────────────────────────────────────
  LLM: stop=ToolUse  model=gemini-2.5-flash  tokens in/out=297/22
  tool_use: write_file({"contents":"5","path":"out.txt"})
    → dispatched write_file → wrote 1 bytes to out.txt

── Turn 3 ───────────────────────────────────────────
  LLM: stop=EndTurn  model=gemini-2.5-flash  tokens in/out=341/18
  text: I have counted the lines in input.txt and written the count to out.txt.
  (no tool_use — agent is done)

══ Result ════════════════════════════════════════════════
total tokens in/out: 888/57
out.txt contents: "5"
✔ PASS — agent wrote the correct line count (5) via the tool loop
```

## What this revealed for the production loop-react runner

1. **The provider gives you tool calls in two events, and the name is
   only on the first one.** `ToolCallStart` carries `{index, id, name}`;
   `ToolCallEnd` carries `{index, id, parsed_args, thought_signature}`
   but **not the name**. The runner must correlate by `index` (this POC
   keeps an `index → (id, name)` map in `drain`). A real runner needs a
   robust accumulator that also survives interleaved parallel calls and
   `ToolCallArgsDelta` streaming.

2. **`thought_signature` is load-bearing and must be echoed at source.**
   Gemini's thinking models return an opaque `thought_signature` on each
   call and **reject the next turn if it's missing**. The runner has to
   capture it at `ToolCallEnd` and thread it back verbatim onto the
   `Message::Assistant { tool_calls }` it replays — i.e. write-it-at-
   source into the conversation history, never reconstruct it. A generic
   runner that drops provider-opaque tokens will break multi-turn Gemini.

3. **The loop, budget, and conversation state are all the runner's job —
   the pipeline owns none of it.** `LlmService` is single-shot; there is
   no agent tool-loop on the single-provider path (`run.rs` even notes
   "no agent tool-loop"). This POC hand-manages: append
   `Assistant{tool_calls}` + one `Message::Tool{tool_call_id, is_error}`
   per result, re-clone the whole history each turn, and own `MAX_TURNS`.
   The production runner wants (a) a real `Conversation`/session type
   instead of re-cloning `Vec<Message>`, (b) an explicit turn budget +
   `Caller` **deadline** (the POC's context has no
   deadline/IAM), and (c) a typed termination reason (done vs.
   budget-exhausted vs. tool-error-loop) rather than the current
   "empty tool_calls ⇒ done" heuristic.
```
