# 文档 11 — 性能与容量规划

> 范围：性能 SLO、基准测试方法论、容量规划公式、瓶颈分析、扩缩容策略、成本优化、压测与性能回归检测。
>
> 上下文：本文档不引入新组件，规范前面 Doc 01-10 各组件的性能目标、容量边界、压测方法。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **不增加 LLM 路径外的可感知延迟** | Middleware 总开销 P99 < 10ms,Cache miss 时近似裸调 Provider |
| **Cache hit 必须显著快** | L1 hit < 5ms,L2 hit < 30ms,与裸调动辄 5-60s 形成数量级差异 |
| **可水平扩展** | Team / SaaS 模式下 10x QPS 通过加副本即可,不改架构 |
| **尾延迟可控** | P99 不能比 P50 大 100x;Provider 抖动通过 retry / fallback 隔离 |
| **资源使用可预测** | 给定 QPS 和 tenant 数,能估算所需 CPU / RAM / DB 容量 |
| **背压而非崩溃** | 超载时降级 (拒绝 / 排队 / 降低质量),绝不让进程 OOM 或线程池雪崩 |
| **基准回归零容忍** | 性能关键路径有 benchmark,CI 检测 5%+ 回归 |
| **成本可观测** | Provider 成本占 95%+,优化重点是 cache hit rate 和模型选型 |

**反目标**：
- 不为了"延迟极致"过度优化（与 LLM 的几秒延迟相比，节省 100µs 没意义）
- 不在没数据的情况下做"性能优化"——所有优化必须基于 profiling
- 不堆机器解决架构问题——10x 资源解决不了的瓶颈，应该改设计
- 不为了 benchmark 数字牺牲可读性

---

## 2. 性能 SLO

### 2.1 用户可见的延迟

| 操作 | P50 目标 | P99 目标 | P99.9 目标 |
|---|---|---|---|
| Cache hit (L1) | < 2ms | < 10ms | < 30ms |
| Cache hit (L2) | < 10ms | < 50ms | < 200ms |
| LLM call TTFT | < 1s | < 3s | < 10s |
| LLM call total (短输出) | < 5s | < 30s | < 60s |
| LLM call total (长输出) | < 30s | < 120s | < 300s |
| Tool call (本地) | < 100ms | < 500ms | < 2s |
| Tool call (MCP subprocess, warm) | < 50ms | < 200ms | < 1s |
| Tool call (MCP subprocess, cold) | < 500ms | < 2s | < 5s |
| Trajectory submit → first event | < 100ms | < 500ms | < 2s |

### 2.2 Middleware 内部预算

每层目标延迟，加起来不超过 10ms（P99）：

| 层 | P99 预算 |
|---|---|
| Telemetry (span 创建) | < 0.5ms |
| Auth (cached principal) | < 0.5ms |
| IAM (cached scope eval) | < 1ms |
| Budget check (Redis Lua) | < 2ms |
| Cache lookup (L1 miss + L2 query) | < 3ms |
| Prompt Guard fast lane (aho-corasick) | < 1ms |
| Routing (table lookup) | < 0.5ms |
| Circuit breaker check | < 0.5ms |
| Retry wrap | < 0.5ms |
| **Sum** | **< 10ms** |

Prompt Guard slow lane (ONNX) 不在串行路径上 (Doc 02 §4.5 并行),不计入。

### 2.3 吞吐量 SLO

| 维度 | Personal | Team (单实例) | Team (3 副本) | SaaS |
|---|---|---|---|---|
| 并发 trajectory | 5 | 200 | 600 | 10000+ |
| LLM call QPS | 5 | 100 | 300 | 5000+ |
| Cache lookup QPS | 50 | 2000 | 6000 | 50000+ |
| Event write QPS | 50 | 1500 | 4500 | 30000+ |
| Tool invocation QPS | 5 | 500 | 1500 | 10000+ |

---

## 3. 瓶颈分析

理解每层的瓶颈才知道往哪扩容。从外到内分析典型 LLM 请求：

### 3.1 延迟构成

```
Cache miss + LLM 调用的典型 P50 时间分解 (10s 总延迟):

User Input
   │
   │  ↓ <1ms   axum handler 解析 + auth
   │
Middleware Stack
   │  ↓ <10ms  auth/iam/budget/cache_lookup (miss)/guard fast/routing
   │
Provider Adapter
   │  ↓ <5ms   prompt 拼装 + reqwest send
   │
Network → LLM Provider
   │  ↓ ~200ms  TLS handshake + 跨区网络 + provider queue
   │
LLM Inference
   │  ↓ 200-1000ms  TTFT (provider 自身计算)
   │
   │  ↓ 5-30s    剩余 token streaming
   │
Network → Runtime
   │  ↓ <10ms   stream 接收
   │
Middleware (出站)
   │  ↓ <5ms    schema_validation / cache_write_async / cost_accounting
   │
Response → User
```

**核心观察**：
- 90%+ 时间在 Provider (network + inference)
- 我们能控制的部分 < 5%
- 性能优化的真正杠杆是 **避免调用** (cache hit) 或 **选更快的 Provider** (model tier)

### 3.2 单实例瓶颈递进

随着 QPS 增加,瓶颈依次出现：

| QPS | 主瓶颈 | 缓解 |
|---|---|---|
| < 10 | 无 | - |
| 10-100 | Postgres 写入 (event log) | Batched insert + async write |
| 100-500 | Redis 命令 RTT | Pipelining + 连接池调大 |
| 500-1000 | tokio runtime 调度 | 增加 worker_threads |
| 1000-5000 | Provider rate limit | Provider 副本 (多账号) + Routing 分散 |
| > 5000 | 单实例文件句柄 / 网络 | 横向扩展 (多副本) |

### 3.3 容量瓶颈

| 资源 | 单实例上限 | 横向扩展可解 |
|---|---|---|
| CPU | 16 核 | ✅ |
| 内存 | 32 GB | ✅ |
| 文件句柄 | 65535 (per process) | ✅ |
| Postgres 连接 | 100-500 (含池) | ❌ (DB 是中心) |
| Redis 连接 | 10000+ | ❌ |
| Provider QPS | 由 provider 决定 | 部分 (多账号 + routing) |
| Provider TPM | 由 provider 决定 | 部分 |
| LLM 月成本 | 由预算决定 | ❌ |

中心化资源（Postgres / Redis / Provider 配额）需要单独扩容策略，不是加副本就行。

---

## 4. 资源 sizing

### 4.1 应用实例

```
Baseline (空闲):
  - CPU: 0.1 vCPU
  - RAM: 200 MB

Per concurrent trajectory:
  - CPU: 0.02 vCPU (大部分时间在 await)
  - RAM: 50-200 KB (state) + 0-2 MB (in-flight prompt buffer)

Per session (idle):
  - RAM: 10-50 KB

Per CLI subprocess (Doc 01 §6.2):
  - CPU: 0.2 vCPU (claude CLI 单进程)
  - RAM: 100-300 MB
  - File handles: ~20

Per MCP subprocess (Doc 05 §5.3):
  - CPU: 0.05-0.2 vCPU (取决于 server 实现)
  - RAM: 30-150 MB
  - File handles: ~10
```

**估算公式** (单实例)：

```
Required vCPU = 1 + (concurrent_trajectories × 0.02) 
              + (CLI_sessions × 0.2) 
              + (MCP_sessions × 0.1)

Required RAM (MB) = 500 + (concurrent_trajectories × 0.5)
                  + (idle_sessions × 0.03)  
                  + (CLI_sessions × 200)
                  + (MCP_sessions × 100)
```

实例规格示例：

| 实例规格 | 适合场景 | 大致承载 |
|---|---|---|
| 1 vCPU + 2GB | Personal mode | 5-10 trajectory + 2 CLI |
| 2 vCPU + 4GB | 小团队 (10-20 人) | 50 trajectory + 5 CLI + 10 MCP |
| 4 vCPU + 8GB | 团队 (50 人) | 200 trajectory + 20 CLI + 30 MCP |
| 8 vCPU + 16GB | 企业 (200 人) | 500 trajectory + 50 CLI + 80 MCP |
| 16 vCPU + 32GB | 大企业单实例上限 | 1000 trajectory |
| > 16 vCPU | 多副本横向扩展 | - |

### 4.2 Postgres

```
Baseline:
  - CPU: 2 vCPU
  - RAM: 4 GB
  - Disk: 50 GB SSD (含 30 天热数据 + WAL)

Per 1000 concurrent trajectories:
  - CPU: +1 vCPU (写入 + 索引维护)
  - RAM: +2 GB (shared_buffers + cache)
  - Disk IOPS: +500 sustained

Per million daily events:
  - Disk: +12 GB (含索引)
```

推荐实例：

| 规模 | 实例 | 备注 |
|---|---|---|
| < 100 并发 | db.t4g.large (2c/8GB) | 起步,burst 模式 OK |
| 100-500 并发 | db.r6g.large (2c/16GB) | RAM 优先 |
| 500-2000 并发 | db.r6g.xlarge (4c/32GB) | + read replica |
| > 2000 并发 | db.r6g.2xlarge + 2-3 read replica | + 分库 (按 tenant range) |

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

推荐实例：

| 规模 | 实例 | 备注 |
|---|---|---|
| < 100 并发 | cache.t4g.small (1c/2GB) | 起步 |
| 100-1000 并发 | cache.r6g.large (2c/16GB) | 单节点足够 |
| > 1000 并发 | Redis Cluster (3-6 sharded) | 高可用 + 容量 |

### 4.4 OTel Collector / 监控栈

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

通常 OTel Collector 走 sidecar 模式（与 application 同 pod），不需要单独扩容。

---

## 5. 并发模型

### 5.1 Tokio runtime 配置

```rust
fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get())          // 默认 = CPU 核数
        .max_blocking_threads(512)                 // 阻塞任务池
        .thread_name("tars-worker")
        .thread_stack_size(2 * 1024 * 1024)        // 2MB 默认够用
        .enable_all()
        .build()
        .unwrap();
    
    rt.block_on(async_main());
}
```

关键：
- **CPU 密集任务必须 spawn_blocking** (例如 ONNX 推理 / sha256 大数据)
- **网络 IO 全部 async** (reqwest / sqlx / redis-rs 都原生 async)
- **不要在 async 里持有 std::sync::Mutex 跨 await** (会阻塞 worker)

### 5.2 Backpressure 策略

每层都有"队列上限",超过就拒绝而非堆积：

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

应用场景：
- HTTP handler 入口：max 1000 并发请求,超过返回 503
- LLM provider 调用：每个 provider max N 个 inflight (按 provider quota)
- Tool subprocess：max M 个 active subprocess (避免文件句柄耗尽)
- Postgres 池：max P 个连接,超过等待但有 timeout

### 5.3 Channel 容量

```rust
// MPSC channel 必须 bounded,否则慢消费者会 OOM
let (tx, rx) = mpsc::channel(1024);

// Broadcast channel 同上
let (tx, _) = broadcast::channel(256);

// 慢消费者处理:
match rx.recv().await {
    Ok(msg) => ...,
    Err(broadcast::error::RecvError::Lagged(n)) => {
        // 跳过了 n 条消息,记 metric + 警告
        tracing::warn!(skipped = n, "subscriber lagged");
    }
    Err(_) => break,
}
```

### 5.4 LLM Streaming 的特殊处理

LLM 流式响应的 backpressure：

```rust
// inner_stream 是 Provider 返回的 BoxStream<ChatEvent>
// outer_consumer 是上层 Frontend Adapter

let stream = inner_stream
    .ready_chunks(10)              // 批量,降低跨 await 开销
    .timeout_at(deadline)          // 整体超时
    .take_until(cancel.cancelled());

while let Some(chunk) = stream.next().await {
    // 如果消费者慢 (UI 渲染卡顿),不能堆积
    if outbound_tx.try_send(chunk).is_err() {
        // outbound 满 → 跳过 / 合并
        metrics.record_stream_drop();
    }
}
```

---

## 6. Cache 性能与 ROI

### 6.1 Cache hit rate 的经济意义

```
单次 LLM 调用成本:    $0.05 (示例,gpt-4o 平均)
Cache hit 边际成本:   $0.0001 (Redis 查询)
Hit rate 节省:
  20% hit rate:  节省 20% × $0.05 = $0.01/req
  50% hit rate:  节省 ~$0.025/req
  80% hit rate:  节省 ~$0.04/req
```

100k req/天 × 50% hit rate = 节省 $2500/天 = $75k/月。**这是 cache 工作的核心 ROI**。

### 6.2 Cache hit rate 监控

按 tenant / provider / model_tier 切片：

```
Metric: llm.cache.hit_rate{tenant, provider, level}
Alert: hit_rate < 30% for 1h → SRE 调查
       (可能是 PromptBuilder 不稳定 / hasher_version 刚 bump / 真没复用机会)
```

### 6.3 L3 explicit cache 的成本边界

详见 Doc 03 §10.5 + 我们之前的成本测算：

```
L3 cache 经济模型:
  挂机费: $1/小时/100k tokens (Gemini 估值)
  调用降价: input cost × 25%
  
  盈亏平衡: 1 小时内调用次数 N 满足
    N × original_input_cost × 0.75 > $1
  
  对 100k token prefix:
    original_input_cost = 100k × $1.25/M = $0.125/调用
    破净 N = 1 / (0.125 × 0.75) ≈ 11 次/小时
```

Janitor (Doc 03 §8) 必须监控每个 L3 handle 的实际调用频率,低于 break-even 阈值的主动 delete。

### 6.4 Cache 性能基准

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

目标：L1 hit < 100µs (内存查找 + clone)，L2 hit < 5ms (Redis RTT)。

---

## 7. 横向扩展

### 7.1 应用实例的 stateless 性

应用实例必须是 stateless：

- 所有持久状态在 Postgres / Redis / S3
- 内存 cache (L1) 是 best-effort,失败时 fallback 到 L2
- 子进程 (CLI / MCP) 是实例 local 的,但通过 session id 路由保证粘性

### 7.2 Session affinity (粘性路由)

某些场景需要 session 粘性：
- CLI subprocess (Claude / Gemini) 在某个实例创建后,后续同 session 请求最好回到同实例 (复用 subprocess)
- 否则每个实例都建一份 subprocess,文件句柄爆炸

实现：
- LB 层基于 `session_id` hash 做 sticky routing (HAProxy / Envoy)
- 实例 down 时 session migration: 标记 dirty + 下次请求新实例重建

### 7.3 自动扩缩容

```yaml
# K8s HPA 示例
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
    # 自定义: 基于 backpressure 拒绝率扩容
    - type: Pods
      pods:
        metric:
          name: backpressure_rejection_rate
        target:
          type: Value
          value: "0.01"        # 拒绝率 > 1% 立即扩
```

**关键**：
- 扩容时机要早 (60% CPU 而不是 80%)，因为 LLM 任务长，新实例 warm-up 慢
- 缩容要慢 (10 min 稳定后才缩),避免 oscillation
- 缩容前 drain：标记 SIGTERM → 拒绝新请求 → 等 in-flight 完成 → exit

### 7.4 中心组件 (Postgres / Redis) 扩展

不能水平加副本解决,必须：

**Postgres**：
1. **Read replica**：读流量分散
2. **按 tenant 分库**：超大租户独立 cluster
3. **冷热分层** (Doc 09 §6)：hot 在 OLTP,cold 在 S3
4. **Citus / 分布式 Postgres**：超大规模 (10k+ tenant) 时

**Redis**：
1. **Redis Cluster**：原生 sharding
2. **多实例分功能**：cache / budget / pubsub 拆开

---

## 8. 成本优化

### 8.1 成本结构

```
典型 SaaS 部署成本占比:
  - LLM Provider API:      85-95%   ← 真正的钱在这
  - Compute (实例):         3-8%
  - Storage (DB + S3):      1-3%
  - Network (egress):       1-2%
  - Observability backend:  1-2%
```

LLM cost 完全主导。所有成本优化聚焦于此。

### 8.2 LLM 成本优化的杠杆

按效果排序：

1. **Cache hit rate** (§6)：直接对应金额节省,目标 > 50%
2. **Model tier 优化** (Doc 04 §4.3)：用小模型干小活,reasoning tier 严格按需
3. **Prompt 长度优化**：减少 RAG context / 历史 / tools schema 冗余
4. **Structured Output 替代多轮**：strict schema 一次出结果,避免来回澄清
5. **L3 explicit cache** (Doc 03)：长 system prompt 复用
6. **Speculative decoding (provider 侧)**：让 provider 用小模型预测加速
7. **Provider 套利**：同档不同 provider 价格差 2-5x,Routing 选便宜的

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
  claude-opus-4-7:            $215.40 (66%)  ← 最贵
  gpt-4o:                     $67.30 (21%)
  gemini-2.5-flash:           $42.32 (13%)

LLM Breakdown by Skill:
  code-review-deep:           $189.20 (58%)  ← 优化重点
  security-audit:             $98.42 (30%)
  doc-summarize:              $37.40 (12%)

Optimization Opportunities:
  ⚠ code-review-deep: cache hit rate 12%, could be 40%+
    → Stabilize PromptBuilder.static_prefix
  ⚠ security-audit: 80% calls use reasoning tier, only 30% need it
    → Move simple checks to default tier
  ✓ doc-summarize: cache hit rate 76%, well optimized
```

### 8.4 自动降级

```rust
pub struct CostBasedRouter {
    base_router: Arc<dyn RoutingPolicy>,
    cost_threshold: f64,
}

impl RoutingPolicy for CostBasedRouter {
    async fn select(&self, req: &ChatRequest, ...) -> Result<Vec<ProviderId>, _> {
        let tenant_remaining = self.budget.remaining(&ctx.tenant_id).await?;
        let estimated = estimate_cost(req);
        
        // 预算紧张时降档
        if tenant_remaining < estimated * 10.0 {
            // 把 reasoning tier 降级为 default tier
            let downgraded_req = req.with_tier(ModelTier::Default);
            return self.base_router.select(&downgraded_req, ...).await;
        }
        
        self.base_router.select(req, ...).await
    }
}
```

---

## 9. 压测方法论

### 9.1 压测工具链

```rust
// criterion - micro benchmarks
[dev-dependencies]
criterion = "0.5"
divan = "0.1"

// 端到端 - k6 / locust / vegeta
```

### 9.2 三种压测目标

| 类型 | 目标 | 工具 |
|---|---|---|
| Micro benchmark | 单函数性能,catch 回归 | criterion / divan |
| Component benchmark | 单组件 (e.g., CacheRegistry) | criterion + mock dependencies |
| End-to-end load test | 整体吞吐与延迟 | k6 + 真实 staging |

### 9.3 Micro benchmark 集

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

### 9.4 端到端压测脚本 (k6)

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

### 9.5 真实流量 shadow 测试

生产流量 mirror 到 staging：

```
Production:
  User → Production Cluster → Real LLM calls
                ↓ async mirror
  Staging:
  Mirrored requests → Staging Cluster → Mock LLM (same delay distribution)
```

捕获生产流量的形状（请求间隔、payload 大小分布、tenant 分布），在 staging 重放。Mock LLM 用历史响应回放，避免实际花钱。

---

## 10. 性能回归检测

### 10.1 CI 集成

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
    alert-threshold: '110%'         # 慢 10% 就 fail
    comment-on-alert: true
```

### 10.2 持续 baseline 跟踪

每个 main 分支提交都跑 benchmark,结果存到 GitHub Pages 上的 dashboard。趋势图明显恶化触发调查。

### 10.3 Production-like benchmark

```rust
// 不只跑 micro benchmark,还要跑接近真实场景的 e2e bench
fn bench_full_request_path(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pipeline = runtime.block_on(setup_realistic_pipeline());
    
    c.bench_function("full_request.cache_hit", |b| {
        // 预热 cache
        runtime.block_on(prime_cache(&pipeline, &test_request()));
        
        b.to_async(&runtime).iter(|| async {
            pipeline.execute(test_request()).await.unwrap()
        });
    });
}
```

---

## 11. Hot spot 与异常流量

### 11.1 单租户暴增

某租户 CI 流水线突然跑 1000 个并发 PR review → 单租户消耗超出 quota → 影响其他租户？

**对策**：
- 每租户独立 BoundedExecutor (max concurrent per tenant)
- Postgres 连接池按 tenant 配额 (避免单租户耗尽全池)
- 异常增长触发自动降级 (改用更便宜的 model tier)

### 11.2 单 trajectory 长尾

某 trajectory 已经跑 30 分钟,占用资源不释放：

**对策**：
- TaskBudget.max_wall_clock 硬上限 (Doc 04 §8)
- 超时强制 cancel + 写入 partial result
- 超长任务的 metric 单独追踪,异常多触发产品讨论 (任务设计问题?)

### 11.3 Cache 雪崩

某流行 prompt 的 cache 同时过期 → 成百上千请求同时 miss → 全部打到 Provider：

**对策**：
- Singleflight (Doc 03 §6)：同 key 并发 miss 只允许一次穿透
- Cache TTL 加 jitter (避免同时过期)
- Provider 端 rate limit 触发时 fail-fast (circuit breaker)

### 11.4 Provider 抖动

某 Provider 突然延迟翻倍：

**对策**：
- Circuit breaker 触发 (Doc 02 §4.7)
- Routing 自动切换到 fallback provider
- TTFT 异常告警 (基于 EWMA baseline)

---

## 12. Profiling 工具链

### 12.1 持续 profiling

```rust
// pprof crate 集成
[dependencies]
pprof = { version = "0.x", features = ["protobuf-codec", "flamegraph"] }

// 在 dev / staging 启用
#[cfg(feature = "pprof")]
fn start_pprof_server(port: u16) {
    tokio::spawn(async move {
        let server = axum::Router::new()
            .route("/debug/pprof/profile", get(pprof_handler));
        // ...
    });
}
```

生产环境用 `Pyroscope` / `Parca` 持续 sample,找出 CPU / 内存热点。

### 12.2 异步任务 profiling

`tokio-console` 用于实时观察 async task 状态：

```rust
console_subscriber::init();   // 在 dev / staging
```

```bash
tokio-console http://staging.tars.internal:6669
```

可看到：
- 每个 task 的 idle / busy / poll 时间
- 阻塞 worker 的 task
- channel 满 / 空的状况

### 12.3 内存 profiling

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

## 13. 反模式清单

1. **不要在没 profiling 数据的情况下做"性能优化"**——你猜的瓶颈往往不是真的。
2. **不要在 async 里持有同步锁跨 await**——会阻塞整个 worker。
3. **不要用 unbounded channel**——慢消费者会 OOM。
4. **不要在每次请求加载 tokenizer / 解析配置**——预加载 + Arc 共享。
5. **不要在 hot path 做 syscall (open/stat/read)**——文件 IO 必须 async + cached。
6. **不要为了"省连接"用单 Postgres 连接**——并发竞争反而更慢,池化才是正解。
7. **不要让 Backpressure 失败时返回 5xx**——应该 429 + Retry-After。
8. **不要让 graceful shutdown 等待所有 in-flight**——给硬上限 (例如 60s),超过就 kill。
9. **不要在 metric label 里放高基数字段**——见 Doc 08 §5.5。
10. **不要在压测时用真实 LLM API**——花钱无止境,用 mock + 真实延迟分布。
11. **不要把 micro benchmark 当生产性能预测**——E2E 压测才能反映真实负载。
12. **不要忽略 P99 / P99.9**——平均值掩盖真问题。
13. **不要让 cache 过期时间统一**——加 jitter 避免雪崩。
14. **不要让单租户能耗尽全局资源**——每租户独立 quota。
15. **不要在生产开 DEBUG log**——log IO 可能比业务慢。

---

## 14. 与上下游的契约

### 上游 (调用方) 承诺

- 遵守 rate limit / quota,触发 429 时实施退避
- 不跑无意义的轮询 (用 SSE / WebSocket)
- 大请求适当分页

### 下游 (Provider / DB / 监控) 契约

- Provider: 公布 rate limit + 文档化 typical latency
- DB: 连接池正确配置,慢查询监控启用
- 监控: 实时聚合,query latency < 5s

### 跨 SLO 契约

- 应用 SLO (本文 §2) 是与外部承诺的契约
- 内部 budget (§2.2) 是各团队 / 各组件的内部目标
- 内部 budget 总和应 < 应用 SLO,留出余量

---

## 15. 待办与开放问题

- [ ] LLM provider 调用的 connection reuse 优化 (HTTP/2 multiplexing)
- [ ] 大文件 ContentRef 上传到 S3 的并行度 (multipart upload threshold)
- [ ] Postgres 连接池的 prepared statement 缓存策略
- [ ] tokio runtime 的 worker_threads 调优实测 (vs CPU 核数)
- [ ] CLI subprocess pool 的 idle timeout 实测最优值
- [ ] 自适应 cache TTL (热点延长,冷数据缩短)
- [ ] LLM 响应流的 chunking 策略 (网络效率 vs latency)
- [ ] 端到端 trace + profile 关联 (从 trace 找到具体 hot 函数)
- [ ] 跨地域部署的延迟优化 (Provider region affinity)
- [ ] AI 辅助的容量规划 (LLM 读历史 metric 给 sizing 建议)
- [ ] **Speculative Execution**: LLM 流式生成 JSON action 时,基于早期 token 预热相关 cache / 预解析参数 schema / 预热子进程,target 100-300ms TTFT 收益 (需做 streaming JSON 增量解析 + 投机执行的取消逻辑)
