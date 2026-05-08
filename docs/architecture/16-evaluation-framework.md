# Doc 16 — Evaluation Framework

> Scope: produce **multi-dimensional dimension scores** after each LLM call, so consumers see system behavior across multiple semantic axes (schema-compliance rate / rubric-grounding rate / specified-field fill rate / hallucination rate / etc.). Works **online** (alongside production, scoring every call) and **offline** (replaying a dataset to produce a release report).
>
> Upstream: Doc 02 Middleware Pipeline produces the `Response`; Doc 09 Storage Schema's `tars-storage::EventStore` is the persistence layer for evaluation events.
>
> Downstream: anything that wants to see "trends in LLM system behavior" — downstream consumer dogfood dashboards, future admin dashboards, release experiment comparisons, root-cause investigations.
>
> **Explicit non-goals**: this document is about **evaluation** (emit metrics, write time series, do not affect Response), not **validation** (modify / reject Response). Doc 15 already cleanly separates the two. A dimension score suddenly dropping is a diagnostic signal, not a production gate; if you want a gate, use Doc 15's OutputValidator.

---

## 1. Design goals

| Goal | Description |
|---|---|
| **Metric drops → immediately investigable** | Dashboard shows schema_compliance 24h moving average dropped → `trace_id` pinpoints the specific call → see raw (request, response) → root-cause prompt / model / config |
| **Same code, three deployment modes** | Online (triggered per call on the production path) / Sync (caller pulls scores immediately) / Offline (dataset replay over history) — all share one `Evaluator` trait |
| **Stream-first architecture** | Pipeline does not know who subscribes downstream; Pipeline only emits `LlmCallFinished` events to EventStore; evaluators are EventStore consumers + producers. **Pipeline and evaluators do not depend on each other**: adding an evaluator does not touch Pipeline, removing one does not affect production |
| **Deterministic-first** | Built-in evaluators are all deterministic and cheap (schema parse / set membership / regex / counting). LLM-as-judge is an escape hatch — explicitly async, samplable |
| **Replayable** | Run the same evaluator over a historical EventStore dump → reproduce that point-in-time metric snapshot. "Which dimensions flipped before/after last week's prompt change" is one SQL query |
| **Cross-pipeline aggregation by construction** | Multiple Pipelines (different providers / different roles) write to the same EventStore → one dashboard sees everything, no per-Pipeline metric-backend wiring |
| **Dimension-first, against a single "correctness" score** | Lesson from Rotten Tomatoes — a 0.87 score tells you nothing. Each dimension scored independently, tracked independently, composed if needed |

**Non-goals**:

- **No dashboard / UI** — tars provides data (EventStore + a set of SQL view templates); the UI is the consumer's problem
- **No imposed metric backend** (Prometheus / Statsig / DataDog) — EventStore is the source of truth, write an exporter for whichever backend you want; exporters are not v1
- **No alerting** — SQL queries + thresholds are the consumer's responsibility; tars does not ship a rules engine
- **No replacement for Doc 15 validation** — evaluation does not affect Response, validation does not affect dashboards. The two coexist and never interact
- **No LLM-as-judge as built-in** — too expensive, non-deterministic, easily becomes anti-pattern. The trait supports it, but built-ins are all deterministic

---

## 2. Architecture overview

```
                   ┌──────────────────────────────────────────────────┐
                   │  EventStore (append-only stream, source of truth)│
                   │   trace_id   ts_ms   event                       │
                   │   ─────────  ─────   ───────                     │
                   │   abc123     1000    LlmCallFinished{req,resp}   │
                   │   abc123     1003    EvaluationScored{dim,value} │ ← evaluator A writes
                   │   abc123     1005    EvaluationScored{dim,value} │ ← evaluator B writes
                   │   def456     2500    LlmCallFinished{...}        │
                   │   def456     2505    EvaluationScored{...}       │
                   └──────────────────────────────────────────────────┘
                       ▲                            ▲
                       │ write                       │ read + write
                       │                            │
        ┌──────────────┴──────────┐    ┌────────────┴───────────────┐
        │  Pipeline (producer)    │    │  EvaluatorRunner            │
        │   - after call,         │    │   - subscribe LlmCallFinished│
        │     write LlmCallFinished│    │   - run each evaluator      │
        │   - unaware of evaluators│    │   - write EvaluationScored  │
        └─────────────────────────┘    └─────────────────────────────┘
                                                    │
                                                    │ same evaluators, different deploy modes:
                                                    │
                ┌───────────────┬───────────────────┴────────────────┐
                ▼               ▼                                    ▼
         ┌──────────────┐ ┌──────────────┐                  ┌──────────────────┐
         │  Online      │ │  Sync        │                  │  Offline         │
         │  - bg task   │ │  - caller    │                  │  - dataset replay│
         │  - tail live │ │    gets      │                  │  - release report│
         │    EventStore│ │    scores    │                  │  - experiment    │
         │              │ │  - blocking  │                  │    compare       │
         └──────────────┘ └──────────────┘                  └──────────────────┘
```

**Core architectural principle: Pipeline ↔ Evaluator decoupled via events.**

When Pipeline finishes a call, it **unconditionally writes** one `LlmCallFinished` event. It doesn't know who subscribes or how many evaluators will run.

Evaluators are EventStore consumers: subscribe to `LlmCallFinished` → run `score(req, resp)` → write the resulting `Vec<DimensionScore>` as one `EvaluationScored` event.

Dashboards / offline reports / drill-down tools are further-downstream consumers, reading `EvaluationScored` from EventStore to build trend lines.

---

## 3. Core types

### 3.1 `Evaluator` trait

```rust
// Location: tars-eval (new crate) or tars-runtime::eval (submodule, depending on size)
pub trait Evaluator: Send + Sync {
    /// Stable name. Multiple dimensions from one evaluator share this
    /// as a prefix (e.g. RubricEvaluator might emit "rubric.grounded"
    /// and "rubric.ad_hoc_rate"); single-dim evaluators just use
    /// `name()` as the dim.
    fn name(&self) -> &str;

    /// Cost class — controls scheduling. Cheap: synchronous, run on
    /// every call. Expensive: async, may be sampled, may run with
    /// concurrency limit. See §5 deployment modes.
    fn cost_class(&self) -> CostClass { CostClass::Cheap }

    /// Score a (req, resp) pair across one or more dimensions.
    /// Returns `Vec<DimensionScore>` — one evaluator may emit
    /// multiple dimensions (e.g. all rubric stats from one parse),
    /// and they are written as a single atomic `EvaluationScored`
    /// event so downstream group-by aggregation sees a single
    /// timestamp per evaluator-run.
    ///
    /// `hints` carries pass-through context from earlier middleware
    /// in the call (e.g. ValidationMiddleware reporting "fixed an
    /// unescaped newline"). Most evaluators ignore this; read only
    /// when scoring should incorporate what the pipeline already
    /// discovered. Hint keys follow the documented `<owner>.<key>`
    /// namespace convention — see §3.3.
    ///
    /// Sync: cheap, deterministic, no IO.
    fn score(
        &self,
        req: &ChatRequest,
        resp: &Response,
        hints: &Hints,
    ) -> Vec<DimensionScore>;
}

/// Async variant — only implemented when an evaluator genuinely needs
/// IO (LLM-as-judge / ground-truth lookup against a remote KB / etc.).
/// Most evaluators implement Evaluator (sync) only.
#[async_trait]
pub trait AsyncEvaluator: Send + Sync {
    fn name(&self) -> &str;
    fn cost_class(&self) -> CostClass { CostClass::Expensive }
    async fn score_async(
        &self,
        req: &ChatRequest,
        resp: &Response,
        hints: &Hints,
    ) -> Vec<DimensionScore>;
}

pub enum CostClass {
    /// Sub-millisecond, deterministic, no IO. Sync executor.
    Cheap,
    /// Milliseconds-to-seconds, possibly non-deterministic, IO allowed.
    /// Async executor with concurrency cap; sampled when load is high.
    Expensive,
}

#[derive(Clone, Debug)]
pub struct DimensionScore {
    /// Dimension name. Convention: `<evaluator_name>.<dim>` for
    /// multi-dim evaluators; just `<evaluator_name>` for single-dim.
    pub dim: String,
    /// Numerical score. Convention: `0.0..=1.0` for ratios, raw
    /// integer (cast to f64) for counts, milliseconds for durations.
    /// Documentation per evaluator declares which.
    pub value: f64,
    /// For ratio dims: the denominator (so consumers can re-aggregate
    /// without averaging-of-averages bias). E.g. for
    /// `rubric.grounded_rate=0.57`, sample_size=7 means "4 of 7
    /// findings were grounded".
    pub sample_size: u32,
    /// Optional drill-down payload — concrete reasons behind the
    /// score, so the dashboard can render *why* without re-fetching
    /// `LlmCallFinished` and re-running the evaluator's logic in the
    /// reader's head.
    ///
    /// **Only fill when the value alone doesn't explain itself.**
    /// Cheap evaluators (length / schema_compliance: 0 or 1) leave
    /// this `None`. Rubric / hallucination / grounding evaluators
    /// fill it with the specific offending IDs / snippets.
    ///
    /// **Cap at ~4 KiB per score.** Larger diagnostic payloads should
    /// reference a separate event (e.g. via `tars-storage::ContentStore`
    /// once that lands — B-7) rather than inline. Inline blobs above
    /// this size will trigger a tracing warn but won't reject the
    /// score; bloat is a soft constraint.
    ///
    /// Common shapes (per-evaluator doc declares schema):
    ///   {"missing": ["rule_402", "rule_517"], "hallucinated": ["bad_id_123"]}
    ///   {"failed_findings": [{"id": "f-3", "reason": "snippet not in source"}]}
    pub details: Option<serde_json::Value>,
}
```

**Why split sync and async into two traits**:

- Most evaluators are deterministic, pure functions, millisecond-class — synchronous is simplest
- LLM-as-judge / remote KB lookup is async; mixing it into the sync trait forces every evaluator to box::pin futures, polluting the 80% simple cases
- Two traits make cost obvious to the caller — schedule them separately (sync runs synchronously, async goes through a task pool + rate limit)

**Unified value semantics**:

- ratio (`0..=1`) — most dimensions; sample_size required
- count — value is a count; sample_size left as 0
- ms — value is milliseconds; sample_size left as 0
- The doc-comment on each built-in evaluator declares the value type; downstream aggregation code relies on convention, not on a type enum — avoid over-engineering

### 3.3 `Hints` — Pass-through Insights

`Hints` is a **one-way read-only view** written by middleware during the request lifecycle and read by evaluators. It corresponds to a snapshot of `RequestContext.attributes` (frozen into the event payload at the moment Pipeline writes `LlmCallFinished`).

```rust
#[derive(Clone, Debug, Default)]
pub struct Hints {
    inner: HashMap<String, serde_json::Value>,
}

impl Hints {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> { ... }
    pub fn contains_key(&self, key: &str) -> bool { ... }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &serde_json::Value)> { ... }
    // ↑ read-only API — evaluators cannot mutate
}
```

**Standard hint namespaces** (middleware writes with an owner prefix; evaluators read across namespaces; new prefixes must be registered in this table to avoid collisions):

| Prefix | Writer | Example keys |
|---|---|---|
| `validation.*` | ValidationMiddleware (Doc 15) | `validation.format_corrections`: `["unescaped_newline_in_string"]`<br>`validation.dropped_findings`: `["finding_3", "finding_7"]` |
| `cache.*` | CacheLookupMiddleware | `cache.layer`: `"L1"` / `"L2"`<br>`cache.original_call_id`: original trace_id |
| `retry.*` | RetryMiddleware | `retry.attempted_providers`: list of provider ids visited during fallback |
| `routing.*` | RoutingMiddleware | `routing.tier`: `"reasoning"` / `"fast"`<br>`routing.fallback_chain`: candidate ids that were considered |
| `caller.*` | Application-level hints injected explicitly by the caller into RequestContext | `caller.session_id` / `caller.user_id` (as needed) |

**Constraints**:
- Evaluators are **read-only** — cannot mutate `Hints`, preserving the core invariant that evaluators do not affect the production path
- Hint keys must use a namespace prefix (flat keys without a prefix are anti-pattern), preventing different middleware from clobbering each other's keys
- The middleware writing a hint owns documentation declaring which keys it writes and what schema they have — add fields when needed, do not stuff them in silently

**Typical use case**:

```rust
// ValidationMiddleware writes during Filter:
ctx.attributes.write().unwrap().insert(
    "validation.format_corrections".into(),
    json!(["unescaped_newline_in_string", "trailing_comma_removed"]),
);

// FormatRobustnessEvaluator consumes:
fn score(&self, req: &ChatRequest, resp: &Response, hints: &Hints) -> Vec<DimensionScore> {
    let corrections = hints.get("validation.format_corrections")
        .and_then(|v| v.as_array())
        .map(|a| a.len() as u32)
        .unwrap_or(0);
    // 0 corrections → 1.0; N corrections → 1.0 - 0.1 * N (clamped 0)
    let score = (1.0 - 0.1 * corrections as f64).max(0.0);
    vec![DimensionScore {
        dim: "format_robustness".into(),
        value: score,
        sample_size: 1,
        details: if corrections > 0 {
            Some(json!({"corrections_applied": corrections}))
        } else { None },
    }]
}
```

This way the evaluator does not need to re-run regexes to detect the same things — it directly consumes facts validation already discovered.

### 3.4 New events

```rust
// tars-runtime::event adds three:

pub enum AgentEvent {
    ...existing...

    /// Emitted by Pipeline when one successful call completes.
    /// EvaluatorRunner subscribes to this. Payload uses
    /// `serde_json::Value` rather than hard-coding ChatRequest /
    /// ChatResponse — see §3.5 "why wire untyped, runtime typed".
    LlmCallFinished {
        trace_id: TraceId,
        /// `chat` / `embedding` / `completion` / `Other(...)`,
        /// EvaluatorRunner uses this to pick the typed evaluator path.
        modality: Modality,
        /// Serialized form of the provider-shape-specific request payload.
        /// Under Modality::Chat this is the JSON serialization of `ChatRequest`.
        request: serde_json::Value,
        /// Same; under Modality::Chat this is the JSON serialization of `ChatResponse`.
        response: serde_json::Value,
        /// Snapshot of `RequestContext.attributes` (frozen at finish-time).
        /// Middleware writes; evaluators read via the `Hints` view (§3.3).
        processing_hints: serde_json::Value,
        ts_ms: u64,
    },

    /// Written when an evaluator finishes scoring one (req, resp). One
    /// evaluator run produces exactly one EvaluationScored containing
    /// a `Vec<DimensionScore>` — **all dims in the batch share one
    /// timestamp, strictly aligned for group-by aggregation**.
    EvaluationScored {
        trace_id: TraceId,                // links back to LlmCallFinished
        evaluator_name: String,
        scores: Vec<DimensionScore>,      // ← atomic batch, no longer one event per dim
        kind: EvalKind,
        ts_ms: u64,
    },

    /// Written when the evaluator itself blew up (panic / timeout / IO error / etc.).
    /// **Must be a separate variant, not folded into EvaluationScored**:
    /// - dashboards can distinguish "model output regressed" vs "evaluator broke"
    /// - SQL `WHERE event_type='EvaluationFailed'` directly pulls failure windows;
    ///   `AVG(value)` is not contaminated by half-data
    /// - failure telemetry and success telemetry never cross-contaminate queries
    EvaluationFailed {
        trace_id: TraceId,
        evaluator_name: String,
        error_kind: EvalErrorKind,
        message: String,                  // detailed error (panic msg / IO error)
        elapsed_ms: u64,                  // time spent before failing (timeout diagnosis)
        kind: EvalKind,
        ts_ms: u64,
    },
}

pub enum Modality {
    /// LLM chat / completion shape — `request: ChatRequest`,
    /// `response: ChatResponse`. All current tars-pipeline traffic uses this.
    Chat,
    /// Embedding model — request: prompt(s), response: vector(s).
    /// Not yet implemented; the enum slot is reserved to avoid breaking EventStore schema later.
    Embedding,
    /// Old-style (non-chat) completion — string in/out, no message boundary.
    Completion,
    /// Forward-compat: when replaying old EventStore dumps across versions,
    /// new modalities still deserialize on old tars; typed evaluators just
    /// skip — generic untyped evaluators (if any) see raw JsonValue.
    Other(String),
}

pub enum EvalKind {
    /// Online — produced alongside a production call (written by the Online runner)
    Online,
    /// Offline — produced by a dataset run (written by the Offline runner);
    /// `dataset_id` identifies which dataset / which experiment
    Offline { dataset_id: String },
}

pub enum EvalErrorKind {
    /// Sync evaluator panicked — caught by the runner's catch_unwind
    Panic,
    /// async evaluator hit tokio::time::timeout — typically LLM-judge / IO
    Timeout,
    /// async evaluator's external dependency failed (HTTP / DB / KB)
    AsyncIoError,
    /// A dependency like source_lookup is unavailable when the evaluator expects it
    DependencyMissing,
    /// Payload schema couldn't be parsed — LlmCallFinished data is already broken
    /// (a common case for cross-version compatibility fallback failures)
    SchemaSkewed,
    /// Other internal error that doesn't fit a category
    Internal(String),
}
```

**Schema evolution notes**:

- `LlmCallFinished.request` / `.response` are `serde_json::Value`; **EventStore is not bound to any typed schema** — adding/changing fields on `ChatRequest`, introducing Embedding modality, etc., still deserializes (JsonValue is the most permissive shape)
- New event variants (`Modality::Audio` etc.) use `#[serde(other)]` as a wildcard fallback so old tars can still read new dumps
- `EvalErrorKind` uses `Internal(String)` as a catch-all; adding new variants doesn't break old events

### 3.5 Why wire untyped, runtime typed

EventStore wire format uses `serde_json::Value`; **the API the evaluator sees uses typed `&ChatRequest` / `&Response`**. EvaluatorRunner is the boundary translator:

| Layer | Shape | Rationale |
|---|---|---|
| **EventStore wire** (event payload fields) | `serde_json::Value` | Cross-tars-version replay / cross-modality compatibility / freedom for schema evolution / third-party analysis tools don't have to link tars-types |
| **Evaluator API surface** (the args evaluators see) | typed `&ChatRequest, &Response, &Hints` | IDE autocomplete / compile-time schema check / PyO3 wrapping for Python evaluators is also a typed dict (matching the `Pipeline.complete()` Response shape) |

```rust
// Boundary translation inside EvaluatorRunner:
match modality {
    Modality::Chat => {
        let req: ChatRequest = serde_json::from_value(payload.request)
            .map_err(|e| EvalErrorKind::SchemaSkewed)?;
        let resp: ChatResponse = serde_json::from_value(payload.response)
            .map_err(|e| EvalErrorKind::SchemaSkewed)?;
        let hints = Hints::from_value(payload.processing_hints);
        for e in &self.chat_evaluators {
            // typed evaluator API
            let scores = e.score(&req, &resp, &hints);
            ...
        }
    }
    Modality::Embedding => { /* EmbeddingRequest / EmbeddingResponse */ }
    Modality::Other(_) => { /* typed evaluators skip; only untyped run */ }
}
```

**The case against "hardcode ChatRequest/ChatResponse everywhere"**: binding wire format to runtime types means schema evolution requires a migration step every time. In LLM land, modalities turn over every 12 months (chat → tool_use → reasoning → real-time-audio → multimodal); EventStore can't follow each rev.

**The case against "JsonValue everywhere including the evaluator API"**: evaluators lose typed contract, are painful to write, IDE can't help, PyO3 binding can only return a dict — inconsistent with the typed Response shape tars already established.

The compromise: EventStore is a stable facts log; code is its typed interpretation.

---

## 4. Two-layer decoupling: `CallEventChannel` + `MetricsSink`

Pipeline **writes a channel** (not EventStore directly) when a call completes; a background worker pool consumes from the channel, runs evaluators, and writes results to a `MetricsSink`. The point of two abstractions:

| Abstraction | Responsibility | v1 implementation | Future extensions |
|---|---|---|---|
| **`CallEventChannel`** | Transport from Pipeline → Evaluator pool | `tokio::sync::mpsc<CallEvent>` (in-process) | `TcpEventChannel` (sidecar over network) |
| **`MetricsSink`** | Evaluator results → persistence/export | `SqliteEventStoreSink` (writes local SQLite) | `PrometheusSink` / `DataDogSink` / `CompoundSink` (fan-out) |

**Design principles**:

- The Pipeline path **never blocks, never knows who is listening downstream** — it just `channel.send(event).await`, returning in microseconds
- The worker pool is **out-of-band** — pool full / evaluator slow / sink write failed: **the main path is entirely unaffected**
- Channel implementation is swappable: v1 is in-process mpsc, future could be a network channel cutting over to sidecar deployment, **without touching one line of Pipeline code**
- Sinks compose: default SQLite, add Prometheus by wrapping `CompoundSink([SqliteSink, PrometheusSink])`, **with no framework change**

This matches OpenTelemetry Collector's agent-vs-collector layering — agent in-process by default, collector when scale demands.

### 4.1 `CallEventChannel` trait

```rust
// Location: tars-eval::channel
pub trait CallEventChannel: Send + Sync {
    /// Send one LlmCallFinished event to the downstream evaluator pool.
    /// The implementation should be non-blocking (or at most very brief
    /// backpressure block) — the Pipeline main path cannot wait for the
    /// metrics path. Dropping the event and returning Err is preferable
    /// to blocking (metrics is best-effort).
    fn send(&self, ev: CallEvent) -> Result<(), ChannelError>;
}

#[derive(Clone, Debug)]
pub enum CallEvent {
    /// LLM call succeeded. Eval pool runs success-path evaluators.
    Finished(LlmCallFinishedPayload),
    /// LLM call failed. Eval pool runs failure-path evaluators (if any)
    /// + writes the sink for an audit trail.
    Failed(LlmCallFailedPayload),
}

pub enum ChannelError {
    /// Channel full — configured buffer size / pool concurrency is
    /// insufficient and metrics can't keep up with production rate.
    /// When Pipeline gets this, it should log + drop, not block the main path.
    Full,
    /// Channel closed (sidecar offline / pool shutdown)
    Closed,
}

/// v1 in-process implementation: bounded mpsc.
pub struct LocalChannel {
    tx: tokio::sync::mpsc::Sender<CallEvent>,
}

impl CallEventChannel for LocalChannel {
    fn send(&self, ev: CallEvent) -> Result<(), ChannelError> {
        // try_send: when full, drop and return Full; never block the Pipeline.
        self.tx.try_send(ev).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => ChannelError::Full,
            mpsc::error::TrySendError::Closed(_) => ChannelError::Closed,
        })
    }
}
```

**What to do when the buffer is full**: v1 default is `try_send` + drop. **Metrics are best-effort and must not drag production**. tracing::warn records the drop count; if drops are frequent the evaluator pool isn't keeping up — increase worker count or shed evaluators (a downstream problem, not for the main pipe to solve).

### 4.2 `MetricsSink` trait

```rust
// Location: tars-eval::sink
#[async_trait]
pub trait MetricsSink: Send + Sync {
    /// LlmCallFinished arrived (eval pool also wants to persist raw req+resp for drill-down).
    async fn write_call_finished(&self, ev: LlmCallFinishedPayload)
        -> Result<(), SinkError>;
    
    /// Evaluator ran successfully.
    async fn write_eval_scored(&self, ev: EvaluationScoredPayload)
        -> Result<(), SinkError>;
    
    /// Evaluator itself blew up.
    async fn write_eval_failed(&self, ev: EvaluationFailedPayload)
        -> Result<(), SinkError>;
}

/// v1 implementation: writes tars-storage::EventStore.
pub struct SqliteEventStoreSink {
    store: Arc<dyn EventStore>,
}

#[async_trait]
impl MetricsSink for SqliteEventStoreSink {
    async fn write_call_finished(&self, ev: LlmCallFinishedPayload) -> Result<(), SinkError> {
        // wrap LlmCallFinishedPayload into AgentEvent::LlmCallFinished
        // call EventStore::append
        ...
    }
    // similarly for the other two
}

/// Multi-sink fan-out — e.g. write SQLite + Prometheus simultaneously
pub struct CompoundSink {
    sinks: Vec<Arc<dyn MetricsSink>>,
}

#[async_trait]
impl MetricsSink for CompoundSink {
    async fn write_eval_scored(&self, ev: EvaluationScoredPayload) -> Result<(), SinkError> {
        // write all sinks concurrently; one failure does not affect the others
        let futures: Vec<_> = self.sinks.iter().map(|s| s.write_eval_scored(ev.clone())).collect();
        let results = futures::future::join_all(futures).await;
        // at least one success → success (metric-fault-tolerant); only return error if all fail
        if results.iter().any(|r| r.is_ok()) { Ok(()) } else { Err(...) }
    }
    // similarly for the others
}
```

**Future sink implementation examples** (none in v1 scope; just demonstrating the trait shape is open enough):

- `PrometheusSink` — `EvaluationScored` → counter / gauge metric
- `DataDogSink` — push metrics to DataDog API
- `OtelMetricsSink` — bridge to OpenTelemetry meter
- `S3ArchiveSink` — archive large LlmCallFinished payloads to S3, leaving trace_id in the main sink

### 4.3 Pipeline changes

Pipeline **only deals with `CallEventChannel`**, never directly touches sinks or EventStore:

```rust
// tars-pipeline::Pipeline
pub struct Pipeline {
    ...existing...
    /// Optional: when absent, no metric events are sent (test fixtures / temp pipelines).
    /// Required from the builder in production.
    call_channel: Option<Arc<dyn CallEventChannel>>,
}

impl Pipeline {
    // At the outermost wrapper:
    async fn complete(...) -> Result<Response, ProviderError> {
        let result = self.inner.call(...).await;
        
        if let Some(ch) = &self.call_channel {
            let ev = match &result {
                Ok(resp) => CallEvent::Finished(LlmCallFinishedPayload {
                    trace_id: ctx.trace_id.clone(),
                    modality: Modality::Chat,
                    request: serde_json::to_value(&req).unwrap_or(json!(null)),
                    response: serde_json::to_value(resp).unwrap_or(json!(null)),
                    processing_hints: ctx.attributes_snapshot(),
                    ts_ms: now_ms(),
                }),
                Err(e) => CallEvent::Failed(LlmCallFailedPayload { ... }),
            };
            // Non-blocking send. Full/closed → log + drop, NEVER block the main path.
            if let Err(e) = ch.send(ev) {
                tracing::warn!(error = ?e, "metrics channel send failed");
            }
        }
        
        result
    }
}
```

**Key invariants**:
- Pipeline calling `ch.send()` is **synchronous, non-blocking, never awaits** — failure is logged and dropped
- Total latency cost of the metric path = one `try_send` atomic op ≈ a few nanoseconds
- Slow sinks, slow evaluators, network write failures — **all happen on the worker pool side of the world**, fully isolated from the Pipeline main path

### 4.2 SQL query patterns

When EventStore uses `tars-storage::SqliteEventStore`, the `payload` field is a JSON string. `EvaluationScored.scores` is an array; common queries unfold it with `json_each`. Supported on SQLite 1.38+ / PostgreSQL.

**Moving average of one dim over the past 1h** (`json_each` unrolls the scores array):

```sql
SELECT
  strftime('%Y-%m-%d %H:%M', e.ts_ms/1000, 'unixepoch') AS bucket,
  AVG(CAST(json_extract(s.value, '$.value') AS REAL)) AS avg,
  COUNT(*) AS n
FROM events e, json_each(json_extract(e.payload, '$.scores')) s
WHERE e.event_type = 'EvaluationScored'
  AND json_extract(s.value, '$.dim') = 'schema_compliance'
  AND e.ts_ms > strftime('%s', 'now', '-1 hour') * 1000
GROUP BY bucket
ORDER BY bucket;
```

**Drill-down — trace_ids in the past 1h with schema_compliance < 0.5 plus their details**:

```sql
SELECT
  json_extract(e.payload, '$.trace_id')        AS tid,
  CAST(json_extract(s.value, '$.value') AS REAL) AS score,
  json_extract(s.value, '$.details')           AS why  -- ← drill-down gives the reason directly
FROM events e, json_each(json_extract(e.payload, '$.scores')) s
WHERE e.event_type = 'EvaluationScored'
  AND json_extract(s.value, '$.dim') = 'schema_compliance'
  AND CAST(json_extract(s.value, '$.value') AS REAL) < 0.5
  AND e.ts_ms > strftime('%s', 'now', '-1 hour') * 1000;
```

**Pull raw req+resp for the specific call using the trace_id above**:

```sql
SELECT payload FROM events
WHERE event_type = 'LlmCallFinished'
  AND json_extract(payload, '$.trace_id') IN (...);
```

**Cross-dim correlation — relationship between one dim and retry_count over the same window**:

```sql
SELECT
  CAST(json_extract(s.value, '$.value') AS REAL)         AS schema_score,
  CAST(json_extract(call.payload, '$.response.telemetry.retry_count') AS INT) AS retries
FROM events eval, json_each(json_extract(eval.payload, '$.scores')) s
JOIN events call ON json_extract(call.payload, '$.trace_id') = json_extract(eval.payload, '$.trace_id')
WHERE eval.event_type = 'EvaluationScored'
  AND json_extract(s.value, '$.dim') = 'schema_compliance'
  AND call.event_type = 'LlmCallFinished'
  AND eval.ts_ms > strftime('%s', 'now', '-1 day') * 1000;
```

**Evaluator health — distinguishing "score is low" vs "evaluator broke"** (key health-check):

```sql
-- Compute success rate and avg score per evaluator side by side.
-- avg_score comes from EvaluationScored, failure_count from EvaluationFailed;
-- union the two queries to immediately see which side is problematic.
SELECT
  evaluator_name,
  -- success path
  COUNT(*) FILTER (WHERE event_type = 'EvaluationScored') AS scored,
  AVG(CASE WHEN event_type = 'EvaluationScored'
           THEN (
             SELECT AVG(CAST(json_extract(s.value, '$.value') AS REAL))
             FROM json_each(json_extract(payload, '$.scores')) s
           ) END) AS avg_score,
  -- failure path
  COUNT(*) FILTER (WHERE event_type = 'EvaluationFailed') AS failed,
  -- top failure reason
  (SELECT json_extract(payload, '$.error_kind')
   FROM events e2
   WHERE e2.event_type = 'EvaluationFailed'
     AND json_extract(e2.payload, '$.evaluator_name') = evaluator_name
   GROUP BY json_extract(e2.payload, '$.error_kind')
   ORDER BY COUNT(*) DESC LIMIT 1) AS top_error_kind
FROM (
  SELECT json_extract(payload, '$.evaluator_name') AS evaluator_name,
         event_type,
         payload
  FROM events
  WHERE event_type IN ('EvaluationScored', 'EvaluationFailed')
    AND ts_ms > strftime('%s', 'now', '-1 day') * 1000
)
GROUP BY evaluator_name;
```

**Evaluator failure window drill-down**:

```sql
-- "Which evaluators panicked / timed out in the past 1h"
SELECT
  json_extract(payload, '$.evaluator_name') AS evaluator,
  json_extract(payload, '$.error_kind')      AS error_kind,
  json_extract(payload, '$.message')         AS message,
  json_extract(payload, '$.elapsed_ms')      AS elapsed_ms,
  ts_ms
FROM events
WHERE event_type = 'EvaluationFailed'
  AND ts_ms > strftime('%s', 'now', '-1 hour') * 1000
ORDER BY ts_ms DESC;
```

We provide a `tars-eval::sql::common_queries` module as templates; consumers reuse them directly without each downstream consumer having to write `json_each` unrolling themselves.

---

## 5. Three deployment modes

The same `Evaluator` trait, three deployment shapes; the only differences are "event source + scheduling + write location".

### 5.1 Online (recommended primary path)

**Use case**: emit metrics for every production call, dashboards show trends in real time.

```rust
// Spawn the background runner:
let runner = OnlineEvaluatorRunner::new(
    event_store.clone(),
    vec![
        Box::new(SchemaComplianceEvaluator),
        Box::new(RubricGroundingEvaluator::new(rubric_paths)),
        Box::new(AdHocRateEvaluator),
        Box::new(EvidenceFilledEvaluator),
    ],
);
tokio::spawn(runner.run());

// Inside the runner:
//   tail EventStore;
//   for each LlmCallFinished:
//       for each evaluator (cheap, sync): score, append EvaluationScored
//       for each evaluator (expensive, async): tokio::spawn(score_async, append)
```

Properties:
- **Fully decoupled** from production pipeline — production path is unaware of evaluators
- Latency: cheap evaluators land in EventStore in milliseconds; expensive ones run async, taking seconds to tens of seconds
- Replay-safe: runner restarts continue from the last checkpoint (see §6.1 offset tracking)
- Failure isolation: a single evaluator throwing only loses that one row of EvaluationScored — does not affect other evaluators or production

### 5.2 Sync (escape hatch)

**Use case**: caller wants the score immediately (rare, e.g. deciding whether to fall back based on the score).

```rust
// Score synchronously after a single call:
let resp = pipeline.complete(req, ctx).await?;
let scores: Vec<DimensionScore> = sync_evaluators
    .iter()
    .flat_map(|e| e.score(&req, &resp))
    .collect();
if scores.iter().any(|s| s.dim == "schema_compliance" && s.value < 0.3) {
    // caller decides to degrade / retry / error
}
```

Properties:
- Blocks the Response return — only cheap evaluators are appropriate
- caller gets `Vec<DimensionScore>` and decides on the spot
- **Not the default mode** — most callers should use Online; sync is only for "score must immediately influence behavior"
- Note: if what you want is "score low → reject and retry", that's not evaluation, that's **Doc 15 validation** — don't conflate them

### 5.3 Offline (release gate / experiment compare)

**Use case**: pre-release run over a historical dataset producing a release report; A/B prompt-experiment comparison.

```rust
// Read snapshot of past events:
let events = SqliteEventStore::open("./snapshot-2026-05-04.db")?;
let runner = OfflineEvaluatorRunner::new(
    events,
    evaluators,
    DatasetId::new("prompt-experiment-v1.2"),
);
let report = runner.run_to_completion().await?;
report.write_csv("./report-v1.2.csv");
```

Properties:
- Runs over historical EventStore dumps; never touches production
- Can run LLM-as-judge expensive evaluators (does not affect production latency)
- `DatasetId` tags this batch of EvaluationScored as belonging to a specific experiment, easy to filter in subsequent SQL
- **Same evaluator implementations, identical to online** — that's the architectural payoff. If it works well online, it just works offline too

---

## 6. EvaluatorRunner implementation

### 6.1 Online runner

**v1 design: consume from `CallEventChannel`, write to `MetricsSink`.** In-process tokio task pool, configurable worker count.

```rust
pub struct OnlineEvaluatorRunner {
    /// Receives LlmCallFinished / Failed sent by Pipeline
    rx: tokio::sync::mpsc::Receiver<CallEvent>,
    /// Result sink — defaults to SqliteEventStoreSink, swappable / stackable
    sink: Arc<dyn MetricsSink>,
    sync_evals: Vec<Arc<dyn Evaluator>>,
    async_evals: Vec<Arc<dyn AsyncEvaluator>>,
    /// Several worker tokio tasks consume the channel concurrently. Suggested: CPU cores / 2.
    worker_count: usize,
    /// Global concurrency cap for expensive evaluators — prevents LLM-as-judge from saturating the background.
    async_concurrency_cap: Arc<Semaphore>,
}

impl OnlineEvaluatorRunner {
    pub async fn run(self: Arc<Self>) {
        // Spawn worker_count workers, sharing rx via Arc<Mutex<Receiver>>
        // (or broadcast / task-pool pattern — implementation detail per trait choice)
        let mut handles = Vec::new();
        for _ in 0..self.worker_count {
            let runner = self.clone();
            handles.push(tokio::spawn(async move { runner.worker_loop().await }));
        }
        // Wait for all workers to exit (when the channel closes)
        for h in handles { let _ = h.await; }
    }

    async fn worker_loop(self: Arc<Self>) {
        while let Some(ev) = self.rx_recv().await {
            match ev {
                CallEvent::Finished(payload) => self.score_one(payload).await,
                CallEvent::Failed(payload) => self.write_failure_audit(payload).await,
            }
        }
    }

    async fn score_one(&self, payload: LlmCallFinishedPayload) {
        // 0. Persist raw call for later drill-down
        let _ = self.sink.write_call_finished(payload.clone()).await;

        // 1. Modality-specific deserialize
        let (req, resp) = match payload.modality {
            Modality::Chat => match (
                serde_json::from_value::<ChatRequest>(payload.request.clone()),
                serde_json::from_value::<ChatResponse>(payload.response.clone()),
            ) {
                (Ok(r), Ok(s)) => (r, s),
                _ => {
                    self.write_eval_failed(&payload, "deserialize", EvalErrorKind::SchemaSkewed,
                        "could not decode chat req/resp from event payload").await;
                    return;
                }
            },
            // Embedding / Completion / Other: typed evaluators skip; generic untyped path (future)
            _ => return,
        };
        let hints = Hints::from_value(&payload.processing_hints);

        // 2. Sync evaluators — run panic-safe inline
        for e in &self.sync_evals {
            let started = Instant::now();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                e.score(&req, &resp, &hints)
            }));
            match result {
                Ok(scores) => {
                    let _ = self.sink.write_eval_scored(EvaluationScoredPayload {
                        trace_id: payload.trace_id.clone(),
                        evaluator_name: e.name().to_string(),
                        scores,
                        kind: EvalKind::Online,
                        ts_ms: now_ms(),
                    }).await;
                }
                Err(p) => {
                    self.write_eval_failed(&payload, e.name(), EvalErrorKind::Panic,
                        &panic_message(&p)).await;
                }
            }
        }

        // 3. Async evaluators — concurrency-cap + timeout
        for e in &self.async_evals {
            let permit = self.async_concurrency_cap.clone().acquire_owned().await.ok();
            let sink = self.sink.clone();
            let payload = payload.clone();
            let req = req.clone();
            let resp = resp.clone();
            let hints = hints.clone();
            let e = e.clone();
            tokio::spawn(async move {
                let _permit = permit;  // hold until task ends
                let started = Instant::now();
                let timeout = Duration::from_secs(30);  // default timeout, configurable
                match tokio::time::timeout(timeout, e.score_async(&req, &resp, &hints)).await {
                    Ok(scores) => {
                        let _ = sink.write_eval_scored(EvaluationScoredPayload {
                            trace_id: payload.trace_id.clone(),
                            evaluator_name: e.name().to_string(),
                            scores,
                            kind: EvalKind::Online,
                            ts_ms: now_ms(),
                        }).await;
                    }
                    Err(_) => {
                        let _ = sink.write_eval_failed(EvaluationFailedPayload {
                            trace_id: payload.trace_id.clone(),
                            evaluator_name: e.name().to_string(),
                            error_kind: EvalErrorKind::Timeout,
                            message: format!("async evaluator timeout after {:?}", timeout),
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            kind: EvalKind::Online,
                            ts_ms: now_ms(),
                        }).await;
                    }
                }
            });
        }
    }
}
```

**Key decisions**:

- **`catch_unwind` shields against sync evaluator panics** — one evaluator crashing does not affect others or the worker itself
- **`tokio::time::timeout` shields async evaluators** — prevents an LLM-judge from hanging an entire worker
- **Both panic and timeout are turned into `EvaluationFailed` events** written to the sink — dashboards see "evaluator failure" as a distinct signal
- **worker_count is configurable** — 1-2 for a Mac laptop, scale with CPU on servers. When the worker pool is full, backpressure is signaled via channel `Full` to Pipeline; **Pipeline drops the event + logs, main path stays unblocked**

**Checkpoint persistence**: in-process mpsc has no checkpoint concept — the channel closes and stops. **v1 does not persist checkpoints** — after restart **only new events are processed**; for historical dumps, use OfflineRunner. Once moved to a sidecar, the channel side becomes responsible for checkpointing (cf. Kafka consumer offset pattern); the main tars process does not carry that responsibility.

### 6.2 Offline runner

```rust
pub struct OfflineEvaluatorRunner {
    /// Typically an iterator from SqliteEventStore::iter_calls() that
    /// scans a historical dump for all LlmCallFinished entries
    events: Box<dyn Iterator<Item = LlmCallFinishedPayload>>,
    sync_evals: Vec<Arc<dyn Evaluator>>,
    async_evals: Vec<Arc<dyn AsyncEvaluator>>,
    /// Tags this batch of scores with which dataset / experiment they belong to.
    /// Dashboards use dataset_id to distinguish `prompt-v1.2` vs `prompt-v1.3` etc.
    dataset_id: DatasetId,
    /// Same as OnlineRunner — sink target, defaults to SqliteEventStoreSink,
    /// but tagged with `kind: Offline { dataset_id }`.
    sink: Arc<dyn MetricsSink>,
}

impl OfflineEvaluatorRunner {
    pub async fn run_to_completion(self) -> Report {
        let mut report = Report::new();
        for payload in self.events {
            // Same deserialize as Online runner
            let (req, resp) = match deserialize_chat(&payload) {
                Ok(pair) => pair,
                Err(e) => { report.record_error(...); continue; }
            };
            let hints = Hints::from_value(&payload.processing_hints);

            // Sync evaluators — run directly without catch_unwind (offline lets panics surface bugs)
            for e in &self.sync_evals {
                let scores = e.score(&req, &resp, &hints);
                self.sink.write_eval_scored(EvaluationScoredPayload {
                    trace_id: payload.trace_id.clone(),
                    evaluator_name: e.name().to_string(),
                    scores: scores.clone(),
                    kind: EvalKind::Offline { dataset_id: self.dataset_id.clone() },
                    ts_ms: now_ms(),
                }).await.ok();
                for s in scores {
                    report.record(&self.dataset_id, &payload.trace_id, e.name(), s);
                }
            }
            // Async evaluators — offline does not rate-limit and runs sequentially,
            // because outside the production path, running every evaluator is the point
            for e in &self.async_evals {
                let scores = e.score_async(&req, &resp, &hints).await;
                self.sink.write_eval_scored(EvaluationScoredPayload {
                    trace_id: payload.trace_id.clone(),
                    evaluator_name: e.name().to_string(),
                    scores: scores.clone(),
                    kind: EvalKind::Offline { dataset_id: self.dataset_id.clone() },
                    ts_ms: now_ms(),
                }).await.ok();
                for s in scores {
                    report.record(&self.dataset_id, &payload.trace_id, e.name(), s);
                }
            }
        }
        report
    }
}
```

The `Report` type aggregates all dimensions × all calls and provides convenience methods like `to_csv` / `to_json` / `summary()`. All EvaluationScored entries are **also written to the sink** (with `kind: Offline { dataset_id }`), so subsequent SQL on EventStore can also query offline-produced data.

**The bodies of Online and Offline are nearly identical** — that's the win from `MetricsSink` + a single `Evaluator` trait abstraction: switching deployment mode leaves 90% of the code untouched.

---

## 7. Built-in evaluators

Grouped by cost class. **v1 ships only deterministic & cheap** ones — LLM-judge types are not v1 built-ins; the trait supports them, but it's left to callers to implement.

### 7.1 Cheap / deterministic (v1 built-ins)

#### `SchemaComplianceEvaluator`

```rust
pub struct SchemaComplianceEvaluator {
    schema: serde_json::Value,
}
```

Behavior: try to parse `resp.text` as JSON, then validate with jsonschema.
Output: `schema_compliance` dimension, 0.0 or 1.0.

#### `RubricGroundingEvaluator`

```rust
pub struct RubricGroundingEvaluator {
    json_path: String,             // "$.findings[*].rule_id"
    allowed: HashSet<String>,
}
```

Outputs multiple dimensions:
- `rubric.grounded_rate`: ratio of rule_ids in the allowlist (ratio, sample_size = total findings)
- `rubric.ad_hoc_rate`: ratio of ad-hoc findings
- `rubric.hallucinated_rate`: ratio that's neither in the allowlist nor ad-hoc

#### `FieldFilledRateEvaluator`

```rust
pub struct FieldFilledRateEvaluator {
    json_path: String,             // "$.findings[*].evidence"
    name: String,                  // "evidence_filled" → dimension name
}
```

Output: `<name>` dimension, ratio of non-empty fields. E.g. downstream consumer's evidence-tag fill rate.

#### `RegexMatchCountEvaluator`

```rust
pub struct RegexMatchCountEvaluator {
    field: ResponseField,         // Text | Thinking | ToolCallsArgs
    pattern: regex::Regex,
    name: String,
    /// `Count` = raw count output; `PerThousandChars` = count / (chars/1000),
    /// the latter for cross-length response comparability.
    normalize: NormalizeMode,
}
```

Generic tool — lets callers compose "frequency of an offending pattern" etc.

#### `LengthEvaluator`

```rust
pub struct LengthEvaluator {
    field: ResponseField,
    name: String,
}
```

Outputs `<name>.chars`, `<name>.tokens` dimensions. Lowest cost; add it to every call as a baseline.

#### `SnippetGroundingEvaluator`

```rust
pub struct SnippetGroundingEvaluator {
    json_path: String,            // "$.findings[*].snippet"
    /// Source provider — given a snippet, decides whether it actually appears in the source file.
    /// Injected at construction; does no IO at runtime.
    source_lookup: Arc<dyn Fn(&ChatRequest) -> Option<String> + Send + Sync>,
}
```

Output: `snippet.grounded_rate` — ratio of finding snippets that appear in the source.

Note `source_lookup` is a closure injected at construction — keeps the evaluator synchronous and IO-free. Anything needing a database query for source goes through async evaluators (see §7.2).

### 7.2 Expensive / non-deterministic (v1 trait support, no built-ins)

The `AsyncEvaluator` trait lets callers implement these. Two common categories:

- **LLM-as-judge** — calls a judge model to score the response. Slow, expensive, non-deterministic. tars does not ship one; if you want it, write your own `LlmJudgeEvaluator { judge_pipeline: Arc<Pipeline>, prompt_template: String }`.
- **Ground-truth retrieval** — compare against a labeled corpus (precision / recall). Requires labeled data + retrieval. tars does not ship one.

The doc section gives an **example implementation** — but as an example, not as a built-in, to avoid pushing users toward it where they shouldn't use it.

---

## 8. Anti-patterns (explicitly avoided)

Source: [Fractional AI: Your evals have a Rotten Tomatoes problem](https://blog.fractionalai.com) (2026-02). This section restates the three most important and applies them to tars design.

### 8.1 A single "correctness" score = Rotten Tomatoes

Wrong: write an `OverallQualityEvaluator` that emits a single `quality` score.

Reason: collapses every dimension into one number. When 0.87 → 0.81, you can't tell which axis dropped.

Right: split dimensions: schema_compliance / grounding / hallucination_rate / evidence_filled / length / each tracked independently. Each is a separate SQL query with its own trend line.

Composition is done **on the consumer side**: the dashboard's "quality I care about" is a weighted sum of several dims — that's **configuration**, not a built-in evaluator in tars.

### 8.2 LLM-as-judge as default reach

Wrong: any evaluation problem instinctively dispatched to another LLM with `Rate this 0-1: ...`.

Reason:
- Slow — one extra LLM call per dim
- Expensive — counts toward token cost
- Non-deterministic — same input scored twice gives different results, dashboards jitter so you can't see the trend
- Easy to ask a judge to evaluate too many things in one query, falling back into §8.1

Right: deterministic-first: schema check / set membership / regex / counting / grounding-by-substring. These cover 80%+ of practical dimension needs, in milliseconds, deterministic, repeatable.

Only when something genuinely cannot be expressed deterministically ("is this response's tone appropriate") do you reach for LLM-judge — and even then, build it as an async evaluator, sample it, rate-limit it.

### 8.3 Treat evaluation as a gate

Wrong: dashboard sees schema_compliance < 0.8 and pages the SRE; or evaluator internally does if-score-low-then-reject.

Reason: evaluation is a dashboard, not a switch. Using it as a gate immediately degrades it into a §8.1 single correctness score, and ties switch latency to the evaluation runner (async) — latency + non-determinism = both false positives and missed signals.

Right: evaluation + validation are two separate things:
- Dashboard drops → investigate root cause → adjust prompt / config / model
- Want a gate ("score low → reject and retry") → write a Doc 15 OutputValidator

### 8.4 Evaluate the whole pipeline instead of single LLM calls

Wrong: only adding an evaluator at the final `run_task` output: "what's this task's overall score".

Reason: a pipeline contains multiple LLM calls (orchestrator + worker + critic). When the final score drops, you can't tell which call's which stage regressed.

Right: attach evaluators to every LLM call — since events are written per call (`LlmCallFinished` per call, not per task), evaluators are naturally per-call granularity. Dashboard dimensions can carry an `agent_role` tag distinguishing critic / worker / orchestrator — that's a SQL filter, not a new evaluator.

### 8.5 One base class wrapping all outcomes across languages

Not in our design — `Evaluator` returns `Vec<DimensionScore>` instead of an `EvaluationOutcome` enum. Each dim is an independent data point; there is no "Filter / Reject / Annotate" disposition. Listing this anti-example as a reminder: **evaluator is not validator; do not reuse `ValidationOutcome`.**

---

## 9. Usage patterns

### 9.1 Downstream consumer dogfood case

```python
# consumer/eval/registry.py — caller registers their own evaluators
import tars

EVALUATORS = [
    tars.eval.SchemaComplianceEvaluator(schema=ARC_RESPONSE_SCHEMA),
    tars.eval.RubricGroundingEvaluator(
        json_path="$.findings[*].rule_id",
        allowed=load_known_rule_ids(),
    ),
    tars.eval.FieldFilledRateEvaluator(
        json_path="$.findings[*].evidence",
        name="evidence_filled",
    ),
    tars.eval.LengthEvaluator(field="text", name="response"),
]

# Spawn the runner once at consumer startup:
event_store = tars.SqliteEventStore("./events.db")
runner = tars.eval.OnlineEvaluatorRunner(event_store, EVALUATORS)
asyncio.create_task(runner.run())  # background

# Pipeline is unchanged — it just emits LlmCallFinished, runner consumes:
critic_pipeline = tars.Pipeline.from_default("qwen_coder_local")
                       .with_event_store(event_store)
```

Dashboard query:

```python
# consumer/eval/dashboard.py — read EventStore for trend data
def schema_compliance_last_24h() -> list[(datetime, float)]:
    return event_store.query("""
        SELECT bucket, AVG(value) FROM (
            SELECT
                strftime('%H:%M', ts_ms/1000, 'unixepoch') AS bucket,
                CAST(json_extract(payload, '$.value') AS REAL) AS value
            FROM events
            WHERE event_type = 'EvaluationScored'
              AND json_extract(payload, '$.dim') = 'schema_compliance'
              AND ts_ms > strftime('%s','now','-1 day') * 1000
        ) GROUP BY bucket ORDER BY bucket;
    """)
```

### 9.2 Release-gate case (offline)

```python
# Run a historical dataset to compare prompt v1.2 vs v1.3 reports
# Note: dataset is an EventStore dump, not a hand-labeled set
events_v12 = tars.SqliteEventStore("./snapshot-prompt-v1.2.db")
events_v13 = tars.SqliteEventStore("./snapshot-prompt-v1.3.db")

report_v12 = tars.eval.OfflineEvaluatorRunner(events_v12, EVALUATORS, "v1.2").run()
report_v13 = tars.eval.OfflineEvaluatorRunner(events_v13, EVALUATORS, "v1.3").run()

# Print tabular comparison:
print(report_v12.compare(report_v13).markdown())
```

---

## 10. Dimension naming and cross-semantic conventions

As more evaluators are added, the dimension-name set grows. Conventions to avoid collisions:

### 10.1 Namespace rule

`<evaluator_name>.<sub_dim>` — when one evaluator emits multiple dims, separate with a dot. Examples:
- `rubric.grounded_rate` / `rubric.ad_hoc_rate` / `rubric.hallucinated_rate`
- `length.chars` / `length.tokens`
- `schema_compliance` (single-dim uses the bare name)

### 10.2 Unit conventions

- **Ratios (`*_rate`)**: value ∈ `[0, 1]`, sample_size required
- **Counts (`*_count`)**: value is a non-negative integer, sample_size left as 0
- **Durations (`*_ms`)**: value is in milliseconds
- **Token (`*_tokens`)**: value is a token count
- **Chars (`*_chars`)**: value is a character count

### 10.3 Documentation requirement

Every built-in evaluator's doc-comment must declare:
- Which dims are emitted
- The value range + unit per dim
- The semantics of sample_size

```rust
/// `RubricGroundingEvaluator` outputs three dimensions:
///
/// | dim                       | range | sample_size           |
/// |---------------------------|-------|-----------------------|
/// | rubric.grounded_rate      | [0,1] | total findings        |
/// | rubric.ad_hoc_rate        | [0,1] | total findings        |
/// | rubric.hallucinated_rate  | [0,1] | total findings        |
pub struct RubricGroundingEvaluator { ... }
```

---

## 11. Implementation path

Follows the milestone style from [Doc 14 §9](./14-implementation-path.md). This document maps to **M9 wave 2** — immediately after Doc 15 wave 1 (validation).

### 11.1 Wave breakdown

| Stage | Content | Estimate |
|---|---|---|
| **W2.1** | `Evaluator` / `AsyncEvaluator` traits + `DimensionScore` + `EvalKind` enum + `LlmCallFinished` / `EvaluationScored` events | 1 day |
| **W2.2** | `Pipeline` change: on call completion, `tokio::spawn` writes `LlmCallFinished` to EventStore; constructor accepts `event_store: Option<...>` | 1 day |
| **W2.3** | `OnlineEvaluatorRunner` + `OfflineEvaluatorRunner` impl + concurrency cap | 1.5 days |
| **W2.4** | Built-in evaluators: Schema / RubricGrounding / FieldFilledRate / RegexMatchCount / Length / SnippetGrounding | 1.5 days |
| **W2.5** | tars-py exposure: `tars.eval.Evaluator` base class + 1:1 mapping for built-ins + `Pipeline.with_event_store` API | 1 day |
| **W2.6** | SQL query template module `tars-eval::sql::common_queries` + doc examples | 0.5 day |
| **W2.7** | Unit + integration tests + downstream consumer dogfood evaluator switchover + CHANGELOG | 1 day |
| **Total** | | **~7.5 days** |

### 11.2 Immediate follow-up actions after landing

1. Move the metric portion of downstream consumer's `_known_rule_ids` (demote count) into a `RubricGroundingEvaluator` instance, delete the old inline logic — the validation portion migrates to Doc 15
2. Downstream consumer dogfood dashboards switch from hand-picked numbers → SQL on EventStore
3. Downstream consumer adds an `EvidenceFilledRateEvaluator`, immediately seeing the evidence-field fill rate
4. tars side monitors OnlineEvaluatorRunner lag — the time delta from `LlmCallFinished` write to `EvaluationScored` write; p95 should be < 1s

### 11.3 Not in v1 (pushed to v2)

- LLM-as-judge built-in evaluator — anti-pattern §8.2
- Per-tenant evaluator config — wait for multi-tenant to land (Doc 06 §3)
- Persistent runner checkpoint — at startup, accept duplicate scoring from rescanning
- Metric backend exporters (Prometheus / DataDog) — wait until a real user requests it
- Streaming evaluation (mid-stream token-by-token scoring) — same stance as Doc 15 §13.2: not done
- Explicit dependency graph between evaluators — evaluators do not depend on each other (each runs against the same LlmCallFinished independently)

---

## 12. Cross-doc references

- **Doc 02 Middleware Pipeline** — where the Pipeline's outermost wrapper writes `LlmCallFinished` events
- **Doc 04 Agent Runtime** — where the `AgentEvent` enum gets the `LlmCallFinished` / `EvaluationScored` variants
- **Doc 09 Storage Schema** — `EventStore` trait + SQL schema; the two events added here are persisted per that spec
- **Doc 15 Output Validation** — contrast doc: validation is synchronous, mutates Response, is a gate; evaluation is asynchronous, emits metrics, is a dashboard
- **Doc 14 Implementation Path** — milestone ordering: this document maps to M9 wave 2

---

## 13. Open questions

### 13.1 Consistency across multiple EventStore writes

If a single Pipeline points at multiple EventStores (production + staging), which writes go where?

Verdict: one EventStore per Pipeline is the v1 assumption. Multi-store is a future multi-tenant problem.

### 13.2 Evaluator scoring window

Some dimensions ("ad-hoc rate over the last 100 calls") naturally need windowed computation, not per-call scoring.

Verdict: two paths. (A) evaluator holds sliding window state — breaks the "stateless pure function" contract, rejected. (B) per-call score emits the raw count, the window is computed via group-by on the dashboard / SQL side — cleaner. **Take B**.

### 13.3 Should failed LlmCalls also write events?

`LlmCallFinished` currently includes `response`. On failure (`Err(ProviderError)`) there is no response.

Verdict: write a `LlmCallFailed` sibling variant — failure cases are also evaluation input ("is this retry's error rate high"). But v1 only does the success path; failures are added when an evaluator actually needs that data.

### 13.4 Relationship between evaluator scores and `Response.telemetry`

`Response.telemetry.cache_hit` is already in the LlmCallFinished payload — accessible via `response.telemetry.cache_hit`. Does an evaluator still need a separate "cache_hit_rate" dim?

Verdict: no — direct SQL `AVG(json_extract(...))` does it. Evaluators only emit facts that the evaluator computes; they do not duplicate facts telemetry already wrote.

### 13.5 Relationship to OpenTelemetry metrics

OTel metrics are also observability data. Do EvaluationScored events + OTel metrics double up on data points?

Verdict: EvaluationScored is **semantic dimensions** (rubric_grounded_rate); OTel metrics are **infra dimensions** (http_request_duration_ms). They don't overlap. A future `EvaluationScored → OTel meter` exporter is possible, but the source-of-truth remains EventStore.
