# Observability scaling — right-sized now, trait-seam later

> Design note (design-only). How tars's observability compares to the modern
> fleet-scale LLM-SRE stack (Drain + ClickHouse + Qdrant + trace-tree + a
> Redpanda/Benthos ingestion tier), and the deliberate decision **not to build
> that now** — because the seams that let it grow into that stack are already
> traits. When a capability is genuinely needed, **add/adopt a trait and drop in
> an implementation**; do not stand up fleet infrastructure for a personal-mode
> tool. See [`observability.md`](./observability.md), [`eval-methodology.md`](./eval-methodology.md).

## 0. Thesis

tars's observability is **already on the right side of the two design axes** that
matter — it just makes a different **scale** choice than a fleet SRE stack, and
that choice is correct for its positioning (personal-mode, single-node; M5
shipped, M6 personal-only). The path to fleet scale is **a new impl behind an
existing trait**, not a rebuild. So: **don't build ClickHouse / Qdrant / a
Redpanda+Benthos ingestion tier now** — keep the seams clean and swap the impl
when scale actually demands it.

## 1. The two axes (where tars already sits right)

**Axis A — structure at the SOURCE, not reconstructed downstream.**
The fleet stack runs **Drain** (a fixed-depth template parser) to *recover*
structure from unstructured text logs — because those logs come from systems it
doesn't own. tars does not emit unstructured text for its own signals: it emits
**typed `PipelineEvent`s** via `EventEmitter` → `PipelineEventStore`
(`tars-melt/src/metrics.rs`: *"the events are the source of truth"*). Structured
at the source strictly beats parsing downstream — the same principle as typed
error propagation. **Consequence: do NOT adopt Drain for tars's own telemetry.**
Drain's only justified home in tars is the one text source we don't own — the
**subprocess CLI stderr** boundary (claude_cli / codex), lifted **once** into a
typed error there, never a downstream parser.

**Axis B — control-flow track vs data-semantic track.**
The fleet stack splits system logs (control flow) from prompt/response payloads
(data semantics). tars **already has this split**: `PipelineEvent` (the control
/ telemetry track) + `BodyStore` bodies (the ChatRequest/ChatResponse payloads,
referenced from events via `ContentRef`, content-addressed & deduped). Two
tracks, one anchor.

**The anchor.** A global `TraceId` (`tars-types/src/context.rs:20`) threads
through the pipeline context — the same "trace as universal correlation anchor"
the fleet stack builds its whole query model around.

## 2. What tars already has vs the fleet stack

| Fleet-SRE element | tars today | Verdict |
|---|---|---|
| Structured logs (Drain → template_id) | typed `PipelineEvent` emitted at source (`pipeline_event_store.rs:53`) | tars is *past* Drain for own signals |
| Control-flow / data-semantic split | `PipelineEvent` + `BodyStore` bodies via `ContentRef` (`body_store.rs:30`) | ✅ present |
| Global Trace ID anchor | `TraceId` in context (`context.rs:20`) | ✅ present |
| Events = source of truth, sink swappable | OTLP export bridge (`tars-melt`), events canonical | ✅ present |
| Cohort tags + analysis | cohort tags + `tars.eval` (`read_calls`/`write_score`/`EvaluationScored`) | ✅ present |
| Columnar store (ClickHouse) | **SQLite** (`SqlitePipelineEventStore`, `SqliteBodyStore`) | different **scale**, correct now |
| Ingestion tier (Redpanda/Benthos/OTel Collector) | **in-process direct write** | correct for single-node |
| Vector / semantic-outlier mining (Qdrant) | **absent** | the one real gap |

## 3. The scaling principle — seam is a trait, scale is a drop-in impl

**The storage sinks are ALREADY traits.** SQLite is *an* impl, not the design:

- `pub trait PipelineEventStore` — `pipeline_event_store.rs:53` (impl `SqlitePipelineEventStore` `:364`)
- `pub trait BodyStore` — `body_store.rs:30` (impl `SqliteBodyStore` `:168`)
- `pub trait EventStore` — `sqlite.rs` (impl `SqliteEventStore` `:130`)

So the fleet-scale migration is **not a rewrite** — it is: write
`struct ClickHouseEventStore; impl PipelineEventStore for ClickHouseEventStore`
and inject it at the composition root. **The emit side (EventEmitter, every
`PipelineEvent` producer) does not change one line.** That is exactly "right-sized
now, swap the impl when needed."

| Capability | Seam status | The drop-in impl (when needed) | Trigger |
|---|---|---|---|
| Fleet-scale event/body store | ✅ trait exists (`PipelineEventStore`/`BodyStore`/`EventStore`) | `ClickHouseEventStore` / object-store body sink | M6 multi-tenant / event volume outgrows SQLite |
| Trace export to a tracing backend | ✅ OTLP bridge exists (`tars-melt`) | point OTLP at an OTel Collector → ClickHouse/Tempo | same |
| High-throughput ingestion tier | n/a (in-process) | front the OTLP export with Redpanda+Benthos | sustained EPS the in-process path can't absorb |
| **Semantic-outlier / silent-failure mining** | ❌ **no trait yet** | see §4 | the "hallucination with no error" need |

## 4. The one missing seam — the semantic-outlier track

The genuine capability gap (not a scale swap): **silent failures** — the model
emits *valid-looking but logically wrong* output, no `Error`, the run is green,
the result is a disaster. tars catches typed errors + metric evals, but has no
detector for this class. This is where the fleet stack's Vector-Embedding +
edge-clustering earns its keep.

**Per the principle, don't build a Qdrant pipeline now — define the seam first.**
When the need is real, ADD a trait over the event store — a subscriber/analysis
seam, e.g.:

```rust
/// Consumes finished calls (events + bodies) for post-hoc analysis.
/// The default impl is None/no-op; an embedding-outlier impl is dropped in later.
pub trait CallAnalyzer: Send + Sync {
    fn on_call(&self, ev: &PipelineEvent, bodies: &dyn BodyStore);
}
```

…then the first impl is a `SemanticOutlierAnalyzer` that async-embeds sampled
prompt/response pairs (tagged with `TraceId`), stores vectors (a `VectorSink`
trait → Qdrant/Milvus impl), clusters, and flags outliers / semantic-searches
guardrail-refusal patterns. This connects to `eval-methodology.md` and mirrors
arc's output-validation frontier. **Incubate as an eval capability; measure
precision before it gates anything.**

## 5. Non-goals (explicit)

- **Do NOT** stand up ClickHouse / Qdrant / Redpanda / Benthos / Flink now.
  That fleet infrastructure is massive over-engineering for a personal-mode,
  single-node tool. SQLite + in-process emit is the correct size **today**.
- **Do NOT** adopt Drain for tars's own telemetry (it's structured at source).
  Drain stays confined to the subprocess-stderr anti-corruption boundary.
- **Do NOT** pre-build the semantic-outlier impl; land the trait seam only when
  the silent-failure need is concrete.

## 6. Why the architecture is already migration-ready

Three properties, all present, make every scale swap a drop-in:

1. **Events are the source of truth** — the store is a *sink*, not the model,
   so swapping SQLite→ClickHouse changes nothing upstream.
2. **Sinks are traits** — `PipelineEventStore` / `BodyStore` / `EventStore`
   (§3), injected at the composition root.
3. **A single anchor** — `TraceId` correlates across any future backends
   (ClickHouse rows ↔ a vector store's payload ↔ an OTel trace tree), exactly as
   the fleet stack correlates everything through the trace id.

**Right-sized now; the seams are already the shape of the fleet stack, so scale
is `impl Trait for NewBackend`, not a rebuild.**
