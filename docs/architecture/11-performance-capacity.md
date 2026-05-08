# Doc 11 — Performance & Capacity Planning

> Scope: performance SLOs, benchmarking methodology, capacity planning formulas, bottleneck analysis, scaling strategy, cost optimization, load testing and performance regression detection.
>
> Context: this doc introduces no new components; it specifies the performance targets, capacity boundaries, and load-testing methods for the components defined in Docs 01-10.

---

## 1. Design goals

| Goal | Notes |
|---|---|
| **No perceptible latency added outside the LLM path** | Total middleware overhead P99 < 10ms; on cache miss, near-bare Provider call |
| **Cache hits must be substantially faster** | L1 hit < 5ms, L2 hit < 30ms — orders of magnitude below the 5-60s of a bare call |
| **Horizontally scalable** | In Team / SaaS modes, 10x QPS via more replicas — no architecture changes |
| **Tail latency under control** | P99 must not be 100x P50; Provider jitter isolated via retry / fallback |
| **Predictable resource usage** | Given QPS and tenant count, required CPU / RAM / DB capacity is estimable |
| **Backpressure, not crash** | Under overload: degrade (reject / queue / lower quality); never let processes OOM or thread pools avalanche |
| **Zero tolerance for benchmark regressions** | Performance-critical paths have benchmarks; CI catches regressions of 5%+ |
| **Cost is observable** | Provider cost dominates 95%+; the optimization focus is cache hit rate and model selection |

**Anti-goals**:
- Don't over-optimize for "ultimate latency" (saving 100µs is meaningless next to multi-second LLM latency)
- No "performance optimization" without data — every optimization must be backed by profiling
- Don't throw machines at architecture problems — bottlenecks that 10x resources can't solve need a redesign
- Don't sacrifice readability for benchmark numbers

---

## 2. Performance SLOs

### 2.1 User-visible latency

| Operation | P50 target | P99 target | P99.9 target |
|---|---|---|---|
| Cache hit (L1) | < 2ms | < 10ms | < 30ms |
| Cache hit (L2) | < 10ms | < 50ms | < 200ms |
| LLM call TTFT | < 1s | < 3s | < 10s |
| LLM call total (short output) | < 5s | < 30s | < 60s |
| LLM call total (long output) | < 30s | < 120s | < 300s |
| Tool call (local) | < 100ms | < 500ms | < 2s |
| Tool call (MCP subprocess, warm) | < 50ms | < 200ms | < 1s |
| Tool call (MCP subprocess, cold) | < 500ms | < 2s | < 5s |
| Trajectory submit → first event | < 100ms | < 500ms | < 2s |

### 2.2 Middleware internal budgets

Per-layer latency targets, summing to under 10ms (P99):

| Layer | P99 budget |
|---|---|
| Telemetry (span creation) | < 0.5ms |
| Auth (cached principal) | < 0.5ms |
| IAM (cached scope eval) | < 1ms |
| Budget check (Redis Lua) | < 2ms |
| Cache lookup (L1 miss + L2 query) | < 3ms |
| Prompt Guard fast lane (aho-corasick) | < 1ms |
| Routing (table lookup) | < 0.5ms |
| Circuit breaker check | < 0.5ms |
| Retry wrap | < 0.5ms |
| **Sum** | **< 10ms** |

Prompt Guard slow lane (ONNX) is off the serial path (Doc 02 §4.5 parallel) and is not counted.

### 2.3 Throughput SLOs

| Dimension | Personal | Team (single instance) | Team (3 replicas) | SaaS |
|---|---|---|---|---|
| Concurrent trajectories | 5 | 200 | 600 | 10000+ |
| LLM call QPS | 5 | 100 | 300 | 5000+ |
| Cache lookup QPS | 50 | 2000 | 6000 | 50000+ |
| Event write QPS | 50 | 1500 | 4500 | 30000+ |
| Tool invocation QPS | 5 | 500 | 1500 | 10000+ |

---

## 3. Bottleneck analysis

To know where to scale, you have to understand each layer's bottleneck. Walking from outside in for a typical LLM request:

### 3.1 Latency breakdown

```
Typical P50 breakdown for cache miss + LLM call (10s total):

User Input
   │
   │  ↓ <1ms   axum handler parse + auth
   │
Middleware Stack
   │  ↓ <10ms  auth/iam/budget/cache_lookup (miss)/guard fast/routing
   │
Provider Adapter
   │  ↓ <5ms   prompt assembly + reqwest send
   │
Network → LLM Provider
   │  ↓ ~200ms  TLS handshake + cross-region network + provider queue
   │
LLM Inference
   │  ↓ 200-1000ms  TTFT (provider-side compute)
   │
   │  ↓ 5-30s    remaining token streaming
   │
Network → Runtime
   │  ↓ <10ms   stream receive
   │
Middleware (egress)
   │  ↓ <5ms    schema_validation / cache_write_async / cost_accounting
   │
Response → User
```

**Key observations**:
- 90%+ of the time is in Provider (network + inference)
- The portion under our control is < 5%
- The real performance lever is **avoiding the call** (cache hit) or **picking a faster Provider** (model tier)

### 3.2 Single-instance bottleneck progression

As QPS rises, bottlenecks appear in order:

| QPS | Primary bottleneck | Mitigation |
|---|---|---|
| < 10 | None | - |
| 10-100 | Postgres writes (event log) | Batched insert + async write |
| 100-500 | Redis command RTT | Pipelining + larger connection pool |
| 500-1000 | tokio runtime scheduling | Increase worker_threads |
| 1000-5000 | Provider rate limit | Provider replicas (multiple accounts) + Routing spread |
| > 5000 | Per-instance file handles / network | Horizontal scaling (multi-replica) |

### 3.3 Capacity bottlenecks

| Resource | Per-instance ceiling | Solvable by horizontal scale |
|---|---|---|
| CPU | 16 cores | ✅ |
| Memory | 32 GB | ✅ |
| File handles | 65535 (per process) | ✅ |
| Postgres connections | 100-500 (incl. pool) | ❌ (DB is centralized) |
| Redis connections | 10000+ | ❌ |
| Provider QPS | Provider-determined | Partial (multi-account + routing) |
| Provider TPM | Provider-determined | Partial |
| Monthly LLM spend | Budget-determined | ❌ |

Centralized resources (Postgres / Redis / Provider quota) need their own scaling strategy — adding replicas isn't enough.

---

## 4. Resource sizing

### 4.1 Application instance

```
Baseline (idle):
  - CPU: 0.1 vCPU
  - RAM: 200 MB

Per concurrent trajectory:
  - CPU: 0.02 vCPU (mostly awaiting)
  - RAM: 50-200 KB (state) + 0-2 MB (in-flight prompt buffer)

Per session (idle):
  - RAM: 10-50 KB

Per CLI subprocess (Doc 01 §6.2):
  - CPU: 0.2 vCPU (single claude CLI process)
  - RAM: 100-300 MB
  - File handles: ~20

Per MCP subprocess (Doc 05 §5.3):
  - CPU: 0.05-0.2 vCPU (depends on server impl)
  - RAM: 30-150 MB
  - File handles: ~10
```

**Estimation formula** (single instance):

```
Required vCPU = 1 + (concurrent_trajectories × 0.02) 
              + (CLI_sessions × 0.2) 
              + (MCP_sessions × 0.1)

Required RAM (MB) = 500 + (concurrent_trajectories × 0.5)
                  + (idle_sessions × 0.03)  
                  + (CLI_sessions × 200)
                  + (MCP_sessions × 100)
```

Example instance sizes:

| Instance size | Suitable for | Approximate capacity |
|---|---|---|
| 1 vCPU + 2GB | Personal mode | 5-10 trajectories + 2 CLI |
| 2 vCPU + 4GB | Small team (10-20 people) | 50 trajectories + 5 CLI + 10 MCP |
| 4 vCPU + 8GB | Team (50 people) | 200 trajectories + 20 CLI + 30 MCP |
| 8 vCPU + 16GB | Enterprise (200 people) | 500 trajectories + 50 CLI + 80 MCP |
| 16 vCPU + 32GB | Single-instance ceiling for large enterprise | 1000 trajectories |
| > 16 vCPU | Multi-replica horizontal scaling | - |

### 4.2 Postgres

```
Baseline:
  - CPU: 2 vCPU
  - RAM: 4 GB
  - Disk: 50 GB SSD (30 days hot data + WAL)

Per 1000 concurrent trajectories:
  - CPU: +1 vCPU (writes + index maintenance)
  - RAM: +2 GB (shared_buffers + cache)
  - Disk IOPS: +500 sustained

Per million daily events:
  - Disk: +12 GB (with indexes)
```

Recommended instances:

| Scale | Instance | Notes |
|---|---|---|
| < 100 concurrent | db.t4g.large (2c/8GB) | Starter, burst mode OK |
| 100-500 concurrent | db.r6g.large (2c/16GB) | RAM-prioritized |
| 500-2000 concurrent | db.r6g.xlarge (4c/32GB) | + read replica |
| > 2000 concurrent | db.r6g.2xlarge + 2-3 read replica | + sharding (by tenant range) |

### 4.3 Redis

```
Baseline:
  - CPU: 1 vCPU
  - RAM: 2 GB

Per 100k cache entries (avg 4KB):
  - RAM: +400 MB

Per 1000 concurrent budget operations/s:
  - CPU: +0.5 vCPU
```

Recommended instances:

| Scale | Instance | Notes |
|---|---|---|
| < 100 concurrent | cache.t4g.small (1c/2GB) | Starter |
| 100-1000 concurrent | cache.r6g.large (2c/16GB) | Single node sufficient |
| > 1000 concurrent | Redis Cluster (3-6 sharded) | HA + capacity |

### 4.4 OTel Collector / observability stack

```
Per 10k metric samples/min:
  - CPU: 0.2 vCPU
  - RAM: 200 MB

Per 1k traces/s (with 10% sampling):
  - CPU: 0.5 vCPU
  - RAM: 500 MB

Per 10k log lines/s:
  - CPU: 0.5 vCPU
  - RAM: 1 GB
```

Typically OTel Collector runs sidecar (in the same pod as the app) and doesn't need separate scaling.

---

## 5. Concurrency model

### 5.1 Tokio runtime configuration

```rust
fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get())          // default = CPU core count
        .max_blocking_threads(512)                 // blocking task pool
        .thread_name("tars-worker")
        .thread_stack_size(2 * 1024 * 1024)        // 2MB default is fine
        .enable_all()
        .build()
        .unwrap();
    
    rt.block_on(async_main());
}
```

Key points:
- **CPU-bound work must use spawn_blocking** (e.g., ONNX inference / sha256 over large data)
- **Network I/O is fully async** (reqwest / sqlx / redis-rs are natively async)
- **Don't hold a std::sync::Mutex across an await in async** (it blocks the worker)

### 5.2 Backpressure strategy

Every layer has a "queue cap"; over the cap means reject rather than pile up:

```rust
pub struct BoundedExecutor {
    semaphore: Arc<Semaphore>,
    rejected_total: Counter,
}

impl BoundedExecutor {
    pub fn new(max_concurrent: usize) -> Self { ... }
    
    pub async fn try_execute<F, T>(&self, fut: F) -> Result<T, BackpressureError>
    where F: Future<Output = T>
    {
        let permit = self.semaphore.try_acquire()
            .map_err(|_| {
                self.rejected_total.add(1, &[]);
                BackpressureError::QueueFull
            })?;
        
        let result = fut.await;
        drop(permit);
        Ok(result)
    }
}
```

Application points:
- HTTP handler ingress: max 1000 concurrent requests; over the cap returns 503
- LLM provider calls: each provider gets max N inflight (per provider quota)
- Tool subprocess: max M active subprocesses (avoid file-handle exhaustion)
- Postgres pool: max P connections; over the cap waits with a timeout

### 5.3 Channel capacity

```rust
// MPSC channels must be bounded; otherwise a slow consumer OOMs
let (tx, rx) = mpsc::channel(1024);

// Same for broadcast channels
let (tx, _) = broadcast::channel(256);

// Slow consumer handling:
match rx.recv().await {
    Ok(msg) => ...,
    Err(broadcast::error::RecvError::Lagged(n)) => {
        // dropped n messages, record metric + warn
        tracing::warn!(skipped = n, "subscriber lagged");
    }
    Err(_) => break,
}
```

### 5.4 Special handling for LLM streaming

Backpressure for LLM streaming responses:

```rust
// inner_stream is the BoxStream<ChatEvent> returned by Provider
// outer_consumer is the upstream Frontend Adapter

let stream = inner_stream
    .ready_chunks(10)              // batch, reduce cross-await overhead
    .timeout_at(deadline)          // overall timeout
    .take_until(cancel.cancelled());

while let Some(chunk) = stream.next().await {
    // if the consumer is slow (UI render hitching), don't accumulate
    if outbound_tx.try_send(chunk).is_err() {
        // outbound full → drop / coalesce
        metrics.record_stream_drop();
    }
}
```

---

## 6. Cache performance and ROI

### 6.1 Economics of cache hit rate

```
Per LLM call cost:    $0.05 (example, gpt-4o average)
Cache hit marginal:   $0.0001 (Redis lookup)
Hit rate savings:
  20% hit rate:  saves 20% × $0.05 = $0.01/req
  50% hit rate:  saves ~$0.025/req
  80% hit rate:  saves ~$0.04/req
```

100k req/day × 50% hit rate = $2500/day saved = $75k/month. **This is the core ROI of caching.**

### 6.2 Cache hit rate monitoring

Sliced by tenant / provider / model_tier:

```
Metric: llm.cache.hit_rate{tenant, provider, level}
Alert: hit_rate < 30% for 1h → SRE investigate
       (could be unstable PromptBuilder / hasher_version recently bumped / genuinely no reuse)
```

### 6.3 Cost boundary of L3 explicit cache

See Doc 03 §10.5 + the cost analysis we did earlier:

```
L3 cache economic model:
  Idle fee: $1/hour/100k tokens (Gemini estimate)
  Call discount: input cost × 25%
  
  Break-even: number of calls N within an hour satisfies
    N × original_input_cost × 0.75 > $1
  
  For a 100k token prefix:
    original_input_cost = 100k × $1.25/M = $0.125/call
    Break-even N = 1 / (0.125 × 0.75) ≈ 11 calls/hour
```

Janitor (Doc 03 §8) must monitor each L3 handle's actual call frequency and proactively delete those below the break-even threshold.

### 6.4 Cache performance benchmarks

```rust
#[bench]
fn bench_cache_l1_hit(b: &mut Bencher) {
    let cache = test_l1_cache();
    let key = test_key();
    cache.put(&key, test_value()).await;
    
    b.iter(|| {
        runtime.block_on(async {
            cache.get(&key).await.unwrap();
        })
    });
}
```

Targets: L1 hit < 100µs (in-memory lookup + clone); L2 hit < 5ms (Redis RTT).

---

## 7. Horizontal scaling

### 7.1 Statelessness of application instances

App instances must be stateless:

- All persistent state lives in Postgres / Redis / S3
- In-memory cache (L1) is best-effort; falls back to L2 on miss
- Subprocesses (CLI / MCP) are instance-local but pinned via session-id-based routing for stickiness

### 7.2 Session affinity (sticky routing)

Some scenarios need session stickiness:
- After a CLI subprocess (Claude / Gemini) is created on instance A, follow-up requests for the same session should preferably return to A (to reuse the subprocess)
- Otherwise every instance ends up creating its own subprocess and file handles explode

Implementation:
- LB-layer sticky routing keyed on `session_id` hash (HAProxy / Envoy)
- Session migration when an instance goes down: mark dirty + the next request rebuilds on a new instance

### 7.3 Auto-scaling

```yaml
# K8s HPA example
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: tars-server
spec:
  minReplicas: 3
  maxReplicas: 30
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: 60
    - type: Pods
      pods:
        metric:
          name: trajectory_active_per_pod
        target:
          type: AverageValue
          averageValue: "100"
    # Custom: scale based on backpressure rejection rate
    - type: Pods
      pods:
        metric:
          name: backpressure_rejection_rate
        target:
          type: Value
          value: "0.01"        # rejection rate > 1% scales out immediately
```

**Key**:
- Scale out early (60% CPU, not 80%) — LLM tasks are long and new instances warm up slowly
- Scale in slowly (only after 10 min stable) to avoid oscillation
- Drain before scale-in: SIGTERM → reject new requests → wait for in-flight to complete → exit

### 7.4 Scaling centralized components (Postgres / Redis)

Can't be solved by adding replicas; must:

**Postgres**:
1. **Read replicas**: spread read traffic
2. **Per-tenant sharding**: huge tenants get a dedicated cluster
3. **Hot/cold tiering** (Doc 09 §6): hot in OLTP, cold in S3
4. **Citus / distributed Postgres**: at very large scale (10k+ tenants)

**Redis**:
1. **Redis Cluster**: native sharding
2. **Multi-instance by function**: split cache / budget / pubsub

---

## 8. Cost optimization

### 8.1 Cost structure

```
Typical SaaS deployment cost breakdown:
  - LLM Provider API:      85-95%   ← the real money
  - Compute (instances):    3-8%
  - Storage (DB + S3):      1-3%
  - Network (egress):       1-2%
  - Observability backend:  1-2%
```

LLM cost dominates absolutely. All cost optimization is focused there.

### 8.2 Levers for LLM cost optimization

In order of impact:

1. **Cache hit rate** (§6): direct dollar savings; target > 50%
2. **Model tier optimization** (Doc 04 §4.3): use small models for small jobs; reasoning tier strictly on demand
3. **Prompt length optimization**: cut RAG context / history / tools schema redundancy
4. **Structured Output instead of multi-turn**: strict schema produces results in one shot, no back-and-forth clarification
5. **L3 explicit cache** (Doc 03): reuse long system prompts
6. **Speculative decoding (provider-side)**: let the provider use a small model to speculate-accelerate
7. **Provider arbitrage**: same tier across providers can vary 2-5x in price; Routing picks the cheapest

### 8.3 Per-tenant cost dashboard

```
TARS Cost Dashboard - Tenant: acme_corp
================================================
Period: 2026-05 (so far)

Total Spend:                  $342.18
  - LLM Calls:                $325.02 (95%)
  - L3 Cache Storage:         $12.15
  - Compute Allocation:       $5.01

LLM Breakdown by Model:
  claude-opus-4-7:            $215.40 (66%)  ← most expensive
  gpt-4o:                     $67.30 (21%)
  gemini-2.5-flash:           $42.32 (13%)

LLM Breakdown by Skill:
  code-review-deep:           $189.20 (58%)  ← optimization focus
  security-audit:             $98.42 (30%)
  doc-summarize:              $37.40 (12%)

Optimization Opportunities:
  ⚠ code-review-deep: cache hit rate 12%, could be 40%+
    → Stabilize PromptBuilder.static_prefix
  ⚠ security-audit: 80% calls use reasoning tier, only 30% need it
    → Move simple checks to default tier
  ✓ doc-summarize: cache hit rate 76%, well optimized
```

### 8.4 Auto-degradation

```rust
pub struct CostBasedRouter {
    base_router: Arc<dyn RoutingPolicy>,
    cost_threshold: f64,
}

impl RoutingPolicy for CostBasedRouter {
    async fn select(&self, req: &ChatRequest, ...) -> Result<Vec<ProviderId>, _> {
        let tenant_remaining = self.budget.remaining(&ctx.tenant_id).await?;
        let estimated = estimate_cost(req);
        
        // when budget is tight, downgrade tier
        if tenant_remaining < estimated * 10.0 {
            // demote reasoning tier to default tier
            let downgraded_req = req.with_tier(ModelTier::Default);
            return self.base_router.select(&downgraded_req, ...).await;
        }
        
        self.base_router.select(req, ...).await
    }
}
```

---

## 9. Load testing methodology

### 9.1 Load testing toolchain

```rust
// criterion - micro benchmarks
[dev-dependencies]
criterion = "0.5"
divan = "0.1"

// end-to-end - k6 / locust / vegeta
```

### 9.2 Three load-testing targets

| Type | Goal | Tool |
|---|---|---|
| Micro benchmark | Single-function performance, catch regressions | criterion / divan |
| Component benchmark | Single component (e.g., CacheRegistry) | criterion + mock dependencies |
| End-to-end load test | Overall throughput and latency | k6 + real staging |

### 9.3 Micro benchmark suite

```rust
// benches/critical_path.rs
use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};

fn bench_cache_key_compute(c: &mut Criterion) {
    let factory = CacheKeyFactory::default();
    let req = test_request_1k_tokens();
    let ctx = test_ctx();
    
    c.bench_function("cache_key.compute", |b| {
        b.iter(|| factory.compute(black_box(&req), black_box(&ctx)).unwrap())
    });
}

fn bench_iam_evaluation(c: &mut Criterion) {
    let iam = test_iam_engine();
    let principal = test_principal_with_n_scopes(50);
    
    c.bench_function("iam.evaluate", |b| {
        b.iter(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                iam.evaluate(&principal, &test_resource(), Action::Invoke).await.unwrap()
            })
        })
    });
}

fn bench_aho_corasick_guard(c: &mut Criterion) {
    let guard = FastGuard::with_default_patterns();
    
    let mut group = c.benchmark_group("guard.scan");
    for size in [100, 1000, 10000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let text = generate_text(size);
            b.iter(|| guard.scan(black_box(&text)));
        });
    }
}

criterion_group!(benches, bench_cache_key_compute, bench_iam_evaluation, bench_aho_corasick_guard);
criterion_main!(benches);
```

### 9.4 End-to-end load test script (k6)

```javascript
// load_test.js
import http from 'k6/http';
import { check, sleep } from 'k6';

export const options = {
  stages: [
    { duration: '2m', target: 10 },     // ramp-up
    { duration: '5m', target: 100 },    // sustained
    { duration: '2m', target: 200 },    // peak
    { duration: '3m', target: 0 },      // ramp-down
  ],
  thresholds: {
    http_req_duration: ['p(95)<5000', 'p(99)<30000'],
    http_req_failed: ['rate<0.01'],
  },
};

export default function () {
  const payload = JSON.stringify({
    skill: 'code-review',
    repo: 'test-repo',
    pr: Math.floor(Math.random() * 1000),
  });
  
  const res = http.post(
    'https://staging.tars.example/api/tasks',
    payload,
    { headers: { 'Content-Type': 'application/json', 'Authorization': `Bearer ${__ENV.TOKEN}` } }
  );
  
  check(res, { 'status 202': r => r.status === 202 });
  sleep(1);
}
```

### 9.5 Real-traffic shadow testing

Mirror production traffic to staging:

```
Production:
  User → Production Cluster → Real LLM calls
                ↓ async mirror
  Staging:
  Mirrored requests → Staging Cluster → Mock LLM (same delay distribution)
```

Capture the shape of production traffic (request intervals, payload size distribution, tenant distribution) and replay in staging. The Mock LLM replays historical responses to avoid actual spend.

---

## 10. Performance regression detection

### 10.1 CI integration

```yaml
# .github/workflows/bench.yml
- name: Run benchmarks
  run: cargo bench --bench critical_path -- --output-format bencher | tee bench.txt

- name: Compare with baseline
  uses: benchmark-action/github-action-benchmark@v1
  with:
    tool: 'cargo'
    output-file-path: bench.txt
    external-data-json-path: ./cache/benchmark-data.json
    fail-on-alert: true
    alert-threshold: '110%'         # fail on 10% slowdown
    comment-on-alert: true
```

### 10.2 Continuous baseline tracking

Every commit on main runs benchmarks; results are stored on a GitHub Pages dashboard. Trend-line deterioration triggers investigation.

### 10.3 Production-like benchmark

```rust
// not just micro benchmarks — also run e2e benches close to real scenarios
fn bench_full_request_path(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pipeline = runtime.block_on(setup_realistic_pipeline());
    
    c.bench_function("full_request.cache_hit", |b| {
        // warm cache
        runtime.block_on(prime_cache(&pipeline, &test_request()));
        
        b.to_async(&runtime).iter(|| async {
            pipeline.execute(test_request()).await.unwrap()
        });
    });
}
```

---

## 11. Hot spots and abnormal traffic

### 11.1 Single-tenant burst

A tenant's CI pipeline suddenly fires 1000 concurrent PR reviews → that tenant blows past quota → impacts other tenants?

**Countermeasures**:
- Per-tenant BoundedExecutor (max concurrent per tenant)
- Postgres connection pool with per-tenant quotas (no single tenant can exhaust the pool)
- Abnormal growth triggers auto-degradation (switch to a cheaper model tier)

### 11.2 Long-tail single trajectory

A trajectory has been running 30 minutes and won't release resources:

**Countermeasures**:
- Hard cap on TaskBudget.max_wall_clock (Doc 04 §8)
- On timeout, force cancel + persist partial result
- Track ultra-long-task metrics separately; abnormal counts trigger product discussion (task design issue?)

### 11.3 Cache stampede

A popular prompt's cache entries expire simultaneously → hundreds or thousands of requests miss at once → all hit the Provider:

**Countermeasures**:
- Singleflight (Doc 03 §6): for the same key, only one concurrent miss is allowed through
- TTL with jitter (avoid simultaneous expiry)
- On Provider rate-limit, fail-fast (circuit breaker)

### 11.4 Provider jitter

A Provider's latency suddenly doubles:

**Countermeasures**:
- Circuit breaker trips (Doc 02 §4.7)
- Routing automatically switches to fallback provider
- TTFT anomaly alert (based on EWMA baseline)

---

## 12. Profiling toolchain

### 12.1 Continuous profiling

```rust
// pprof crate integration
[dependencies]
pprof = { version = "0.x", features = ["protobuf-codec", "flamegraph"] }

// enabled in dev / staging
#[cfg(feature = "pprof")]
fn start_pprof_server(port: u16) {
    tokio::spawn(async move {
        let server = axum::Router::new()
            .route("/debug/pprof/profile", get(pprof_handler));
        // ...
    });
}
```

In production, use `Pyroscope` / `Parca` for continuous sampling to find CPU / memory hot spots.

### 12.2 Async task profiling

`tokio-console` for live observation of async task state:

```rust
console_subscriber::init();   // in dev / staging
```

```bash
tokio-console http://staging.tars.internal:6669
```

You can see:
- Per-task idle / busy / poll time
- Tasks blocking the worker
- Channel full / empty states

### 12.3 Memory profiling

```rust
// jemalloc + heap profiling
[dependencies]
tikv-jemallocator = { version = "0.6", features = ["profiling"] }

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
```

```bash
jeprof --show_bytes /path/to/tars heap.prof
```

---

## 13. Anti-pattern checklist

1. **Don't do "performance optimization" without profiling data** — the bottleneck you guess at is rarely the real one.
2. **Don't hold a sync lock across an await in async** — it blocks an entire worker.
3. **Don't use unbounded channels** — slow consumers OOM.
4. **Don't load the tokenizer / parse config on every request** — preload + share via Arc.
5. **Don't do syscalls (open/stat/read) on the hot path** — file I/O must be async + cached.
6. **Don't try to "save connections" by using a single Postgres connection** — concurrency contention makes it slower; pooling is the answer.
7. **Don't return 5xx when Backpressure rejects** — use 429 + Retry-After.
8. **Don't let graceful shutdown wait for all in-flight indefinitely** — set a hard cap (e.g., 60s); kill on overrun.
9. **Don't put high-cardinality fields in metric labels** — see Doc 08 §5.5.
10. **Don't load-test against the real LLM API** — costs are unbounded; use mocks + real latency distributions.
11. **Don't treat micro benchmarks as production performance prediction** — only E2E load tests reflect real load.
12. **Don't ignore P99 / P99.9** — averages mask the real problems.
13. **Don't use uniform cache TTLs** — add jitter to avoid stampedes.
14. **Don't let a single tenant exhaust global resources** — per-tenant quotas.
15. **Don't run DEBUG logs in production** — log I/O can be slower than the actual work.

---

## 14. Contracts with upstream and downstream

### Upstream (caller) commitments

- Respect rate limit / quota; back off on 429
- No pointless polling (use SSE / WebSocket)
- Paginate large requests appropriately

### Downstream (Provider / DB / observability) contracts

- Provider: publish rate limits + document typical latency
- DB: connection pools properly sized; slow-query monitoring enabled
- Observability: real-time aggregation; query latency < 5s

### Cross-SLO contracts

- Application SLOs (this doc §2) are the contract with the outside world
- Internal budgets (§2.2) are internal targets for individual teams / components
- Sum of internal budgets should be < application SLO, leaving headroom

---

## 15. TODO and open questions

- [ ] Connection reuse optimization for LLM provider calls (HTTP/2 multiplexing)
- [ ] Parallelism for large ContentRef uploads to S3 (multipart upload threshold)
- [ ] Prepared statement caching strategy for the Postgres connection pool
- [ ] Empirical tuning of tokio runtime worker_threads (vs CPU core count)
- [ ] Empirical optimal idle timeout for the CLI subprocess pool
- [ ] Adaptive cache TTL (extend for hot, shorten for cold)
- [ ] Chunking strategy for LLM response streams (network efficiency vs latency)
- [ ] End-to-end trace + profile correlation (find the specific hot function from a trace)
- [ ] Latency optimization for cross-region deployment (Provider region affinity)
- [ ] AI-assisted capacity planning (LLM reads historical metrics, gives sizing recommendations)
- [ ] **Speculative Execution**: while the LLM streams a JSON action, use early tokens to pre-warm related caches / pre-parse parameter schemas / pre-warm subprocesses; target 100-300ms TTFT gain (requires streaming JSON incremental parsing + cancellation logic for speculative execution)
