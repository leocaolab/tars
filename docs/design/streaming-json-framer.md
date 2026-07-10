# Streaming JSON array framer — design (parked)

**Status:** **PARKED. Priority: LOW.** This document exists so the idea is not
lost, not so anyone builds it next week. Written from a working spike
(`call-typed.md` Q3) and three real Gemini artifacts, so a future implementer
does not re-derive it — but it should stay parked, and the reason is the whole
first section.

The framer is a **salvage mechanism for a failure that should not be salvaged.**
The 65k-token runaway that motivates it
(`arc/docs/retro/2026-07-10-a-failed-pass-reports-clean.md`) was not caused by
the absence of a framer. It was caused by three arc-side silences, and the
framer fixes **none** of them. Two one-line arc fixes are strictly higher value
and strictly cheaper (see "If we ever build this, do these first"). Build those;
leave this parked.

---

## 1. Why the framer is not the fix

The runaway: `gemini-2.5-flash` looped inside `reply` — a free-text finding
field no schema constrains — repeating one 693-char sentence 342 times, hit the
provider's `max_tokens` (65,521 output tokens), and returned a 253 KB JSON
prefix whose first finding object never closed. That is a weak model running
away in free prose. Fine. Models do that. **The defect is what arc did with the
truncated reply**, and it is three stacked silences (`arc/todo.md` §16):

- **§16a — the swallow.** `arc/crates/arc_agents/src/critic/agent.rs:1221`
  unwraps a failed `free_review_async` with `.unwrap_or_else(|e| { warn; ... })`
  that fabricates `FreeReviewOutput { findings: serde_json::Map::new(), ... }`
  (`agent.rs:1227-1230`). A *failed* review pass becomes *zero findings* —
  indistinguishable from "this pass looked and found nothing." The decode layer
  is honest: `critic/decode.rs:121-123` deliberately returns the truncation
  `Err` (`_ => Err(kind)`), with the comment "keep the honest truncation failure
  so the caller surfaces it." The caller does not surface it; `tracing::warn!`
  never reaches the CLI. The operator saw `1 open`.
- **§16b — no output bound.** The critic sets no `max_output_tokens`; the
  generated schema gives `reply` `minLength: 1` and no `maxLength`. The only
  ceiling on a free-prose runaway is the provider's own 65,536.
- **§16c — the accounting.** `arc/crates/.../review/mod.rs:608` sums tokens over
  successful units only, so a crashed unit's 65,517 output tokens vanish from the
  report (printed `Tokens: in=0 out=0`).

A framer that recovers N−1 findings from a truncated stream makes truncation
look like a survivable, ordinary outcome. **It is not.** Truncation means the
model ran away; the operator needs to know that, not receive a partial answer
that looks whole. The honest fix for §16a is to propagate the `Err` — one `?` —
not to rescue more partial JSON out of a truncated reply.

### What the framer does NOT fix

- **The swallow (§16a).** Propagating the `Err` fixes it. Independent of this
  doc, one line, at `agent.rs:1221`. A framer that hands the caller a
  better-salvaged partial answer does nothing if the caller still
  `unwrap_or_else`es it into an empty map.
- **The missing output bound (§16b).** `ChatRequest.max_output_tokens: Option<u32>`
  **already exists** (`tars-types/src/chat.rs:46`) and defaults to `None`
  (`chat.rs:84`) — it is simply never set by the critic. Setting it is the fix,
  and it is independent of the framer. (The request-side pre-flight that compares
  it against provider capability already exists too — `chat.rs:191-195`.)
- **The token accounting (§16c).** An aggregate over the success path only. The
  framer frames and byte-budgets a stream; it does not change how arc totals
  usage.

### What the framer *does* buy (narrow, but real)

1. **A cheap early abort.** Stop *reading* at a byte budget instead of holding
   the stream open until the provider burns its full `max_tokens`. In the spike,
   the runaway aborts at byte offset 8231 rather than draining 253 KB.
2. **Per-element streaming.** Each array element is emitted the moment its braces
   balance, before the stream ends — so a consumer can act on finding #1 while
   #2 is still arriving.
3. **An honest, typed status.** `TruncatedAtEnd` / `AbortedByBudget { offset }`,
   each carrying the **real raw prefix** the model produced — never a
   `parse_failed`/`invalid`/`unknown` sentinel.

**Caveat on (1) — it is a client-side READ abort only, and its money-saving is
UNVERIFIED.** Closing the read stops *arc* waiting and stops downstream
processing of 253 KB. Whether it saves *tokens* depends on two provider-specific
facts I did not verify against any provider's billing docs: (a) whether
disconnecting an in-flight SSE/streaming response actually cancels server-side
generation, and (b) whether the provider bills for tokens already generated at
disconnect. Both are commonly *against* the saving — generation frequently
continues server-side after a client disconnect, and providers bill output
tokens produced. **So state it plainly: the read abort reliably saves wall-clock
latency and downstream work; it is NOT established that it saves money.** The
real money-saver for a runaway is §16b's `max_output_tokens`, which caps
generation server-side.

---

## 2. The design (from the spike, so it need not be re-derived)

### 2.1 Prototype shape and results

The spike (`call-typed.md` §Q3, prototype in `scratchpad/typed_spike/src/framer.rs`)
lifted the exact brace/string/escape balancing from
`critic/decode.rs::salvage_complete_findings` (`decode.rs:154-206`) and made it
**resumable + budgeted**, then fed it three real inputs **in 64-byte chunks, as
`ChatEvent::Delta { text }` would arrive** (`tars-types/src/events.rs:22`) — not
a `String` read whole:

| case | input | result |
|---|---|---|
| well-formed | real 4A30, 2021 B, 3 findings | element #1/#2/#3 closed and emitted on close; `status: Closed`; assembled `CriticEnvelope { findings: 3, exhausted: true }` |
| **the real 65k runaway** | 4645, 253,501 B, `stop_reason: max_tokens`, `output_tokens: 65521` — one unclosed `reply` with a 693-char sentence repeated 342× | **0 elements emitted** (the first element never closes); `status: AbortedByBudget at byte offset 8231` (array start 39 + budget 8192); caller gets the **real 8231-byte prefix**, head = `{\n  "exhausted": true,\n  "findings": [\n    {\n      "reply": "The `config_size` f` |
| truncated between elements | derived from 4A30, 1342 B | element #1/#2 closed and emitted; incomplete tail dropped; `status: TruncatedAtEnd` — truncation **returned**, not swallowed, so the caller sets `exhausted:false` and paginates rather than reporting a false-clean review |

The runaway case is the load-bearing one: the "no element closed within N bytes"
rule fires at offset 8231, the caller receives the true prefix, and there is
**no sentinel** anywhere.

### 2.2 The one seam

The salvage code is already generic except for a single hardcoded string:

| existing code | generic? | disposition |
|---|---|---|
| `find_findings_array_start` (`decode.rs:214-229`) | hardcodes `"findings"` + a bare-top-level-array fallback | **the one seam** — inject as `array_path: Option<&'static str>` |
| `salvage_complete_findings` (`decode.rs:154-206`) | pure brace/string/escape balancing | generic as-is |
| `is_truncation` (`decode.rs:136-145`) | classifies `TarsJsonError::{NoJsonObject, InvalidJson}` — tars-utils' own error taxonomy | generic as-is |

The framer needs **exactly one** dependency-injection seam: the array path. The
precedent for that shape already exists in the same crate:
`JsonAgentResponse::wrapper_tags() -> &'static [&'static str]`
(`tars-utils/src/json_decode.rs:187`, on the trait at `json_decode.rs:181-190`) —
a `&'static` the consumer overrides, "the tag strings are the consumer's
convention; the extraction mechanism in `decode` is generic"
(`json_decode.rs:175-176`). A `fn stream_array_path() -> Option<&'static str>` on
that same trait is the natural home. A framer parameterised by `array_path` does
not know it is framing *findings*; a framer with `"findings"` baked in would —
that is the line not to cross.

### 2.3 What stays in arc, above the framer

The framer frames the *array*. Everything findings-specific stays in arc:

- **`issue_id` stripping + positional keying** — `critic/critic_agent.rs:787`
  (`assign_display_labels`), which removes `issue_id` and assigns positional
  `F-1, F-2, …` keys. Domain policy, above the framer.
- **`exhausted` semantics** — `decode.rs:75-77`
  (`CriticWire::from_salvaged_findings` forces `exhausted:false`, because a
  truncated reply never got to say it was done). Assembling `{findings, exhausted}`
  into the envelope `T` is the generic decode step; *the meaning* of `exhausted`
  is arc's.

### 2.4 Zero schemars — independent of every open `call-typed.md` decision

The framer is a byte-level state machine over incoming text deltas. It needs
**no schema at all** — not schemars, not `adapt_schema`, not the
`JsonSchema` blanket/coherence question. Verified:

- `grep -rln schemars crates/*/Cargo.toml` returns nothing — **no tars crate
  depends on schemars.**
- The three versions in `Cargo.lock` (`0.8.22`, `0.9.0`, `1.2.1`) are all
  **transitive**: `0.8.22` via `tauri-build` / `tauri-utils`, and `0.9.0` +
  `1.2.1` via `serde_with` (`Cargo.lock` lines 6455/6579 and 5575-5576).

So the framer is decoupled from every unresolved decision in `call-typed.md`
(the schemars-in-public-API choice, dynamic-key-map reversal, tools⊕schema, the
`OutputSchema` variant). Those gate `call_typed<T>`. They do **not** gate the
framer. If the framer is ever built, it can ship without any of them.

### 2.5 Where it would live, and why

**`tars-utils`**, next to `decode` and the `JsonAgentResponse` trait. The
layering argument:

- `tars-utils` is "Pure, dependency-free helpers over tars-types (stateless
  algorithms — no I/O, no state)" (`tars-utils/Cargo.toml:7`). A resumable
  brace-balancing framer over incoming text deltas is exactly that — pure,
  stateless-per-construction, no I/O.
- Its three ingredients already live there or below: the balancing logic is
  lifted from `decode.rs`; `is_truncation` classifies `TarsJsonError`, tars-utils'
  own taxonomy; the DI seam extends `JsonAgentResponse` in
  `json_decode.rs`; and the input type `ChatEvent::Delta { text }` is in
  `tars-types` (`events.rs:22`), which `tars-utils` already depends on
  (`tars-utils/Cargo.toml:13`). No new crate edge.

A second consumer (concer) inherits the framer for free the moment its own `T`
names a `stream_array_path` — it writes no salvage code. (Which concer types
would use it is UNVERIFIED — concer not inspected here.)

---

## 3. If we ever build this, do these first

Both are strictly higher value and strictly cheaper than the framer, and both
are independent of it:

1. **The `?` (§16a).** Propagate the decode `Err` at
   `arc/.../critic/agent.rs:1221` instead of `unwrap_or_else`ing it into an empty
   findings map. A failed pass must not read as a clean pass. One line. This is
   the actual fix for the runaway-reported-clean incident.
2. **`max_output_tokens` (§16b).** Set `ChatRequest.max_output_tokens`
   (`tars-types/src/chat.rs:46`, already present, currently `None`) on the
   critic's calls, to a cap justified against observed legitimate replies (the
   corpus in `arc/todo.md` §16b: outputs of 384–6757 tokens). This is the real
   money-saver — it caps generation server-side, which the framer's client-side
   read abort does not reliably do (§1).

Only after both of those land, and only if per-element streaming or an
early-abort on *bounded* replies proves to carry its own weight, revisit this.

---

## Evidence pointers

- Prototype + runs: `call-typed.md` §Q3; `scratchpad/typed_spike/{framer.rs,main.rs}`;
  `scratchpad/gemruns/` (the live artifacts, incl. `bodies/4645….bin`).
- Retro: `arc/docs/retro/2026-07-10-a-failed-pass-reports-clean.md`.
- The three silences: `arc/todo.md` §16 (16a/16b/16c).
- Salvage source: `arc/crates/arc_agents/src/critic/decode.rs`.
