# Doc 08 — MELT (Metrics / Events / Logs / Traces) Observability Design

> Scope: defines the observability data architecture of the Runtime — responsibility split, collection, storage, querying, and alerting across the four pillars (M/E/L/T).
>
> Context: this doc, Doc 04 §3.2 `AgentEvent` (event sourcing), and Doc 06 §10 `AuditLog` (compliance audit) are **three distinct data flows**; see §3 for disambiguation.
>
> Cross-cutting: every Doc 01-07 component emits data into the MELT system. This doc defines "what to emit / how to emit / where it lives / how to query".

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Clear pillar responsibilities** | M/E/L/T each have well-defined semantics and don't mix — picking the wrong pillar leads to runaway cost or unanswerable queries |
| **LLM cost as a first-class metric** | token / dollar are core metrics, on par with latency / errors |
| **Tenant-level isolation** | every signal carries a tenant_id label, allows per-tenant slicing but no cross-tenant bleed |
| **Zero payload leakage** | prompt / response / code content **never enter MELT**; only redacted metadata flows through |
| **OTel standardization** | full-stack OpenTelemetry, allows plugging into any OTLP-compliant backend |
| **Tunable sampling** | LLM calls are expensive; sampling traces at 100% blows up; sample dynamically by tenant / error status / cost |
| **Bounded cardinality** | label dimensions have hard caps to prevent user_id / trace_id from leaking into metric labels and blowing up Prometheus |
| **Deployment-form degradation** | Personal mode disables telemetry by default; Team mode enables everything; Hybrid mode emits only anonymized metrics |

**Anti-goals**:
- Don't conflate audit log (Doc 06 §10) with MELT — audit is a compliance requirement, MELT is an ops need
- Don't treat AgentEvent (Doc 04 §3.2) as the events pillar of MELT — AgentEvent is event sourcing, the business truth, and cannot be downsampled for ops
- Don't persist PII / code content inside MELT — telemetry backends may be accessed without authorization, contaminating the source
- Don't make ops data depend on business storage (Postgres) — when business is down, you still need to see "business is down"

---

## 2. Why MELT Instead of "Logs"

Stuffing every ops signal into logs is the previous-generation (pre-2015) anti-pattern:
- Want "error rate over the last 5 minutes"? grep + awk a few GB of logs
- Want "what services did this request pass through"? text-search trace_id and stitch it together
- Want "which tenant is most expensive"? write yet another aggregation script

Each kind of question has a more appropriate data structure:

| Question type | Best pillar | Data structure |
|---|---|---|
| "Trend of XX over the last 5 minutes" | Metrics | Time-series aggregation (counter / gauge / histogram) |
| "Something meaningful just happened" | Events | Discrete structured records (type + fields) |
| "How exactly was this request handled" | Logs | Time-series text + structured fields |
| "Which components did the request traverse" | Traces | Causal tree (span tree) |

The four pillars are **complementary**, not substitutes. A single LLM call should produce a Metric (counted into total tokens), an Event ("call complete"), a Log (detailed context), and a Trace (linked to its parent span), each serving different query needs.

---

## 3. Disambiguating the Three Data Flows

```
┌─────────────────────────────────────────────────────────────────┐
│  Runtime (Doc 04)                                               │
│                                                                 │
│  Event Sourcing                Audit Log              MELT      │
│  (AgentEvent)                  (AuditEvent)           (M/E/L/T) │
│  Doc 04 §3.2                   Doc 06 §10             this doc  │
│  ↓                             ↓                      ↓         │
│  Postgres event_log            WORM / SIEM            OTel      │
│                                                                 │
│  purpose: replay/recovery      purpose: compliance    purpose: ops │
│  guarantee: integrity (never lose) guarantee: tamper-proof  guarantee: good enough │
│  retention: 30d hot + 1y cold  retention: 7y          retention: 30d │
│  sampling: 100% (business truth) sampling: 100% (compliance) sampling: dynamic │
└─────────────────────────────────────────────────────────────────┘
```

| Dimension | AgentEvent | AuditEvent | MELT |
|---|---|---|---|
| Who reads | Runtime itself (recovery) | Legal / compliance / regulators | SRE / dev / product |
| Cost of dropping one | Task cannot replay, data inconsistent | Compliance violation | Inaccurate monitoring, acceptable |
| Data fidelity | 100% (full payload via ContentRef) | Signed key events | Metadata + summary |
| Downsamplable | ❌ | ❌ | ✅ |
| Write path | Sync (business path) | Async dual-write (business-critical) | Async, best-effort |
| Failure handling | Abort business | Block business + alert | Drop, alert but don't impact business |

**Key invariants**:
- AgentEvent persistence fails → trajectory fails (Doc 04 cannot continue)
- AuditEvent persistence fails → business blocks (Doc 06 compliance floor)
- MELT persistence fails → business continues, self-alerts

Don't merge the three into one storage for the sake of "unification" — failure modes are completely different.

---

## 4. Unique Challenges of LLM Workloads

Traditional microservice observability → the LLM era introduces several new dimensions:

| Challenge | Traditional system | LLM system |
|---|---|---|
| Per-request cost | Microseconds of CPU, negligible | Cents to dollars, must be tracked |
| Per-request duration | <100ms is the norm | Several seconds to several minutes is normal |
| Failure modes | 4xx / 5xx HTTP | + content filter / truncation / hallucination / wrong tool selection |
| Data sensitivity | Specific business fields need redaction | prompt / response are sensitive in their entirety |
| Retry cost | Essentially free | Each retry costs again |
| Streaming | Rare | Default |

New must-collect metrics:
- **Token usage** (input / output / cached)
- **Cost USD** (computed by model pricing)
- **TTFT** (Time To First Token)
- **Throughput** (Tokens / second)
- **Cache hit rate** (L1 / L2 / L3)
- **Stop reason distribution** (EndTurn / MaxTokens / ContentFilter / ToolUse)
- **Tool call success rate** (per tool)
- **Trajectory branch count** (replan count)
- **Compensation success rate** (rollback success rate)

---

## 5. Metrics Design

### 5.1 Type Taxonomy

```rust
pub enum MetricType {
    Counter,            // monotonically increasing (request total / errors total)
    UpDownCounter,      // can go up and down (active sessions / inflight requests)
    Gauge,              // instantaneous value (cache size / queue depth)
    Histogram,          // distribution (latency / token usage)
    Summary,            // client-side aggregated quantiles (rarely used, prefer Histogram)
}
```

The core abstractions of the OTel metrics SDK; every metric must explicitly declare its type.

### 5.2 Naming Convention

```
{domain}.{component}.{measure_name}
```

Examples:
- `llm.provider.request_total` — Counter
- `llm.provider.ttft_ms` — Histogram
- `llm.provider.tokens_input` — Counter
- `llm.provider.tokens_output` — Counter
- `llm.provider.cost_usd` — Counter
- `llm.cache.hits_total` — Counter (label: level=l1/l2/l3)
- `llm.cache.lookup_latency_ms` — Histogram
- `llm.cache.l3_storage_bytes` — Gauge
- `agent.trajectory.active` — UpDownCounter
- `agent.trajectory.completed_total` — Counter (label: status=success/failed)
- `agent.backtrack_total` — Counter (label: reason=critic_reject/error/budget)
- `tool.invocation_total` — Counter
- `tool.invocation_latency_ms` — Histogram
- `pipeline.layer.latency_ms` — Histogram (label: layer=auth/iam/...)
- `runtime.event_log.write_lag_ms` — Histogram

### 5.3 Mandatory SLIs

Every SLO must have a corresponding metric. Minimum set:

| SLI | Metric | Example SLO |
|---|---|---|
| Availability | `llm.provider.request_total` (success/total) | 99.5% |
| TTFT P95 | `llm.provider.ttft_ms` | < 2000ms |
| Full-request P95 | `llm.provider.total_latency_ms` | < 30000ms |
| Error rate | `llm.provider.errors_total` / `request_total` | < 1% |
| Cache hit rate | `llm.cache.hits_total` / lookups | > 30% |
| Tool success rate | `tool.invocation_total` (success/total) | > 99% |
| Trajectory success rate | `agent.trajectory.completed_total{status=success}` / total | > 95% |
| Budget overrun rate | `budget.hard_limit_exceeded_total` / requests | < 0.1% |

### 5.4 Special Handling for LLM Cost Metrics

Cost is dual-dimensional — both a metric and billing data:

```rust
pub struct CostMetric {
    pub provider: ProviderId,
    pub model: String,
    pub tenant: TenantId,
    pub usage: Usage,
    pub cost_usd: f64,
    pub cache_savings_usd: f64,         // how much was saved versus "without cache"
}
```

Write paths:
1. **Metrics**: `llm.cost_usd` Counter, labels include provider / model / tenant
2. **Billing**: Postgres `billing_events` table (Doc 06 §9.2), one independent record per call, auditable
3. **Not in logs**: cost data is sufficient via metric; logs don't need to repeat it

### 5.5 Cardinality Control

Hard constraint of Prometheus / OTel: the number of unique (name + labels) combinations per metric = "number of time series". One series uses ~3KB of memory; 100k series is 300MB.

Fields **absolutely forbidden** from metric labels:

| Field | Reason |
|---|---|
| `trace_id` | UUID globally unique, will explode |
| `request_id` | same as above |
| `user_id` | explodes with enough users |
| `session_id` | short-lived but grows fast |
| `prompt_hash` | hash space is huge |
| `error_message` | free text, infinite variants |
| `code_path` | thousands of paths in a large codebase |

Fields **allowed** as labels (cardinality is constant-class):

| Field | Typical cardinality |
|---|---|
| `tenant_id` | 10²-10³ |
| `provider` | < 10 |
| `model` | < 50 (aggregate by family, don't put specific versions) |
| `model_tier` | < 10 |
| `tool_id` | < 100 |
| `agent_role` | < 20 |
| `error_class` | 5-10 (Permanent/Retriable/...) |
| `status` | < 10 |
| `cache_level` | 3 (l1/l2/l3) |
| `region` | < 20 |

Validation:

```rust
pub struct LabelValidator {
    allowed_labels: HashSet<String>,
    cardinality_limits: HashMap<String, u32>,
}

impl LabelValidator {
    pub fn validate<L: Labels>(&self, labels: &L) -> Result<(), CardinalityError> {
        for (k, v) in labels.iter() {
            if !self.allowed_labels.contains(k) {
                return Err(CardinalityError::DisallowedLabel(k.into()));
            }
            // Runtime check: if a label's actual value count exceeds the threshold, reject
            if self.observed_cardinality(k) > self.cardinality_limits[k] {
                return Err(CardinalityError::CardinalityExceeded { label: k.into() });
            }
        }
        Ok(())
    }
}
```

At startup, wrap the Metric registry; at runtime, violating metric calls panic outright (dev) / silently drop + alert (prod).

### 5.6 Histogram Bucket Choice

LLM latency spans 3 orders of magnitude (10ms to 100s); fixed buckets aren't enough. Recommend exponential buckets:

```rust
// TTFT bucket (ms): 50, 100, 200, 500, 1000, 2000, 5000, 10000, 30000
const TTFT_BUCKETS: &[f64] = &[50.0, 100.0, 200.0, 500.0, 1000.0, 
                                2000.0, 5000.0, 10000.0, 30000.0];

// Total latency bucket (ms): 100, 500, 1000, 5000, 15000, 60000, 300000
const TOTAL_BUCKETS: &[f64] = &[100.0, 500.0, 1000.0, 5000.0, 
                                 15000.0, 60000.0, 300000.0];

// Token count bucket: 10, 100, 1000, 10000, 100000, 1000000
const TOKEN_BUCKETS: &[f64] = &[10.0, 100.0, 1000.0, 10000.0, 
                                 100000.0, 1000000.0];

// Cost bucket (USD): 0.0001, 0.001, 0.01, 0.1, 1, 10
const COST_BUCKETS: &[f64] = &[0.0001, 0.001, 0.01, 0.1, 1.0, 10.0];
```

---

## 6. Events Design

**Important**: "Events" in this section is not Doc 04 §3.2's `AgentEvent` (event sourcing), but the discrete business events that form one of the four MELT pillars — emitted to OTel Logs (event type) or a standalone event bus, for SRE / product analysis.

### 6.1 When to Use Event Instead of Metric

| Scenario | Use Metric | Use Event |
|---|---|---|
| "How many X in the past hour" | ✅ | ❌ |
| "Latency distribution of X" | ✅ | ❌ |
| "What exactly happened in that one X" | ❌ | ✅ |
| "Which abnormal tenants today" | ❌ | ✅ |
| "Special events triggered N times last week" | ❌ | ✅ |

Key properties of an Event: **discrete + structured + enumerable** — a finite predefined set of types, each with fixed fields.

### 6.2 Mandatory Event Types

```rust
pub enum TelemetryEvent {
    // Key business events (different from AuditEvent — these don't need legal-grade retention)
    HighCostRequest { cost_usd: f64, threshold: f64, model: String, tenant: TenantId },
    UnusualLatency { latency_ms: u64, p99_baseline_ms: u64, provider: ProviderId },
    CircuitBreakerOpened { provider: ProviderId, failure_rate: f64 },
    CircuitBreakerClosed { provider: ProviderId },
    BudgetSoftLimitHit { tenant: TenantId, period: String, percent_used: f64 },
    
    // Cache events
    CacheStorageQuotaWarning { tenant: TenantId, percent_used: f64 },
    L3CacheCreated { handle_id: L3HandleId, size_bytes: u64, tenant: TenantId },
    L3CacheEvicted { handle_id: L3HandleId, reason: EvictionReason },
    
    // Agent events
    BacktrackTriggered { trajectory: TrajectoryId, reason: BacktrackReason },
    CompensationFailed { trajectory: TrajectoryId, compensation_id: CompensationId },
    HumanEscalationRequired { trajectory: TrajectoryId, reason: String },
    
    // Security events (overlap with AuditEvent but finer-grained)
    PromptInjectionDetected { detector: String, tenant: TenantId },
    UnusualToolPattern { tool: ToolId, count_per_minute: u32, tenant: TenantId },
    
    // Configuration events
    ConfigReloadCompleted { changes_count: u32 },
    SecretRotationCompleted { ref_count: u32 },
}
```

### 6.3 Distinction from AgentEvent

| AgentEvent (Doc 04) | TelemetryEvent (this doc) |
|---|---|
| Business truth, required for replay | Ops snapshot, loss is acceptable |
| 100% collected | Downsamplable |
| Written to Postgres event_log | Written to OTel logs (event flag) / event bus |
| Every step of every trajectory | Across business, the moments ops cares about |
| Fields are business inputs/outputs | Fields are ops metadata |

**Relationship between the two**: some AgentEvents **derive** TelemetryEvents. For example, `AgentEvent::TrajectoryAbandoned` may derive `TelemetryEvent::BacktrackTriggered` (if reason is critic reject) and `MetricUpdate("agent.backtrack_total")`. Derivation is performed by a dedicated `TelemetryProjector`.

```rust
pub trait TelemetryProjector: Send + Sync {
    fn project(&self, agent_event: &AgentEvent) -> TelemetryProjection;
}

pub struct TelemetryProjection {
    pub events: Vec<TelemetryEvent>,
    pub metric_updates: Vec<MetricUpdate>,
    pub log_lines: Vec<LogLine>,
}
```

`TelemetryProjector` runs synchronously (fast path) when the Runtime commits an AgentEvent; the projection result is dispatched asynchronously (slow path doesn't block business).

---

## 7. Logs Design

### 7.1 Structured Only

Never permit `println!("user x did y, with z")`-style string-concatenated logs.

```rust
// ❌ wrong
tracing::info!("User {} fetched {} bytes from {}", user_id, size, provider);

// ✅ correct
tracing::info!(
    user.id = %user_id,
    payload.size_bytes = size,
    provider = %provider,
    "fetch_completed"
);
```

Reason: grep `"fetch_completed"` will always find every such record; with string concatenation, variants are endless.

### 7.2 Level Conventions

| Level | When to use | Frequency |
|---|---|---|
| ERROR | Business failure / unrecoverable | Rare (single-digit per minute in prod) |
| WARN | Recoverable but worth noting (retry / fallback) | Occasional (tens per minute) |
| INFO | Key business milestones (request done / config loaded) | Common (a few per second) |
| DEBUG | Detailed internal state | Dev / temporary debugging only |
| TRACE | Extremely fine-grained (function-level) | Dev / profiling only |

Production defaults to INFO; DEBUG is not allowed by default (log volume explosion + sensitive data risk).

### 7.3 Mandatory Fields

Every log auto-injects:

```rust
// via tracing::Span's record mechanism
#[instrument(
    fields(
        trace_id = %ctx.trace_id,
        tenant = %ctx.tenant_id,
        session = %ctx.session_id,
        principal = %ctx.principal,
    )
)]
async fn handle_request(ctx: RequestContext, ...) -> ... {
    // every log inside this function automatically carries the above fields
}
```

No need to repeat `tenant=...` on every log — inherited via tracing span context.

### 7.4 Sensitive Data Redaction

Content that absolutely must not enter logs:

| Content | Replacement |
|---|---|
| Full prompt | log `prompt.hash`, `prompt.token_count`, `prompt.system.role` |
| LLM raw response | log `response.token_count`, `response.stop_reason`, `response.has_tool_calls` |
| User code | only log file path + line number, never content |
| API key / secret | never log (don't log even after redaction) |
| Email / phone | hash before logging, or don't log |
| Tool call's specific arguments | log `tool.id`, `args.size_bytes`, `args.field_count`, never the values |

Implementation: wrap with a `tracing` field formatter:

```rust
pub struct SensitiveString(String);

impl fmt::Display for SensitiveString {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<redacted:{}>", self.0.len())
    }
}

// usage
tracing::info!(
    user.email = %SensitiveString(email),  // logs out "<redacted:32>"
    ...
);
```

### 7.5 Log Aggregation

```rust
// startup configuration
tracing_subscriber::registry()
    .with(EnvFilter::from_default_env())
    .with(tracing_subscriber::fmt::layer().json())   // JSON format
    .with(OpenTelemetryLayer::new(otel_tracer))      // simultaneously emit OTel
    .init();
```

JSON format ensures logs can be correctly parsed by Loki / Elasticsearch / Datadog.

---

## 8. Traces Design

### 8.1 Span Design Principles

- **Coarse-grained first**: a top-level span per request + a few key sub-spans, don't add a span per function
- **Cross-process propagation is mandatory**: between CLI subprocess, MCP server, OTel collector, propagate trace_id via OTel context
- **Failures must record_error**: `span.record_error(&e)` lets traces filter erroring requests

### 8.2 Span Tree Shape

```
LLM Request                          (root span, in axum handler)
├─ middleware.telemetry              (Doc 02 §4.1)
├─ middleware.auth
├─ middleware.iam
├─ middleware.budget.check
├─ middleware.cache.lookup
│  ├─ cache.l1.get
│  └─ cache.l2.get
├─ middleware.prompt_guard
│  ├─ guard.fast_lane
│  └─ guard.slow_lane                (parallel, as sibling spans)
├─ middleware.routing
├─ middleware.circuit_breaker.check
├─ middleware.retry
│  └─ provider.openai.stream         (actual LLM call)
│     ├─ http.request                (reqwest auto-instrumented)
│     └─ sse.parse
├─ middleware.cache.write            (outbound, async)
├─ middleware.budget.commit
└─ middleware.telemetry.finalize
```

Each span must have:
- `name`: as shown above
- `kind`: Internal / Client / Server
- `attributes`: business identifiers (tenant / provider / model)
- `events`: key moments (cache hit / retry / circuit open)

### 8.3 Trajectory Tree → Span Tree

How Doc 04's trajectory tree appears in a trace:

```
task.run                             (root)
├─ agent.orchestrator.execute
│  └─ llm.invoke (model=fast)
├─ agent.worker.security.execute     (parallel sibling)
│  ├─ tool.invoke (id=git.fetch_pr_diff)
│  ├─ tool.invoke (id=sast.run_semgrep)
│  └─ llm.invoke (model=reasoning)
├─ agent.worker.perf.execute         (parallel sibling)
│  └─ llm.invoke (model=reasoning)
├─ agent.aggregator.execute
└─ agent.critic.execute
   └─ llm.invoke (model=default)
```

When backtrack is triggered by critic reject, the new trajectory is a new span tree (linked via `link` to the parent trajectory's root span, not a child span).

### 8.4 Sampling

LLM calls routinely take seconds to tens of seconds; sampling all traces blows up the collector. Layered sampling:

```rust
pub enum SamplingDecision {
    AlwaysSample,          // critical events: errors / high cost / security alerts
    PerTenant { rate: f64 },  // per-tenant base rate
    HeadBased { rate: f64 },   // ingress decision
    TailBased,                 // decide after collecting the entire trace (for cost-based sampling)
}

pub struct AdaptiveSampler {
    base_rate: f64,                           // default 0.1 (10%)
    always_sample_predicates: Vec<Predicate>,
}

impl Sampler for AdaptiveSampler {
    fn should_sample(&self, ctx: &SamplingContext) -> SamplingDecision {
        // always-sample cases
        if ctx.has_error() { return AlwaysSample; }
        if ctx.cost_usd > 1.0 { return AlwaysSample; }
        if ctx.tenant_priority == Priority::Premium { return AlwaysSample; }
        if ctx.is_security_event() { return AlwaysSample; }
        
        // default random
        if rand::random::<f64>() < self.base_rate {
            AlwaysSample
        } else {
            SamplingDecision::Drop
        }
    }
}
```

### 8.5 Tail-based Sampling

Some scenarios require seeing the whole trace before deciding to keep it:

- "Find all requests that ultimately failed but went through retries"
- "Find P99 slow-request samples"
- "Find requests that consumed more than $0.50"

Implementation: configure `tailsamplingprocessor` in the OTel Collector:

```yaml
processors:
  tail_sampling:
    decision_wait: 30s              # wait for trace to complete
    policies:
      - name: errors
        type: status_code
        status_code: { status_codes: [ERROR] }
      - name: slow
        type: latency
        latency: { threshold_ms: 10000 }
      - name: expensive
        type: numeric_attribute
        numeric_attribute: { key: cost_usd, min_value: 0.5 }
      - name: random
        type: probabilistic
        probabilistic: { sampling_percentage: 5 }
```

---

## 9. OpenTelemetry Integration

### 9.1 Full-stack OTel

All metrics / events / logs / traces go through the OTel SDK:

```rust
// Cargo.toml
[dependencies]
opentelemetry = "0.x"
opentelemetry_sdk = "0.x"
opentelemetry-otlp = "0.x"
opentelemetry-prometheus = "0.x"  # optional, Prometheus scrape
tracing-opentelemetry = "0.x"
```

```rust
// startup initialization
pub fn init_telemetry(config: &TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let resource = Resource::new(vec![
        KeyValue::new("service.name", "tars"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new("deployment.environment", config.environment.clone()),
        KeyValue::new("host.id", config.node_id.clone()),
    ]);
    
    // Traces
    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(otel_exporter(&config.otlp_endpoint))
        .with_trace_config(
            trace::config()
                .with_sampler(AdaptiveSampler::new(config.sampling_rate))
                .with_resource(resource.clone()),
        )
        .install_batch(runtime::Tokio)?;
    
    // Metrics
    let meter_provider = opentelemetry_otlp::new_pipeline()
        .metrics(runtime::Tokio)
        .with_exporter(otel_exporter(&config.otlp_endpoint))
        .with_resource(resource.clone())
        .with_period(Duration::from_secs(10))
        .build()?;
    
    // Logs (as an OTel signal, not just stdout JSON)
    let logger_provider = opentelemetry_otlp::new_pipeline()
        .logging()
        .with_exporter(otel_exporter(&config.otlp_endpoint))
        .with_resource(resource)
        .install_batch(runtime::Tokio)?;
    
    // Global registration
    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider);
    
    // tracing crate bridge
    let tracing_layer = tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("tars"));
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().json())
        .with(tracing_layer)
        .init();
    
    Ok(TelemetryGuard { /* flush on drop */ })
}
```

### 9.2 Backend Selection

The OTLP protocol guarantees backend-agnosticism; you can plug into:

| Backend | Suited for |
|---|---|
| **Self-hosted Prometheus + Tempo + Loki + Grafana** | Team mode, fully self-managed |
| **OpenObserve / SigNoz / Uptrace** | Single open-source backend, simple ops |
| **Datadog / New Relic / Honeycomb** | SaaS, pay for convenience |
| **Grafana Cloud** | Self-hosted UX + managed |
| **VictoriaMetrics + VictoriaLogs + Jaeger** | High-performance open-source combo |
| **AWS / GCP / Azure native** | All-in-one in the cloud |

**Personal mode**: by default, store a small subset of metrics locally in SQLite, send no OTel; users may opt in to local Grafana / Honeycomb sandbox.

### 9.3 Collector Deployment

```
Application (tars binary)
        │
        │ OTLP gRPC (4317) / HTTP (4318)
        ▼
OpenTelemetry Collector (sidecar / daemonset)
        │
        ├─→ Prometheus remote write (metrics)
        ├─→ Loki / Elasticsearch (logs)
        ├─→ Tempo / Jaeger (traces)
        ├─→ S3 (cold archive)
        └─→ Splunk / Datadog (mirror)
```

Benefits of the Collector:
- Application emits one OTLP stream, doesn't need to know downstream
- Centralized config of sampling / batching / rate-limit
- Backend swap without restarting the application

---

## 10. Sampling Strategy Summary

| Signal | Default sampling | On-demand boost |
|---|---|---|
| Metrics | 100% (small after aggregation) | N/A |
| Events | 100% | N/A |
| Logs (INFO+) | 100% | DEBUG only enabled when trace_id matches |
| Logs (DEBUG) | 0% | Temporarily enabled via dynamic log level |
| Traces | 10% head + 100% tail-based (errors / slow / expensive) | Force 100% per tenant when debugging |

Dynamic tuning via ConfigManager hot reload (Doc 06 §6):

```toml
[telemetry.sampling]
trace_base_rate = 0.1
trace_always_sample_on_error = true
trace_always_sample_above_cost_usd = 1.0
log_default_level = "info"

[telemetry.tenant_overrides.acme_corp]
trace_base_rate = 1.0     # 100% trace for big customer
log_default_level = "debug"
```

---

## 11. Privacy and Redaction (Mandatory)

### 11.1 Content That Must Never Enter MELT

| Content | Mandatory rule |
|---|---|
| Prompt text | only hash + token_count + word_count allowed |
| LLM response text | only hash + token_count + stop_reason allowed |
| User code | path + line number + AST node type, never content |
| Tool args / output | only schema field names + size, never values |
| API key / token | never (not even hashed) |
| PII (email / phone / national ID) | not allowed; if must be emitted, SHA256 + first 6 chars |
| Full stack trace | function name + line OK, local variable values not OK |

### 11.2 Automatic Redaction

Enforced via the type system:

```rust
/// Marks a field as "must never log raw"
pub struct SecretField<T>(T);

impl<T> fmt::Display for SecretField<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<secret>")
    }
}

impl<T> fmt::Debug for SecretField<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SecretField(<redacted>)")
    }
}

pub struct PromptText(pub SecretField<String>);
pub struct LlmResponseText(pub SecretField<String>);
pub struct UserCode(pub SecretField<String>);
```

Any log/trace/event field containing `SecretField<T>` always renders as `<secret>` — even if someone inadvertently writes `tracing::info!(prompt = ?prompt)`.

### 11.3 Startup Validation

CI integrates lint: a custom lint in the style of `clippy::missing_docs_in_private_items` checks that field types in all log/trace calls don't contain unredacted sensitive fields.

```rust
// lint rule example (concept; in practice use dylint / proc-macro)
// ❌ FAIL
tracing::info!(prompt = %prompt_string, "got prompt");

// ✅ PASS
tracing::info!(prompt = %PromptText(SecretField(prompt_string)), "got prompt");

// ✅ PASS (use metadata)
tracing::info!(prompt.hash = %hash(&prompt_string), prompt.tokens = token_count, "got prompt");
```

---

## 12. Storage and Retention

### 12.1 Storage Tiers

| Tier | Medium | Retention | Purpose |
|---|---|---|---|
| Hot | Prometheus / Tempo / Loki | 7-15 days | Real-time queries, alerting |
| Warm | S3 / object storage | 30 days | Postmortems, SLO reports |
| Cold | S3 Glacier | 1 year | Compliance, audit assistance |

Cost control:
- Hot tier only retains essential metrics / sampled traces
- Cross-tier migration is automated (Tempo retention policy / Loki object store)
- Personal mode retains nothing (unless user opts in)

### 12.2 Deletion Contract

- When a tenant is deleted (§6), all data for that tenant in the MELT system **must also be deleted** (GDPR requirement)
- Implementation: tenant_id is a mandatory label, deletion is by label (Loki / Tempo both support this)
- Exception: aggregated, already-anonymized metrics (e.g., `llm.cost_usd_total{tenant="*"}`) may be retained

---

## 13. Alerting

### 13.1 Alert Sources

Alert rules run on Prometheus / Grafana / similar systems, not inside the application:

```yaml
# Prometheus alert rules
groups:
  - name: llm_provider
    rules:
      - alert: LlmProviderHighErrorRate
        expr: |
          sum(rate(llm_provider_errors_total[5m])) by (provider, tenant)
            / sum(rate(llm_provider_request_total[5m])) by (provider, tenant)
          > 0.05
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Provider {{ $labels.provider }} error rate > 5% for tenant {{ $labels.tenant }}"
      
      - alert: LlmCostBudgetSoftLimit
        expr: budget_soft_limit_exceeded_total > 0
        for: 1m
        labels:
          severity: warning
      
      - alert: LlmCostBudgetHardLimit
        expr: budget_hard_limit_exceeded_total > 0
        for: 1m
        labels:
          severity: critical
      
      - alert: AgentBacktrackRateHigh
        expr: |
          sum(rate(agent_backtrack_total[15m])) by (tenant)
            / sum(rate(agent_trajectory_completed_total[15m])) by (tenant)
          > 0.20
        for: 15m
        labels:
          severity: warning
        annotations:
          summary: "Tenant {{ $labels.tenant }} backtrack rate > 20% (task definition issue?)"
      
      - alert: CompensationFailed
        expr: increase(compensation_failed_total[5m]) > 0
        for: 0m
        labels:
          severity: page              # PagerDuty
        annotations:
          summary: "Compensation failed - system in inconsistent state, manual intervention required"
      
      - alert: SecurityEventDetected
        expr: increase(prompt_injection_detected_total[5m]) > 10
        for: 0m
        labels:
          severity: warning
```

### 13.2 SLO and Error Budget

```yaml
# Sloth / OpenSLO style
slos:
  - name: llm_request_availability
    objective: 99.5
    sli:
      good: sum(rate(llm_provider_request_total{status="success"}[5m]))
      total: sum(rate(llm_provider_request_total[5m]))
    
  - name: llm_request_p95_latency
    objective: 95
    sli:
      good: sum(rate(llm_provider_total_latency_ms_bucket{le="30000"}[5m]))
      total: sum(rate(llm_provider_total_latency_ms_count[5m]))
```

When the error budget is exhausted, automatically: pause deploys / raise priority / notify product owner.

---

## 14. Differentiation by Deployment Form

| Form | Metrics | Events | Logs | Traces | OTel Endpoint |
|---|---|---|---|---|---|
| Personal | local SQLite, core metrics only | off | stdout JSON | off | not enabled |
| Personal (opt-in) | same as above | on | same as above | 10% sampling | user-configured endpoint |
| Team | all on | all on | all on (INFO+) | 10% sampling | customer OTel Collector |
| SaaS | all on + billing aggregation | all on | all on (INFO+) | 10% sampling + full tail-based | vendor OTel Collector |
| Hybrid | local all on | local all on | local all on | local all on | **local** OTel Collector; **cloud** receives only anonymized aggregates |

Why Personal mode defaults to all-off:
- Individual users' privacy comes first
- Individual dev scenarios don't need SRE-grade monitoring
- Any outbound traffic requires explicit user opt-in (startup banner prompt)

---

## 15. Testing Strategy

### 15.1 Cardinality Regression Guard

```rust
#[test]
fn no_metric_uses_disallowed_label() {
    let validator = LabelValidator::production();
    let registry = test_metric_registry();
    
    for (name, labels) in registry.all_metrics() {
        for label in labels {
            assert!(
                validator.allowed_labels.contains(&label),
                "Metric {} uses disallowed label {}", name, label
            );
        }
    }
}
```

### 15.2 Redaction Lint

```rust
#[test]
fn no_log_call_passes_raw_secret_field() {
    // Use syn to parse all tracing!() calls across the codebase
    // Check that each field's type isn't String/&str/raw Prompt/Code types
    // Must be SecretField-wrapped or a primitive (u64 / hash etc.)
    let violations = scan_codebase_for_unsafe_log_calls("./src");
    assert_eq!(violations, vec![], "found {} unsafe log calls", violations.len());
}
```

### 15.3 Trace Completeness

```rust
#[tokio::test]
async fn full_request_produces_expected_span_tree() {
    let runtime = test_runtime_with_otel_capture();
    let task = runtime.submit(test_spec(), test_principal()).await.unwrap();
    runtime.wait_until_done(task).await;
    
    let spans = runtime.captured_spans();
    let tree = build_span_tree(&spans);
    
    // verify key spans exist
    assert!(tree.find("middleware.iam").is_some());
    assert!(tree.find("middleware.cache.lookup").is_some());
    assert!(tree.find("provider.openai.stream").is_some());
    
    // verify causal relationships
    assert!(tree.is_ancestor("middleware.cache.lookup", "cache.l1.get"));
}
```

### 15.4 Sampling Test

```rust
#[tokio::test]
async fn errors_always_sampled_regardless_of_base_rate() {
    let sampler = AdaptiveSampler::new(0.0);  // base 0%
    
    let error_ctx = SamplingContext { 
        has_error: true, 
        cost_usd: 0.001, 
        ..Default::default() 
    };
    
    for _ in 0..100 {
        assert_eq!(sampler.should_sample(&error_ctx), SamplingDecision::AlwaysSample);
    }
}
```

---

## 16. Anti-pattern Checklist

1. **Don't conflate audit log / AgentEvent / MELT** — storage, retention, sampling, and failure handling differ entirely.
2. **Don't put high-cardinality fields in metric labels** (trace_id / user_id / message) — will blow up Prometheus.
3. **Don't emit prompt / response / raw code in logs** — enforce redaction via SecretField.
4. **Don't add a span to every function "for coverage"** — coarse-grained first; deep span trees waste perf and are hard to read.
5. **Don't let telemetry failures impact business** — MELT is best-effort, drop + alert, never block.
6. **Don't enable outbound telemetry by default in Personal mode** — must be opt-in.
7. **Don't substitute text grep for structured logs** — logs must be JSON / OTel.
8. **Don't run alert rules inside the application** — rules belong in Prometheus / Grafana / OpsGenie; the app only emits data.
9. **Don't stash cost metrics only in the metric pillar** — also write the billing_events table; the metric is just an aggregated view.
10. **Don't ignore cardinality growth** — periodically review actual time-series counts produced and alert on threshold breach.
11. **Don't sample traces at 100%** — cost explodes; combine head + tail.
12. **Don't make synchronous OTel calls on hot paths** — the SDK must batch + async-export.
13. **Don't double-count** — incrementing the same metric in two places yields 2x counts.
14. **Don't let deleted tenants' data linger in MELT** — GDPR requires cascade deletion.
15. **Don't assume every backend supports every OTel feature** — Tempo doesn't support complex metric queries, Loki isn't great with high-cardinality logs — validate during selection.

---

## 17. Contracts with Upstream and Downstream

### Upstream (Doc 01-07 components) commitments

- All instrumentation through the OTel SDK, never write directly to file / stdout
- Calls carry the correct RequestContext (with trace_id / tenant_id)
- Sensitive fields must be wrapped in SecretField
- High-frequency hot-path functions must not have a trace span (avoid perf loss)

### Downstream (OTel Collector / Backend) contract

- Accepts OTLP gRPC (4317) or HTTP (4318)
- On failure, let the SDK queue and retry; don't reject immediately
- The backend must not be a single point of failure — SDK pairs with the collector's local disk buffer

### Cross-pillar correlation

- Metric labels must share names with trace attributes / log fields (allow drill-down)
- A trace's trace_id must appear on all related log lines
- An Event must be linkable to a full trace via trace_id

---

## 18. TODOs and Open Questions

- [ ] Evaluate eBPF profiling integration (Pyroscope / Parca)
- [ ] Cardinality monitoring metric (recursive: monitor metric count itself)
- [ ] Cross-language tracing (Rust main + Python MCP server)
- [ ] OTel SDK Rust version maturity (vs Go / Java)
- [ ] Extension points for custom trace processors (extra enrichment in unusual cases)
- [ ] Metric pre-aggregation (rollup) at the Collector vs. application layer
- [ ] Personal mode local dashboard (embed Grafana? roll our own?)
- [ ] Alert dedup / flap suppression (don't spam on the same issue)
- [ ] Auto-generated user-level SLO reports (for customer success)
- [ ] AI-assisted root cause analysis (LLM reads metric + log, produces hypotheses)
