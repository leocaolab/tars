# `call_typed<T>` — design evidence and open decisions

Status: **design + prototype**. This doc records what a running spike proved
about three questions that fix the shape of a proposed
`LlmService::call_typed<T>(req, ctx) -> Result<T, ProviderError>`, then lays the
remaining choices in front of the owner with their costs. It does **not** decide
the tradeoffs.

The spike is a standalone crate (`scratchpad/typed_spike`, plus four
`scratchpad/q2_*` crates for the coherence tests). It depends on the real
`tars-provider::adapt_schema`, `tars-utils::decode`, and `schemars 0.8` by path,
and is fed the artifacts of a live `gemini-2.5-flash` run (`scratchpad/gemruns`,
including the 65,521-output-token runaway). Every claim below is either a
compiler result, a printed run, or a `file:line` quote. Claims that could not be
verified are labelled **UNVERIFIED**.

---

## The problem

The contract "a Rust type ↔ a provider's wire format" has four owners and no one
accountable end to end: arc derives a schema from `T`; tars's `adapt_schema`
rewrites it per dialect (one-way, records nothing); the provider may enforce or
silently discard it; arc decodes the reply back into `T` (stripping fences,
un-adapting shapes, salvaging truncation). The schema and the decode drift
because nothing binds them. `call_typed<T>` would give tars sole ownership of the
whole round trip; consumers would never touch a schema.

---

## What is now known

### Q1 — `adapt_schema`'s map→array is reversible; arc destroys the key at DECODE, not at request time

**This corrects the task's framing.** The hypothesis was: the map's key
information is destroyed at the *request* side (before the model sees it), so
"un-adapt" is impossible and `call_typed` must *reject* a dynamic-key-map `T`
under Gemini. The spike shows the opposite about *where* the loss happens.

`tars-provider/src/schema_adapt.rs:86-113` turns `{type:object,
additionalProperties:<Object>}` into `{type:array, items:<Object>}` for Gemini,
and **injects `issue_id: string` into each item** as the key carrier
(`schema_adapt.rs:101-109`). Running the real `adapt_schema` on a
`BTreeMap<String, FixEntry>` schema (spike `q1`):

```
BEFORE:  { "type":"object", "additionalProperties": { "$ref":"#/definitions/FixEntry" }, ... }
AFTER (inline_refs + adapt_schema Gemini):
         { "type":"array", "items": { "type":"object",
             "properties": { "issue_id":{"type":"string"}, "reply":{...}, "verdict":{...} },
             "required":["reply","verdict"] } }
```

Key facts the run establishes:

1. **A dynamic-key map has no key schema to destroy.** `additionalProperties`
   constrains the map's *values*; the keys are unconstrained strings. There is
   no key information in the request schema, so none is destroyed at the request
   side. The task's "keys destroyed before the model sees them" is not what
   happens.
2. **The reversal channel exists by construction.** `adapt_schema` injects
   `issue_id` precisely so the array can be re-keyed into a map. The spike shows
   a faithful reversal (read `issue_id` back into the key) recovering the
   model-chosen keys `["F-2","F-7"]`.
3. **arc throws that channel away at decode.** `critic_agent.rs:787-815`
   (`assign_display_labels`) does `m.remove("issue_id")` and invents positional
   keys `F-1, F-2, …`. The spike reproduces both: faithful reversal yields
   `["F-2","F-7"]`; arc's behaviour yields `["F-1","F-2"]` with the model key
   discarded. **The destruction is a decoder choice, not a request-side
   impossibility.**

So the honest constraint for `call_typed<T>` is *not* "reject dynamic-key maps
because they're irreversible." They **are** reversible via `issue_id`. The real
caveats are two, and they are softer:

- **Semantic reliability is UNVERIFIED.** The injected `issue_id` field carries
  no instruction telling the model "put the map key here"; it is an unlabelled
  string field in the schema. Whether Gemini populates it with the intended key
  is a prompt-engineering question the spike did not test against a live model.
  The *structural* channel is proven; its *reliability* is not.
- **The `$ref`-inline precondition is a live footgun.** `adapt_schema` is **not
  self-contained**: fed a raw `schema_for!` output whose `additionalProperties`
  is a `$ref`, it strips the `$ref` (`schema_adapt.rs:120`) and emits a **corrupt
  item that lost `verdict` and `reply` entirely** — proven in the spike's first
  Q1 block. This is the same precondition arc pays for by inlining refs twice
  (`critic/schema.rs:106`, and historically `verifier/judge.rs`). If tars owns
  `call_typed`, tars must inline before adapting, once, internally.

**The fixer live-bug claim: CONFIRMED, with the mechanism corrected.**
`arc_agents/src/fixer/parse.rs:75` decodes into `FixResponse(BTreeMap<String,
FixEntry>)`. The spike proves that if a Gemini schema were sent, the reply is a
JSON **array**, and `serde_json::from_value::<BTreeMap<..>>(<array>)` fails:

```
FixResponse(BTreeMap)::from_value(<array reply>) = Err — invalid type: sequence, expected a map
```

So yes — **it becomes a live decode bug the instant a schema is sent.** But it is
not luck that saves it today; it is *two* structural guards:
1. The fixer is a tool-using agent (`fixer/agent.rs:98`, `edit_toolset`), and
   `tars-runtime/src/worker.rs:261-273` **panics** if a tool-using worker is
   given any `output_schema` other than `None` — because "providers reject
   `response_format` together with `tools`" (`worker.rs:252`). A schema
   therefore *cannot* be attached to a tool-using call at all.
2. The fixer never sets an `output_schema` anyway (`parse.rs:125` decodes with
   `StructuredOutputMode::None`, fence-scrape).

The bug is real and latent; the codebase is defended against it by the
tools⊕schema mutual exclusion, not by chance.

### Q2 — the "hand-write it" escape hatch DOES exist, conditionally; this corrects the task's belief

The task states: *"I believe the escape hatch does not exist,"* because a blanket
`impl<T: JsonSchema> ResponseSchema for T` should forbid a manual `impl
ResponseSchema for MyType` via coherence (E0119). **The spike disproves that as
stated.** Four crates, `rustc 1.95.0`:

| crate | shape | result |
|---|---|---|
| `q2_blanket_only` | blanket impl alone, method `response_schema` | **compiles** |
| `q2_escape_hatch_ok` | blanket + manual impl for a **non-`JsonSchema`** local type | **compiles** |
| `q2_coherence_fail` | blanket + manual impl for a type that **also** derives `JsonSchema` | **E0119** |
| `q2_newtype_ok` | value-carrying `Schema` with derived + manual constructors, no blanket | **compiles** |

The manual escape hatch **is permitted** as long as the hand-written type does
not itself implement `schemars::JsonSchema` — rustc proves non-overlap because
the type is local, no `impl JsonSchema for LocalType` exists, and the blanket
only covers `JsonSchema` types. It fails **only** when a single type tries to be
*both* schemars-derived and hand-written. Verbatim, that failure is:

```
error[E0119]: conflicting implementations of trait `ResponseSchema` for type `MyType`
3 | impl<T: JsonSchema> ResponseSchema for T { ... }
  | ---------------------------------------- first implementation here
4 | #[derive(JsonSchema)] pub struct MyType { pub x: String }
5 | impl ResponseSchema for MyType { ... }
  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ conflicting implementation for `MyType`
```

Two further findings the spike surfaced:

- **The method must not be named `json_schema`.** That collides with
  `schemars::JsonSchema::json_schema`, and any unqualified `T::json_schema()`
  call is `error[E0034]: multiple applicable items in scope`. Name it
  `response_schema` (or call via `<T as ResponseSchema>::…`).
- **The blanket still welds tars's semver to schemars** for every derive-path
  type. `schemars 0.8::JsonSchema` and `1.0::JsonSchema` are different traits;
  a consumer who derives against the wrong version gets "the trait bound is not
  satisfied" on a type they visibly derived. `pub use schemars;` (consumers
  derive against tars's copy) mitigates but forces tars's version on everyone
  who takes the derive path. tars's own `Cargo.lock` already carries schemars
  `0.8.22`, `0.9.0`, and `1.2.1` simultaneously — the weld is not hypothetical.

So the corrected enumeration of Q2 options (all compile-checked except where
noted):

- **Blanket impl only** — compiles. Every derive-path consumer uses tars's
  schemars; a hand-written type is allowed *iff* it does not also derive
  `JsonSchema`. Simple, but a type can never be both derived and overridden, and
  the schemars version is tars's to dictate.
- **No blanket; every type hand-writes `response_schema()`** — compiles. No
  schemars in the public bound at all. Cost: **drift is back** — the schema is
  hand-maintained again, which is the disease `call_typed` exists to cure.
- **Value-carrying `Schema` (the `q2_newtype_ok` shape)** — compiles. `Schema`
  offers `derived::<T: JsonSchema>()` and `manual(Value)`; no blanket, no
  coherence conflict, both paths coexist per type. Cost: the public API takes a
  `Schema` **value**, so `call_typed` cannot infer the schema from the return
  type `T` — the caller passes `Schema::derived::<T>()` explicitly. The typed
  ergonomics ("just name the return type") are lost.
- **A tars-owned derive macro** — **UNVERIFIED** (not spiked). Would let tars
  emit `impl ResponseSchema` directly, decoupled from schemars's trait identity,
  but is a maintenance burden (a schema derive is not small).

### Q3 — arc's truncation salvage is generic except for ONE seam: the array path

`arc_agents/src/critic/decode.rs` carries ~120 lines of salvage. Line-by-line:

| code | arc-specific? | verdict |
|---|---|---|
| `find_findings_array_start` (`decode.rs:214-229`) | hardcodes `"findings"` + bare-array fallback | **the one seam** — inject as `array_path: Option<&'static str>` |
| `salvage_complete_findings` (`decode.rs:154-206`) | pure brace/string/escape balancing | **generic** |
| `is_truncation` (`decode.rs:136-145`) | classifies `TarsJsonError::{NoJsonObject, InvalidJson}` | **generic** (tars's own error taxonomy) |
| `issue_id` stripping + positional keying | lives in `critic_agent.rs:787` (`assign_display_labels`), **not** in the salvage | **stays in arc**, above the framer |
| `exhausted` envelope flag | `decode.rs:75-77` forces `exhausted:false` on salvage | **stays in arc** — the framer frames the array; assembling `{findings, exhausted}` into `T` is the generic decode step |

The generic framer needs **exactly one** DI seam — the array path — and the
precedent is exact: `tars_utils::JsonAgentResponse::wrapper_tags()` is already
this shape (a `&'static [&'static str]` the consumer overrides). A
`fn stream_array_path() -> Option<&'static str>` on the same trait is the natural
home.

The test this codebase applies to a downward move ("does the moved code need to
know who is calling it?") passes: a framer parameterised by `array_path` does
**not** know it is framing findings. A framer with `"findings"` baked in would —
that is the line not to cross.

**The prototype runs.** A generic `ArrayFramer` (spike `framer.rs`, the exact
balancing logic lifted from `salvage_complete_findings` and made resumable +
budgeted) is fed the three inputs **in 64-byte chunks, as `ChatEvent::Delta`
would arrive** (not a `String` read whole):

```
case1 well-formed (real 4A30, 3 findings, 2021 B):
  ↳ element #1/#2/#3 closed and emitted        (per-element emission on close)
  status: Closed → assembled T = CriticEnvelope { findings: 3, exhausted: true }

case2 REAL 65k runaway (4645, 253501 B, budget 8192):
  total elements emitted: 0
  status: AbortedByBudget at byte offset 8231 (budget 8192)
  partial prefix surfaced to caller: 8231 bytes, head = "{\n  \"exhausted\": true,\n  \"findings\": [\n    {\n      \"reply\": \"The `config_size` f"

case3 truncated between elements (derived from 4A30, 1342 B):
  ↳ element #1/#2 closed and emitted
  status: TruncatedAtEnd — salvaged 2 complete elements; incomplete tail dropped; truncation RETURNED
```

- **Per-element emission**: each array element is emitted the moment its brace
  balances, before the stream ends.
- **Runaway abort**: the 65k runaway never closes its first element (a 693-char
  sentence repeats 342× inside one unclosed `reply`; `stop_reason:max_tokens`,
  `output_tokens:65521`). The "no element closed within N bytes" rule fires at
  **byte offset 8231** (array start 39 + budget 8192). The caller gets the real
  8231-byte partial prefix — **never a sentinel**.
- **Clean truncation between elements**: the two complete elements are salvaged,
  the incomplete tail dropped, and the fact that it was truncated is **returned**
  in `FrameStatus::TruncatedAtEnd`, not swallowed — so the caller sets
  `exhausted:false` and paginates rather than reporting a false-clean review.

---

## What must still be decided (owner's call)

Each is a choice with its cost; the spike does not pick.

1. **`schemars` in the public API.** Pick from Q2's four (blanket-only /
   no-blanket / value-`Schema` / tars derive macro). Cost of blanket-only: every
   derive-path consumer inherits tars's schemars version, and no type can be both
   derived and hand-overridden. Cost of value-`Schema`: `call_typed` loses
   return-type inference. Cost of no-blanket: schema drift returns. The method
   must be named `response_schema`, not `json_schema`, regardless.

2. **Dynamic-key maps — reverse or forbid?** Q1 makes "impossible" false: the
   `issue_id` channel reverses them. So the choice is real, not moot: (a) support
   them by having `call_typed` inject/read `issue_id` faithfully (cost: an
   unlabelled `issue_id` field the model must be *prompted* to fill as the key —
   reliability UNVERIFIED against a live model), or (b) forbid a `T` whose schema
   has `additionalProperties` under Gemini and force consumers to a `Vec<Item>`
   with an explicit id field (cost: consumers restructure their types; the fixer
   is the concrete case). Whichever is chosen, `adapt_schema` must inline `$ref`
   internally first, or it silently corrupts the item (Q1).

3. **Where `call_typed` lives.** `LlmService` is in `tars-pipeline`;
   `adapt_schema` in `tars-provider`; `decode` in `tars-utils`. `call_typed`
   needs all three, so one crate grows a dependency. `tars-pipeline` already
   depends on both `tars-provider` (it holds an `Arc<dyn LlmProvider>`) and can
   reach `tars-utils`, so hosting it on `LlmService` in `tars-pipeline` adds no
   new *crate* edge — but it puts schema derivation + framing into the pipeline
   layer. Alternative: a thin `call_typed` free function in `tars-utils` that
   takes an already-built `LlmService`. Cost is where the dependency and the
   surface area land; both compile-plausible, neither spiked.

4. **`WorkerPersona.output_schema` (the v1.7.0 `OutputSchema` three-state enum,
   `worker.rs:84-98`).** Today a persona carries `None | WorkerResult |
   Custom(name, Value)` — a hand-supplied schema `Value`. Once the schema is a
   function of `T`, `Custom(name, Value)` overlaps with "derive it from `T`". Does
   `OutputSchema` grow a `Typed<T>` variant, does `call_typed` bypass the persona
   schema entirely, or does `Custom` become the output of `T::response_schema()`?
   Not spiked; it is a live API-surface question the moment `call_typed` ships.

5. **Tools.** Providers reject `response_format` alongside `tools`
   (`worker.rs:252`), enforced by the panic at `worker.rs:261-273`. So
   `call_typed` serves **tool-free** calls only. Is that acceptable? Tool-using
   agents (the fixer, the merger) keep the untyped `call()` + fence-scrape decode
   they use today. If it is not acceptable, the only path is tool-use *emulation*
   (a synthetic "emit_result" tool whose args are the schema) — larger, unspiked.

6. **The output budget.** arc's critic sets neither `max_output_tokens` nor a
   byte cap, which is exactly why a 65,521-token runaway was possible. The
   framer's byte budget (spike: 8192, abort at offset 8231) is the mechanical
   backstop, but the *policy* — byte cap vs `max_output_tokens` vs both, and the
   number — is the owner's. The budget must live low enough to abort a runaway
   that a schema constraint failed to prevent (the runaway happened *with* the
   findings-array schema in force).

7. **Migration + second consumer.** What moves out of arc: the salvage
   (`decode.rs:136-229`), the `$ref` inliner (`critic/schema.rs`), the
   `adapt_schema` coupling. What stays: `assign_display_labels`, `exhausted`
   semantics, `is_issue` filtering — arc-domain, above the framer. concer (a
   second consumer) inherits the framer + typed decode for free the moment its
   own `T` names a `stream_array_path`; it writes no salvage code. **UNVERIFIED**
   which concer types would use it (concer not inspected here).

---

## What `call_typed` does NOT fix

Stated plainly so it is not oversold:

- **arc swallowing a decode `Err` into an empty result.** `critic/agent.rs`
  around line 1221 does `free_review_async(...).await.unwrap_or_else(|e| { warn;
  FreeReviewOutput { findings: empty_map, .. } })` — a failed pass becomes zero
  findings (a false-clean contribution) with only a `tracing::warn`. `call_typed`
  returning a typed `Err` does nothing if the caller still `unwrap_or_else`es it
  into emptiness. That is an arc call-site fix, orthogonal to this API.
- **arc's token accounting sums only successful units.** `record_usage_from` runs
  after the salvage path; a unit that failed decode contributes no usage, so the
  runaway's 65,521 output tokens can go unaccounted. `call_typed` frames and
  budgets the stream but does not change how arc totals usage.

---

## Prototype pointers (read-only evidence, in scratchpad)

- `typed_spike/src/framer.rs` — the generic `ArrayFramer` (one seam: `array_path`).
- `typed_spike/src/main.rs` — Q1 (real `adapt_schema`) + Q3 (framer on 3 real inputs).
- `q2_blanket_only`, `q2_escape_hatch_ok`, `q2_coherence_fail`, `q2_newtype_ok`
  — the coherence matrix (compile / compile / E0119 / compile).
- `gemruns/` — the live `gemini-2.5-flash` artifacts, including the 65k runaway
  (`bodies/4645….bin`, `text` field = 253 KB, 342× repeat, `max_tokens`).
