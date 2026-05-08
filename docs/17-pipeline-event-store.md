# Doc 17 — Pipeline Event Store

**Status**: design (2026-05-08). Implementation lives behind W3 enabler;
the present doc is the contract that enabler will materialise.

**Companion**: [Doc 16 — Evaluation Framework](./16-evaluation-framework.md)
defines `Evaluator` / `OnlineEvaluatorRunner` / scoring semantics; this
doc defines the event substrate they consume + write to.

## 1. What it is

A durable, queryable stream of **one event per `Pipeline.call`
boundary** — distinct from the existing trajectory `AgentEvent` stream
which is at agent-decision grain (Doc 04).

Two streams, one trait:

| Stream            | Grain                  | Schema (`tars-types`)         | Crate that emits     |
|-------------------|------------------------|-------------------------------|----------------------|
| Trajectory events | Agent decision         | `AgentEvent`                  | `tars-runtime`       |
| Pipeline events   | One LLM call boundary  | `PipelineEvent`               | `tars-pipeline`      |

Both ride on `EventStore<E>` from `tars-storage`. Independent instances,
no cross-stream join required at the storage layer.

## 2. Use cases

Each is a query that becomes possible *only* with this in place:

- **arc dogfood regression gate**: cohort A (`tags: ["dogfood_2026_05_05"]`)
  vs cohort B — did `validation_summary.outcomes.snippet_grounded.dropped`
  rate change after a model swap?
- **Cross-stream observability**: when a quality metric drops, did
  `telemetry.retry_count` / `cache_hit` rate also change for the same
  call set?
- **Online evaluator** (Doc 16 §4): subscribe to `LlmCallFinished`,
  run an evaluator (cheap deterministic or expensive LLM-as-judge),
  emit `EvaluationScored` for downstream rollups.
- **Offline evaluator / replay** (Doc 16 §5): pull last N call bodies,
  re-issue against a new prompt or model, compare scores.
- **Compliance audit** (M6+): "all LLM calls for tenant X in last 30
  days" — `WHERE tenant_id = ? AND timestamp > ?`.
- **Dataset bootstrap** (B-28): export `(request_body, response_body)`
  for cohort, build SFT training data.

The first three drive W3; the rest are downstream multipliers.

## 3. Event boundary rule

**One event per `Pipeline.call`. Token deltas, retries, and cache
lookups are sub-call concerns — they appear inside the event's
`telemetry` field, not as separate events.**

| Phenomenon                            | Emits event? | Notes |
|---------------------------------------|--------------|-------|
| `Pipeline.complete()` succeeds        | ✅ 1         | call boundary |
| `Pipeline.complete()` fails           | ✅ 1         | error patterns are evaluator input |
| Retry succeeds after 3 attempts       | ✅ 1         | `telemetry.retry_count = 3` |
| Cache hit                             | ✅ 1         | `telemetry.cache_hit = true` |
| Stream `ChatEvent::Delta` token       | ❌           | UX concern, not analytics |
| Session tool-loop, N inner LLM calls  | ✅ N         | each `Pipeline.call` is its own event |
| `Session::send` boundary              | ❌           | trajectory's job, not pipeline's |
| Evaluator produces a score            | ✅ 1 (different variant) | `EvaluationScored`, FK to `LlmCallFinished` |

Invariant: consumer questions are call-grained ("did *this call*
succeed / cost / hallucinate"), so cardinality matches.

## 4. Schema

Lives in `tars-types/src/pipeline_events.rs` (data contract, no
backend dependency).

```rust
#[non_exhaustive]
pub enum PipelineEvent {
    LlmCallFinished(LlmCallFinished),
    EvaluationScored(EvaluationScored),
    /// Catchall for forward-compat — old readers deserialise unknown
    /// variants into `Other` instead of failing the whole record.
    /// Borrowed from codex-rs `ResponseItem::Other` pattern (5 years
    /// of versionless schema evolution rests on this).
    Other(serde_json::Value),
}

pub struct LlmCallFinished {
    // identity
    pub event_id: Uuid,
    pub timestamp: SystemTime,
    pub tenant_id: TenantId,
    pub session_id: Option<SessionId>,    // None = direct Pipeline.complete
    pub trace_id: Option<TraceId>,         // for B-21 OTel correlation

    // request — inline scalars (filter / group-by maps to these)
    pub provider_id: ProviderId,
    pub actual_model: String,              // routing-resolved, not ModelHint
    pub request_fingerprint: [u8; 32],     // tenant-agnostic semantic hash
    pub has_tools: bool,
    pub has_thinking: bool,
    pub has_structured_output: bool,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,

    // bodies — out-of-row via tenant-scoped ContentRef
    pub request_ref: ContentRef,
    pub response_ref: Option<ContentRef>,  // None on error

    // observability — already collected by middleware, snapshot here
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,
    pub telemetry: TelemetrySummary,
    pub validation_summary: ValidationSummary,

    // outcome
    pub result: CallResult,                // Ok | Error{kind}

    // cohort (LangSmith borrow — see B-20 W1.1 borrow points)
    pub tags: Vec<String>,
}

pub struct EvaluationScored {
    pub event_id: Uuid,
    pub timestamp: SystemTime,
    pub tenant_id: TenantId,
    pub call_event_id: Uuid,               // FK to LlmCallFinished
    pub evaluator_name: String,
    pub score: f64,
    pub explanation: Option<String>,
    pub tags: Vec<String>,
}
```

`#[non_exhaustive]` keeps room for future variants
(`ToolCallFinished`, `SessionClosed`, etc.) without SemVer break.

## 5. Inline vs ContentRef

| Layer | What                                                      | Why |
|-------|-----------------------------------------------------------|-----|
| Inline scalars in event row | model, provider, fingerprint, has_tools, temperature, telemetry, validation_summary, tags | Filtered by 99% of queries; full-row scans on tagged dashboards must not pay body-fetch latency |
| `ContentRef` (out-of-row)   | system prompt, messages (full), tool specs, response body, response_schema | Bytes (KB-MB scale); evaluator replay path fetches on demand; storage / TTL tunable independently |
| Not stored                  | per-token `ChatEvent::Delta`, HTTP wire bytes, intermediate retry response bodies | Cardinality / cost / privacy not justified by use cases |

## 6. Tenant isolation (`ContentRef` shape)

Lives in `tars-types/src/content_ref.rs`:

```rust
pub struct ContentRef {
    tenant_id: TenantId,
    body_hash: [u8; 32],
}
```

Self-contained — `BodyStore::fetch(&ContentRef)` enforces tenant
scoping internally; no caller-provided `tenant_id` parameter, no
foot-gun, no probe vector.

**Cross-tenant body dedup is forbidden** — Doc 06 §1 (tenant isolation
sacred) trumps any storage-saving argument. Same body bytes from two
tenants get two distinct `ContentRef` (different `tenant_id` prefixes
in store key). Within-tenant dedup is fine and gives most of the
storage benefit anyway.

`request_fingerprint` (analytics hash on the event row) is
tenant-agnostic on purpose — it's a 32-byte hash with no body
recoverable, used for "this prompt template appeared 10000 times
across tenants" rollups. Fingerprint ≠ body pointer.

## 6.1 BodyStore physical layout (codex-rs borrow)

`BodyStore` is a trait. v1 impl is single-table SQLite; trait shape
keeps room for codex-rs's date-partitioned strategy as v2:

```rust
#[async_trait]
pub trait BodyStore: Send + Sync {
    async fn put(&self, r: &ContentRef, bytes: Bytes) -> Result<()>;
    async fn fetch(&self, r: &ContentRef) -> Result<Bytes>;

    /// Drop all bodies older than `cutoff`. Implementations CAN do
    /// this efficiently (codex-style YYYY/MM/DD dirs → `rm -rf`)
    /// or with `DELETE WHERE created_at < ?`. The trait commits to
    /// the operation existing, not to its cost.
    async fn purge_before(&self, cutoff: SystemTime) -> Result<u64>;

    /// Drop a tenant's entire body footprint — required for
    /// tenant-delete compliance. Implementations MUST partition by
    /// tenant_id internally so this is O(tenant) not O(all bodies).
    async fn purge_tenant(&self, tenant_id: &TenantId) -> Result<u64>;
}
```

The `purge_*` methods exist in the trait so v2 backends (date-partitioned
sqlite-per-day, S3 with lifecycle rules, postgres bytea with index)
can implement retention as physical operations rather than full-table
scans. Codex's `~/.codex/sessions/YYYY/MM/DD/...` shows the value:
"delete all bodies older than 7d" becomes `rmdir` instead of a
multi-million-row DELETE.

v1 impl: single SQLite table with `(tenant_id, body_hash)` PK,
`created_at` index. `purge_before` runs `DELETE WHERE created_at < ?`.
Acceptable until body store hits ~100M rows; v2 partitioning kicks
in by then.

## 7. Where things live (crate placement)

```
tars-types/                       ← data contracts (no backends)
├── pipeline_events.rs            ← PipelineEvent / LlmCallFinished / EvaluationScored / CallResult
├── content_ref.rs                ← ContentRef
└── ... (existing AgentEvent, ChatRequest, etc.)

tars-storage/                     ← backends + traits needing them
├── event_store.rs                ← EventStore<E> trait + SqliteEventStore (existing — generalise)
├── body_store.rs                 ← NEW: BodyStore trait + SqliteBodyStore impl
└── kv_store.rs                   ← (B-7, deferred)

tars-pipeline/                    ← middleware / business logic
└── event_emitter.rs              ← NEW: EventEmitterMiddleware
```

Dependency direction: `tars-pipeline → tars-storage → tars-types`.
`tars-types` has zero downstream dependencies. Backend swap doesn't
touch the schema or anything that imports it.

## 8. Emit semantics

- **Position in onion**: outermost middleware, before Telemetry (or
  fold into Telemetry — see open question below).
- **Trigger**: after `Pipeline.call` returns (Ok or Err). Fire-and-forget
  write to `EventStore<PipelineEvent>` and `BodyStore`. Write failures
  degrade silently with a warn log (same pattern as
  `cache.rs` write fire-and-forget).
- **Sampling**: full emit by default. `EventSampler` trait point left
  open (`AlwaysEmit` impl ships); per-tag rate limiting belongs to
  `OnlineEvaluatorRunner`'s scheduling, not the event-emit path —
  metric rollups need full samples.

### 8.1 PersistenceMode (codex-rs borrow)

Two modes, per-tenant configurable, default `Limited`:

```rust
pub enum PersistenceMode {
    /// Default. Inline scalars + ContentRef bodies. Sufficient for
    /// metric rollups, cohort filtering, regression gates.
    Limited,

    /// Extended debug detail: per-attempt retry payloads, raw stream
    /// timing, intermediate tool-call args/results. Storage cost
    /// ~5-10x Limited. Tenant opts in for debugging windows.
    Extended,
}
```

`PersistenceMode` is **about what's persisted, not how much** — distinct
from sampling. Sampling decides "do we emit this call's event at all";
mode decides "if we emit, how much detail goes in." Both compose:
default tenant gets `Always` × `Limited`; debug-window tenant gets
`Always` × `Extended`; high-QPS prod gets `Rate(0.01)` × `Limited`.

Borrowed from codex-rs `EventPersistenceMode::{Limited, Extended}`
(`recorder.rs` policy module). Same intuition: most consumers want a
small dial, "everything OR essentials"; finer field-level control is
overkill for v1.

## 9. Roadmap (phased)

**Phase 1 — Enabler** (target: this week, blocks W3 main body):
1. `ContentRef` struct in `tars-types`.
2. `PipelineEvent` enum (`LlmCallFinished` / `EvaluationScored` /
   `Other` catchall) + supporting structs (`CallResult`,
   `PersistenceMode`) in `tars-types`. `EvaluationScored` defined but
   not yet emitted (Phase 2).
3. `BodyStore` trait + `SqliteBodyStore` impl in `tars-storage`.
4. `PipelineEventStore` trait + `SqlitePipelineEventStore` impl in
   `tars-storage`. **Existing `EventStore` (trajectory) stays
   unchanged** — Q1 decided two independent traits, not generic.
   Internal `SqliteEventStoreCore` may share scaffolding between the
   two impls.
5. `EventEmitterMiddleware` in `tars-pipeline`. Emits
   `LlmCallFinished` post-call. Outermost layer (before Telemetry).
6. `Pipeline.with_event_store(...)` builder method on
   `tars-py::Pipeline` and the Rust builder.
7. Integration test: drive `Pipeline.call`, assert event landed in
   `SqlitePipelineEventStore` with expected fields + body fetchable
   from `SqliteBodyStore`.

**Phase 2 — Subscription / OnlineEvaluatorRunner** (W3 main body):
8. `EventStore::subscribe()` API → stream of `PipelineEvent`.
9. `OnlineEvaluatorRunner` consumes `LlmCallFinished`, dispatches to
   `Evaluator` impls, emits `EvaluationScored`.
10. `EventSampler` trait + `AlwaysEmit` / `Rate(f64)` /
    `OnDimDrop{watch_dim, threshold}` impls (per Doc 16 §4 LangSmith
    borrow).

**Phase 3 — Offline / replay tooling** (post-W3):
11. CLI: `tars event query <filters>` for ad-hoc inspection.
12. CLI: `tars event replay <event_id> [--with-prompt-rewrite ...]`
    for evaluator iteration.

**Phase 4 — Backend extensibility** (driven by deployment shape):
13. `PostgresEventStore`, `S3BodyStore`, retention policies.

## 10. Open questions

Tagged with the decision needed.

| # | Question | Default if undecided |
|---|----------|----------------------|
| Q1 | Is `EventStore<E>` actually generic, or two independent traits? | **Decided 2026-05-08: two independent traits**. Existing `EventStore` (trajectory, keyed by `TrajectoryId`) stays unchanged. New `PipelineEventStore` trait with `query(time_range, tenant, tags)` / `subscribe()` / `purge_before(cutoff)` / `purge_tenant(id)`. Underlying SQLite scaffolding shared via internal `SqliteEventStoreCore`. Reasons: existing trait is deliberately `dyn`-friendly + JSON-at-boundary, generic over E breaks the first; access patterns are too divergent for one trait (only `append` would actually be shared). |
| Q2 | Place `EventEmitterMiddleware` outside Telemetry, or fold into it? | **Decided 2026-05-08: separate layer**. Single responsibility; lifecycle differs — telemetry is in-mem accumulator dropped at call end, event is durable async write. |
| Q3 | `request_fingerprint` algorithm — what's in the canonicalised body? | model + messages + tools (sorted) + temperature + max_tokens + response_schema. **Not** in fingerprint: tenant_id, IAM scopes, request_id, trace_id (those go in cache key but are tenant-scoping concerns not semantic ones). |
| Q4 | `EvaluationScored.score: f64` flat, or richer typed `Score { dim, value }`? | Flat `f64` for v1; `dim: String` already separable via `evaluator_name`. Punt typed scoring until a 2-dim evaluator ships. |
| Q5 | `ContentRef` body store + cache-registry body store: same physical store or separate? | Same. Both are "tenant-scoped CAS by sha256(tenant + body)"; deduping the implementation removes a class of "which one am I writing to" bugs. |
| Q6 | Failed-call event has `response_ref: None` — does it also store the `ProviderError` chain detail? | `result: CallResult::Error { kind }` + `telemetry.retry_attempts` cover the patterns evaluators want. Full error chain only in the per-attempt log line, not the event. |
| Q7 | TTL defaults — 90d events / 7d bodies — per-tenant override at what layer? | `tars-config` `[tenant.{id}.retention]` block; absent = global default. Implementation deferred to M6 multi-tenant. |
| Q8 | Schema versioning — bump `LlmCallFinished` adding a field — back-compat? | `#[serde(default)]` on every new field, `#[non_exhaustive]` enum, **plus `Other(serde_json::Value)` catchall variant** (codex-rs borrow — old readers don't fail on unknown variants). No explicit version field until a breaking change forces one. |
| Q9 | `BodyStore` v1 physical layout — single SQLite table, date-partitioned, or pluggable? | Pluggable trait. v1 impl is single-table SQLite (simplest); trait commits to `purge_before` / `purge_tenant` ops so v2 (codex-style date-partitioned sqlite-per-day or S3 + lifecycle rules) can replace without consumer changes. |

Most defaults are safe to ship as Phase 1 lands; Q1 and Q2 are the
two needing explicit sign-off before enabler implementation starts.
