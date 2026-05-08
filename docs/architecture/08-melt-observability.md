# 文档 08 — MELT (Metrics / Events / Logs / Traces) 可观测性设计

> 范围：定义 Runtime 的可观测性数据架构——四支柱（M/E/L/T）的职责切分、采集、存储、查询、告警。
>
> 上下文：本文档与 Doc 04 §3.2 `AgentEvent`（事件溯源）、Doc 06 §10 `AuditLog`（合规审计）是**三个不同的数据流**，详见 §3 三者关系辨析。
>
> 横切：所有 Doc 01-07 组件都向 MELT 系统发数据，本文档定义"发什么 / 怎么发 / 存哪 / 怎么查"。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **四支柱职责清晰** | M/E/L/T 各有明确语义,不混用——选错支柱会导致成本失控或查询无效 |
| **LLM 成本一等指标** | token / dollar 是核心 metric,与 latency / errors 并列 |
| **租户级可隔离** | 所有信号都带 tenant_id label,允许按租户切片但不允许跨租户串味 |
| **零 payload 泄漏** | prompt / response / 代码内容**绝不进 MELT**,只走脱敏后的元数据 |
| **OTel 标准化** | 全栈 OpenTelemetry,允许接入任何符合 OTLP 的后端 |
| **采样可调** | LLM 调用昂贵,trace 全采会爆;按租户 / 错误状态 / 成本动态采样 |
| **Cardinality 受控** | label 维度有硬上限,防止 user_id / trace_id 进 metric label 导致 Prometheus 爆 |
| **按部署形态降级** | Personal 模式默认全关 telemetry;Team 模式全开;Hybrid 模式只发匿名指标 |

**反目标**：
- 不把 audit log（Doc 06 §10）和 MELT 混为一谈——audit 是合规要求，MELT 是运维需要
- 不把 AgentEvent（Doc 04 §3.2）当作 MELT 的 events 支柱——AgentEvent 是事件溯源，是业务真相，不能为了运维降采样
- 不在 MELT 系统里持久化 PII / 代码内容——遥测后端可能被未授权访问，污染源头
- 不让运维数据依赖业务存储（Postgres）——业务挂了仍要能看到"业务挂了"

---

## 2. 为什么是 MELT 而不是"日志"

把所有运维信号都塞进 log 是上一代 (pre-2015) 的反模式：
- 想看"过去 5 分钟错误率"？grep + awk 几 GB 的 log
- 想看"这个请求经过哪些服务"？文本搜 trace_id 拼凑
- 想看"哪个租户最贵"？再写个聚合脚本

每一类问题都有更适合的数据结构：

| 问题类型 | 最佳支柱 | 数据结构 |
|---|---|---|
| "过去 5 分钟 XX 的趋势" | Metrics | 时序聚合 (counter / gauge / histogram) |
| "刚发生了什么有意义的事" | Events | 离散结构化记录 (类型 + 字段) |
| "这次请求具体怎么处理的" | Logs | 时间序列文本 + 结构化字段 |
| "请求穿过了哪些组件" | Traces | 因果关系树 (span tree) |

四支柱**互相补充**，不是替代关系。一个 LLM 调用既应该产生 Metric（计入 token 总量）、Event（"调用完成"）、Log（详细上下文）、Trace（与 parent span 关联），各自承担不同的查询需求。

---

## 3. 三类数据流的辨析

```
┌─────────────────────────────────────────────────────────────────┐
│  Runtime (Doc 04)                                               │
│                                                                 │
│  Event Sourcing                Audit Log              MELT      │
│  (AgentEvent)                  (AuditEvent)           (M/E/L/T) │
│  Doc 04 §3.2                   Doc 06 §10             本文档    │
│  ↓                             ↓                      ↓         │
│  Postgres event_log            WORM / SIEM            OTel      │
│                                                                 │
│  目的:replay/recovery         目的:合规/法律           目的:运维 │
│  保证:完整性 (永不丢)         保证:不可篡改           保证:够用即可 │
│  保留:30 天热 + 1 年冷         保留:7 年              保留:30 天  │
│  采样:100% (业务真相)         采样:100% (合规要求)    采样:动态  │
└─────────────────────────────────────────────────────────────────┘
```

| 维度 | AgentEvent | AuditEvent | MELT |
|---|---|---|---|
| 谁读 | Runtime 自己 (recovery) | 法务 / 合规 / 监管 | SRE / 开发 / 产品 |
| 丢一条的代价 | 任务无法 replay,数据不一致 | 合规违规 | 监控不准,可接受 |
| 数据保真度 | 100% (含完整 payload via ContentRef) | 关键事件签名 | 元数据 + 摘要 |
| 是否可降采样 | ❌ | ❌ | ✅ |
| 写入路径 | 同步 (业务路径) | 异步双写 (业务关键) | 异步,best-effort |
| 失败处理 | abort 业务 | 阻塞业务 + 告警 | drop,告警但不影响业务 |

**关键不变量**：
- AgentEvent 落库失败 → trajectory 失败（Doc 04 不能继续）
- AuditEvent 落库失败 → 业务阻塞（Doc 06 合规底线）
- MELT 落库失败 → 业务继续，自身告警

不要为了"统一"把三者合并到一个存储——失败模式完全不同。

---

## 4. LLM 工作负载的独特挑战

传统 microservice 可观测性 → LLM 时代有几个新维度：

| 挑战 | 传统系统 | LLM 系统 |
|---|---|---|
| 单请求成本 | 微秒级 CPU,可忽略 | 几分到几美元,必须追 |
| 单请求耗时 | <100ms 是常态 | 几秒到几分钟正常 |
| 失败模式 | 4xx / 5xx HTTP | + 内容过滤 / 截断 / 幻觉 / 工具调用乱选 |
| 数据敏感性 | 业务字段需要脱敏 | prompt / response 整体即敏感 |
| 重试成本 | 几乎免费 | 每次重试再付钱 |
| 流式 | 罕见 | 默认 |

新增必采指标：
- **Token usage** (input / output / cached)
- **Cost USD** (按 model 定价计算)
- **TTFT** (Time To First Token)
- **Throughput** (Tokens / second)
- **Cache hit rate** (L1 / L2 / L3)
- **Stop reason distribution** (EndTurn / MaxTokens / ContentFilter / ToolUse)
- **Tool call success rate** (per tool)
- **Trajectory branch count** (replan 次数)
- **Compensation success rate** (回溯成功率)

---

## 5. Metrics 设计

### 5.1 类型分类

```rust
pub enum MetricType {
    Counter,            // 单调递增 (request total / errors total)
    UpDownCounter,      // 可增可减 (active sessions / inflight requests)
    Gauge,              // 瞬时值 (cache size / queue depth)
    Histogram,          // 分布 (latency / token usage)
    Summary,            // 客户端聚合的分位数 (用得少,优先 Histogram)
}
```

OTel metrics SDK 的核心抽象，所有 metric 必须显式声明类型。

### 5.2 命名规范

```
{domain}.{component}.{measure_name}
```

示例：
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

### 5.3 必须采集的 SLI

每个 SLO 都必须有对应 metric。最小集：

| SLI | Metric | SLO 示例 |
|---|---|---|
| 可用性 | `llm.provider.request_total` (success/total) | 99.5% |
| TTFT P95 | `llm.provider.ttft_ms` | < 2000ms |
| 完整请求 P95 | `llm.provider.total_latency_ms` | < 30000ms |
| 错误率 | `llm.provider.errors_total` / `request_total` | < 1% |
| 缓存命中率 | `llm.cache.hits_total` / lookups | > 30% |
| Tool 成功率 | `tool.invocation_total` (success/total) | > 99% |
| Trajectory 成功率 | `agent.trajectory.completed_total{status=success}` / total | > 95% |
| 预算超支率 | `budget.hard_limit_exceeded_total` / requests | < 0.1% |

### 5.4 LLM 成本指标的特殊处理

成本是双重维度——既是 metric，也是计费数据：

```rust
pub struct CostMetric {
    pub provider: ProviderId,
    pub model: String,
    pub tenant: TenantId,
    pub usage: Usage,
    pub cost_usd: f64,
    pub cache_savings_usd: f64,         // 比"如果不用 cache"省了多少
}
```

写入路径：
1. **Metrics**：`llm.cost_usd` Counter，labels 包含 provider / model / tenant
2. **Billing**：Postgres `billing_events` 表（Doc 06 §9.2），每条独立记录，可审计
3. **不进 logs**：成本数据通过 metric 已经够用，logs 里不需要重复

### 5.5 Cardinality 控制

Prometheus / OTel 的硬约束：每个 metric 的 (name + labels) 唯一组合数 = "时间序列数"。一个时序占内存 ~3KB，10万 系列就是 300MB。

**绝对禁止**进 metric label 的字段：

| 字段 | 原因 |
|---|---|
| `trace_id` | UUID 全局唯一,会爆掉 |
| `request_id` | 同上 |
| `user_id` | 用户多了就爆 |
| `session_id` | 短期但快速增长 |
| `prompt_hash` | hash 空间巨大 |
| `error_message` | 自由文本,变体无限 |
| `code_path` | 大型代码库路径数千 |

**允许**进 label 的字段（基数都是常量级）：

| 字段 | 典型基数 |
|---|---|
| `tenant_id` | 10²-10³ |
| `provider` | < 10 |
| `model` | < 50 (按 family 聚合,不放具体版本) |
| `model_tier` | < 10 |
| `tool_id` | < 100 |
| `agent_role` | < 20 |
| `error_class` | 5-10 (Permanent/Retriable/...) |
| `status` | < 10 |
| `cache_level` | 3 (l1/l2/l3) |
| `region` | < 20 |

校验：

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
            // 运行时检查:如果某 label 的实际值数已超阈值,reject
            if self.observed_cardinality(k) > self.cardinality_limits[k] {
                return Err(CardinalityError::CardinalityExceeded { label: k.into() });
            }
        }
        Ok(())
    }
}
```

启动时 wrapper Metric 注册器，运行期违规 metric 调用直接 panic（dev）/ 静默 drop + 告警（prod）。

### 5.6 Histogram bucket 选择

LLM 延迟跨度 3 个量级（10ms 到 100s），固定 bucket 不够用。建议指数 bucket：

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

## 6. Events 设计

**重要**：本节的 "Events" 不是 Doc 04 §3.2 的 `AgentEvent`（事件溯源），而是 MELT 四支柱中的离散业务事件——发到 OTel Logs (event 类型) 或独立 event bus，供 SRE / 产品分析。

### 6.1 何时该用 Event 而非 Metric

| 场景 | 用 Metric | 用 Event |
|---|---|---|
| "过去一小时多少次 X" | ✅ | ❌ |
| "X 的延迟分布" | ✅ | ❌ |
| "刚才那次 X 具体发生了什么" | ❌ | ✅ |
| "今天有过哪些异常租户" | ❌ | ✅ |
| "上周触发过 N 次的特殊事件" | ❌ | ✅ |

Event 的关键特性：**离散 + 结构化 + 可枚举**——预定义有限种类型，每种带固定字段。

### 6.2 必采事件类型

```rust
pub enum TelemetryEvent {
    // 关键业务事件 (与 AuditEvent 不同——这些不需要法律级保留)
    HighCostRequest { cost_usd: f64, threshold: f64, model: String, tenant: TenantId },
    UnusualLatency { latency_ms: u64, p99_baseline_ms: u64, provider: ProviderId },
    CircuitBreakerOpened { provider: ProviderId, failure_rate: f64 },
    CircuitBreakerClosed { provider: ProviderId },
    BudgetSoftLimitHit { tenant: TenantId, period: String, percent_used: f64 },
    
    // 缓存事件
    CacheStorageQuotaWarning { tenant: TenantId, percent_used: f64 },
    L3CacheCreated { handle_id: L3HandleId, size_bytes: u64, tenant: TenantId },
    L3CacheEvicted { handle_id: L3HandleId, reason: EvictionReason },
    
    // Agent 事件
    BacktrackTriggered { trajectory: TrajectoryId, reason: BacktrackReason },
    CompensationFailed { trajectory: TrajectoryId, compensation_id: CompensationId },
    HumanEscalationRequired { trajectory: TrajectoryId, reason: String },
    
    // 安全事件 (与 AuditEvent 重叠但更细)
    PromptInjectionDetected { detector: String, tenant: TenantId },
    UnusualToolPattern { tool: ToolId, count_per_minute: u32, tenant: TenantId },
    
    // 配置事件
    ConfigReloadCompleted { changes_count: u32 },
    SecretRotationCompleted { ref_count: u32 },
}
```

### 6.3 与 AgentEvent 的区分

| AgentEvent (Doc 04) | TelemetryEvent (本文) |
|---|---|
| 业务真相,replay 必需 | 运维快照,丢失可接受 |
| 100% 采集 | 可降采样 |
| 写 Postgres event_log | 写 OTel logs (event flag) / event bus |
| 任意 trajectory 的所有 step | 跨任意业务,运维关心的瞬间 |
| 字段是业务输入输出 | 字段是运维元数据 |

**两者关系**：某些 AgentEvent 会**派生**出 TelemetryEvent。例如 `AgentEvent::TrajectoryAbandoned` 可能派生出 `TelemetryEvent::BacktrackTriggered`（如果 reason 是 critic reject）和 `MetricUpdate("agent.backtrack_total")`。派生由专门的 `TelemetryProjector` 完成。

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

`TelemetryProjector` 在 Runtime 提交 AgentEvent 时同步运行（fast path），projection 结果异步发送（slow path 不阻塞业务）。

---

## 7. Logs 设计

### 7.1 结构化 only

绝不允许 `println!("user x did y, with z")` 这种字符串拼接日志。

```rust
// ❌ 错误
tracing::info!("User {} fetched {} bytes from {}", user_id, size, provider);

// ✅ 正确
tracing::info!(
    user.id = %user_id,
    payload.size_bytes = size,
    provider = %provider,
    "fetch_completed"
);
```

理由：grep `"fetch_completed"` 永远能找到所有此类记录；字符串拼接的话变体无穷无尽。

### 7.2 级别约定

| 级别 | 何时用 | 出现频率 |
|---|---|---|
| ERROR | 业务失败 / 不可恢复 | 罕见 (生产上每分钟个位数) |
| WARN | 可恢复但应注意 (重试 / fallback) | 偶尔 (每分钟几十) |
| INFO | 关键业务节点 (请求完成 / 配置加载) | 常见 (每秒几条) |
| DEBUG | 详细的内部状态 | 仅 dev / 临时排查 |
| TRACE | 极细粒度 (函数级) | 仅 dev / profiling |

生产环境默认 INFO，不允许默认 DEBUG（log 量爆炸 + 敏感数据风险）。

### 7.3 必带字段

每条 log 自动注入：

```rust
// 通过 tracing::Span 的 record 机制
#[instrument(
    fields(
        trace_id = %ctx.trace_id,
        tenant = %ctx.tenant_id,
        session = %ctx.session_id,
        principal = %ctx.principal,
    )
)]
async fn handle_request(ctx: RequestContext, ...) -> ... {
    // 该函数体内所有 log 自动携带上述字段
}
```

不需要每条 log 重复写 `tenant=...`——通过 tracing span context 继承。

### 7.4 敏感数据脱敏

绝对不能进 log 的内容：

| 内容 | 替代方案 |
|---|---|
| 完整 prompt | log `prompt.hash`, `prompt.token_count`, `prompt.system.role` |
| LLM raw response | log `response.token_count`, `response.stop_reason`, `response.has_tool_calls` |
| 用户代码 | 只 log 文件路径 + 行数,不 log 内容 |
| API key / secret | 永不 log (即使脱敏也别 log) |
| Email / 手机号 | hash 后 log,或不 log |
| Tool 调用的具体参数 | log `tool.id`, `args.size_bytes`, `args.field_count`, 不 log 值 |

实现：用 `tracing` 的 field formatter 包装：

```rust
pub struct SensitiveString(String);

impl fmt::Display for SensitiveString {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<redacted:{}>", self.0.len())
    }
}

// 使用
tracing::info!(
    user.email = %SensitiveString(email),  // log 出 "<redacted:32>"
    ...
);
```

### 7.5 日志聚合

```rust
// 启动时配置
tracing_subscriber::registry()
    .with(EnvFilter::from_default_env())
    .with(tracing_subscriber::fmt::layer().json())   // JSON 格式
    .with(OpenTelemetryLayer::new(otel_tracer))      // 同时发 OTel
    .init();
```

JSON 格式确保 log 可被 Loki / Elasticsearch / Datadog 正确解析。

---

## 8. Traces 设计

### 8.1 Span 设计原则

- **粗粒度优先**：一个请求顶层 span + 几个关键子 span，不要给每个函数加 span
- **跨进程必须传播**：CLI subprocess、MCP server、OTel collector 之间通过 OTel context 传 trace_id
- **失败必有 record_error**：`span.record_error(&e)` 让 trace 能 filter 出错请求

### 8.2 Span 树形态

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
│  └─ guard.slow_lane                (并行,作为兄弟 span)
├─ middleware.routing
├─ middleware.circuit_breaker.check
├─ middleware.retry
│  └─ provider.openai.stream         (实际 LLM 调用)
│     ├─ http.request                (reqwest auto-instrumented)
│     └─ sse.parse
├─ middleware.cache.write            (出站,异步)
├─ middleware.budget.commit
└─ middleware.telemetry.finalize
```

每个 span 必须有：
- `name`: 见上图
- `kind`: Internal / Client / Server
- `attributes`: 带业务标识 (tenant / provider / model)
- `events`: 关键时刻 (cache hit / retry / circuit open)

### 8.3 Trajectory tree → span tree

Doc 04 的 trajectory tree 在 trace 中的表现：

```
task.run                             (root)
├─ agent.orchestrator.execute
│  └─ llm.invoke (model=fast)
├─ agent.worker.security.execute     (并行 sibling)
│  ├─ tool.invoke (id=git.fetch_pr_diff)
│  ├─ tool.invoke (id=sast.run_semgrep)
│  └─ llm.invoke (model=reasoning)
├─ agent.worker.perf.execute         (并行 sibling)
│  └─ llm.invoke (model=reasoning)
├─ agent.aggregator.execute
└─ agent.critic.execute
   └─ llm.invoke (model=default)
```

被 critic reject 触发 backtrack 时，新 trajectory 是新 span tree（用 link 关联到 parent trajectory 的 root span，不是 child span）。

### 8.4 Sampling

LLM 调用动辄几秒几十秒，全采 trace 会让 collector 爆。分层采样：

```rust
pub enum SamplingDecision {
    AlwaysSample,          // 关键事件:错误 / 高成本 / 安全告警
    PerTenant { rate: f64 },  // 按租户基础采样率
    HeadBased { rate: f64 },   // 入口决策
    TailBased,                 // 收完整个 trace 后再决定 (用于 cost-based 采样)
}

pub struct AdaptiveSampler {
    base_rate: f64,                           // 默认 0.1 (10%)
    always_sample_predicates: Vec<Predicate>,
}

impl Sampler for AdaptiveSampler {
    fn should_sample(&self, ctx: &SamplingContext) -> SamplingDecision {
        // 永远采样的情况
        if ctx.has_error() { return AlwaysSample; }
        if ctx.cost_usd > 1.0 { return AlwaysSample; }
        if ctx.tenant_priority == Priority::Premium { return AlwaysSample; }
        if ctx.is_security_event() { return AlwaysSample; }
        
        // 默认随机
        if rand::random::<f64>() < self.base_rate {
            AlwaysSample
        } else {
            SamplingDecision::Drop
        }
    }
}
```

### 8.5 Tail-based sampling

某些场景需要看完整个 trace 才能决定要不要存：

- "找所有最终失败但中途经过 retry 的请求"
- "找 P99 慢请求的样本"
- "找消耗超过 $0.50 的请求"

实现：OTel Collector 配置 `tailsamplingprocessor`：

```yaml
processors:
  tail_sampling:
    decision_wait: 30s              # 等 trace 完成
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

## 9. OpenTelemetry 集成

### 9.1 全栈 OTel

所有 metrics / events / logs / traces 都走 OTel SDK：

```rust
// Cargo.toml
[dependencies]
opentelemetry = "0.x"
opentelemetry_sdk = "0.x"
opentelemetry-otlp = "0.x"
opentelemetry-prometheus = "0.x"  # 可选,Prometheus 抓取
tracing-opentelemetry = "0.x"
```

```rust
// 启动初始化
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
    
    // Logs (作为 OTel signal,不只是 stdout JSON)
    let logger_provider = opentelemetry_otlp::new_pipeline()
        .logging()
        .with_exporter(otel_exporter(&config.otlp_endpoint))
        .with_resource(resource)
        .install_batch(runtime::Tokio)?;
    
    // 全局注册
    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider);
    
    // tracing crate bridge
    let tracing_layer = tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("tars"));
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().json())
        .with(tracing_layer)
        .init();
    
    Ok(TelemetryGuard { /* drop 时 flush */ })
}
```

### 9.2 后端选型

OTLP 协议保证 backend 无关，可以接：

| Backend | 适合场景 |
|---|---|
| **Self-hosted Prometheus + Tempo + Loki + Grafana** | Team 模式,完全自主 |
| **OpenObserve / SigNoz / Uptrace** | 单一开源后端,运维简单 |
| **Datadog / New Relic / Honeycomb** | SaaS,买便利 |
| **Grafana Cloud** | Self-hosted 体验 + 托管 |
| **VictoriaMetrics + VictoriaLogs + Jaeger** | 高性能开源组合 |
| **AWS / GCP / Azure 原生** | 云上一体化 |

**Personal 模式**：默认本地 SQLite 存少量 metric，不发送任何 OTel；用户可 opt-in 本地 Grafana / Honeycomb sandbox。

### 9.3 Collector 部署

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

Collector 的好处：
- 应用只发一份 OTLP，不需要知道下游
- 集中配置 sampling / batching / 限流
- Backend 切换不重启应用

---

## 10. 采样策略汇总

| 信号 | 默认采样 | 按需提升 |
|---|---|---|
| Metrics | 100% (聚合后体积小) | N/A |
| Events | 100% | N/A |
| Logs (INFO+) | 100% | DEBUG 只在 trace_id matched 时开启 |
| Logs (DEBUG) | 0% | 通过 dynamic log level 临时开启 |
| Traces | 10% head + 100% tail-based (errors / slow / expensive) | 调试时 tenant 级强制 100% |

动态调节通过 ConfigManager 热加载（Doc 06 §6）：

```toml
[telemetry.sampling]
trace_base_rate = 0.1
trace_always_sample_on_error = true
trace_always_sample_above_cost_usd = 1.0
log_default_level = "info"

[telemetry.tenant_overrides.acme_corp]
trace_base_rate = 1.0     # 大客户 100% trace
log_default_level = "debug"
```

---

## 11. 隐私与脱敏（强制）

### 11.1 永不进 MELT 的内容

| 内容 | 强制规则 |
|---|---|
| Prompt 文本 | 只允许 hash + token_count + word_count |
| LLM 响应文本 | 只允许 hash + token_count + stop_reason |
| 用户代码 | 路径 + 行号 + AST 节点类型,绝不内容 |
| Tool args / output | 只允许 schema 字段名 + 大小,不允许值 |
| API key / token | 永不 (即使脱敏后 hash 也不) |
| PII (email / phone / 身份证) | 不允许; 必须输出时 SHA256 + 前 6 位 |
| 完整 stack trace | 函数名 + 行号 OK, 局部变量值不行 |

### 11.2 自动脱敏

通过类型系统强制：

```rust
/// 标记为"绝不能 log 原文"的字段
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

任何 log/trace/event 字段如果包含 `SecretField<T>`，输出永远是 `<secret>` —— 即使有人不小心写了 `tracing::info!(prompt = ?prompt)`。

### 11.3 启动期校验

CI 集成 lint：用 `clippy::missing_docs_in_private_items` 风格的自定义 lint 检查，所有 log/trace 调用的字段类型不能包含未脱敏的敏感字段。

```rust
// lint 规则示例 (concept,实际用 dylint / proc-macro)
// ❌ FAIL
tracing::info!(prompt = %prompt_string, "got prompt");

// ✅ PASS
tracing::info!(prompt = %PromptText(SecretField(prompt_string)), "got prompt");

// ✅ PASS (用元数据)
tracing::info!(prompt.hash = %hash(&prompt_string), prompt.tokens = token_count, "got prompt");
```

---

## 12. 存储与保留

### 12.1 存储分层

| 层 | 介质 | 保留期 | 用途 |
|---|---|---|---|
| Hot | Prometheus / Tempo / Loki | 7-15 天 | 实时查询、告警 |
| Warm | S3 / 对象存储 | 30 天 | 故障复盘、SLO 报告 |
| Cold | S3 Glacier | 1 年 | 合规、审计辅助 |

成本控制：
- Hot 层只保留必要的 metric / 已采样的 trace
- 跨层迁移自动化（Tempo retention policy / Loki object store）
- Personal 模式不保留任何（除非用户 opt-in）

### 12.2 删除契约

- 当 tenant 被 §6 删除时，MELT 系统中所有该 tenant 的数据**也必须删除**（GDPR 要求）
- 实现：tenant_id 是必带 label，按 label 删除（Loki / Tempo 都支持）
- 例外：聚合后已经匿名化的 metric（如 `llm.cost_usd_total{tenant="*"}`）可保留

---

## 13. 告警

### 13.1 告警源

告警规则跑在 Prometheus / Grafana / 类似系统上，不在应用内：

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

### 13.2 SLO 与 error budget

```yaml
# Sloth / OpenSLO 风格
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

错误预算耗尽时自动触发：暂停部署 / 提高优先级 / 通知产品负责人。

---

## 14. 按部署形态的差异化

| 形态 | Metrics | Events | Logs | Traces | OTel Endpoint |
|---|---|---|---|---|---|
| Personal | 本地 SQLite, 仅核心指标 | 关闭 | stdout JSON | 关闭 | 不启用 |
| Personal (opt-in) | 同上 | 启用 | 同上 | 10% sampling | 用户配置的 endpoint |
| Team | 全开 | 全开 | 全开 (INFO+) | 10% sampling | 客户 OTel Collector |
| SaaS | 全开 + 计费聚合 | 全开 | 全开 (INFO+) | 10% sampling + 完整 tail-based | 厂商 OTel Collector |
| Hybrid | 本地全开 | 本地全开 | 本地全开 | 本地全开 | **本地** OTel Collector;**云端**只收匿名聚合 |

Personal 模式默认全关的理由：
- 个人用户的隐私优先
- 个人开发场景不需要 SRE 级监控
- 任何外联都需要用户显式 opt-in（启动 banner 提示）

---

## 15. 测试策略

### 15.1 Cardinality 防退化

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

### 15.2 脱敏 lint

```rust
#[test]
fn no_log_call_passes_raw_secret_field() {
    // 用 syn 解析整个 codebase 的所有 tracing!() 调用
    // 检查每个 field 的类型不能是 String/&str/直接的 Prompt/Code 类型
    // 必须是 SecretField 包装或基础类型 (u64 / hash 等)
    let violations = scan_codebase_for_unsafe_log_calls("./src");
    assert_eq!(violations, vec![], "found {} unsafe log calls", violations.len());
}
```

### 15.3 Trace 完整性

```rust
#[tokio::test]
async fn full_request_produces_expected_span_tree() {
    let runtime = test_runtime_with_otel_capture();
    let task = runtime.submit(test_spec(), test_principal()).await.unwrap();
    runtime.wait_until_done(task).await;
    
    let spans = runtime.captured_spans();
    let tree = build_span_tree(&spans);
    
    // 验证关键 span 存在
    assert!(tree.find("middleware.iam").is_some());
    assert!(tree.find("middleware.cache.lookup").is_some());
    assert!(tree.find("provider.openai.stream").is_some());
    
    // 验证因果关系
    assert!(tree.is_ancestor("middleware.cache.lookup", "cache.l1.get"));
}
```

### 15.4 Sampling 测试

```rust
#[tokio::test]
async fn errors_always_sampled_regardless_of_base_rate() {
    let sampler = AdaptiveSampler::new(0.0);  // 基础 0%
    
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

## 16. 反模式清单

1. **不要把 audit log / AgentEvent / MELT 三者混用**——存储、保留、采样、失败处理完全不同。
2. **不要在 metric label 里放高基数字段**（trace_id / user_id / message）——会爆 Prometheus。
3. **不要在 log 里输出 prompt / response / 代码原文**——通过 SecretField 强制脱敏。
4. **不要为了"覆盖率"在每个函数加 span**——粗粒度优先，深 span tree 性能浪费且难读。
5. **不要让 telemetry 失败影响业务**——MELT 是 best-effort，drop + alert，不阻塞业务。
6. **不要在 Personal 模式默认开启外联 telemetry**——必须 opt-in。
7. **不要用文本 grep 替代结构化 log**——log 必须 JSON / OTel。
8. **不要让告警规则跑在应用内**——规则在 Prometheus / Grafana / OpsGenie，应用只发数据。
9. **不要把成本指标只放 metric**——同时写 billing_events 表，metric 只是聚合视图。
10. **不要忽略 cardinality 增长**——定期 review 实际产出的时间序列数，超阈值告警。
11. **不要让 trace 100% 采样**——成本爆，用 head + tail 组合。
12. **不要在 hot path 上做同步 OTel 调用**——SDK 必须 batch + async export。
13. **不要重复采集**——同一指标在两个地方递增会导致 2x 计数。
14. **不要让 deleted tenant 的数据残留在 MELT**——GDPR 要求级联删除。
15. **不要假设所有后端都支持所有 OTel 特性**——Tempo 不支持复杂 metric query，Loki 不擅长高基数 log——选型时验证。

---

## 17. 与上下游的契约

### 上游 (Doc 01-07 各组件) 承诺

- 所有 instrumentation 通过 OTel SDK,不直接写 file / stdout
- 调用时携带正确的 RequestContext (含 trace_id / tenant_id)
- 敏感字段必须用 SecretField 包装
- 高频热点函数禁止 trace span (避免性能损失)

### 下游 (OTel Collector / Backend) 契约

- 接受 OTLP gRPC (4317) 或 HTTP (4318)
- 失败时让 SDK queue 起来重试,不要立即拒绝
- Backend 不能成为单点——SDK 配合 collector 的本地 disk buffer

### 跨支柱关联

- Metric 标签必须能与 trace attribute / log field 同名 (allow drill-down)
- Trace 的 trace_id 必须出现在所有相关 log 行
- Event 必须能根据 trace_id 关联到完整 trace

---

## 18. 待办与开放问题

- [ ] eBPF profiling 集成 (Pyroscope / Parca) 评估
- [ ] Cardinality 监控的 metric (recursive: 监控 metric 数量本身)
- [ ] 跨语言 tracing (Rust 主体 + Python MCP server)
- [ ] OTel SDK Rust 版本的成熟度 (vs Go / Java)
- [ ] 自定义 trace processor 的扩展点 (非常用情况下的额外 enrichment)
- [ ] Metric pre-aggregation (rollup) 在 Collector 还是应用层
- [ ] Personal 模式的本地 dashboard (内嵌 Grafana? 自己写?)
- [ ] 告警去重 / 抖动抑制 (同一问题不要 spam)
- [ ] 用户级 SLO 报告自动生成 (面向客户成功)
- [ ] AI 辅助根因分析 (LLM 读 metric + log 给 hypothesis)
