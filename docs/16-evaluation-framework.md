# 文档 16 — Evaluation Framework（评估框架）

> 范围：在 LLM 调用结束后产生**多维度的 dimension scores**，让 consumer 看到系统在多个语义维度上的行为（schema 合规率 / rubric grounding 率 / 指定字段填充率 / 幻觉率 / 等等），既可以**在线**（生产路径上每 call 后顺手出指标）也可以**离线**（dataset 跑一遍出 release report）。
>
> 上游：Doc 02 Middleware Pipeline 负责产出 `Response`；Doc 09 Storage Schema 中的 `tars-storage::EventStore` 是评估事件的持久化层。
>
> 下游：所有需要"看 LLM 系统行为趋势"的 consumer——ARC 的 dogfood 仪表板、agouflow、未来的 admin dashboard、release 实验对比、root-cause 调查。
>
> **明确不做的事**：本文档讨论的是**评估**（产指标、写时间序列、不影响 Response），不是**校验**（改 / 拒 Response）。这两件事在 Doc 15 已经明确切开。一个 dimension score 突然下降是诊断信号，不是 production gate；要做 gate 走 Doc 15 的 OutputValidator。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **指标突然掉 → 立即可调查** | 仪表板看到 schema_compliance 24h 移动平均跌 → 通过 `trace_id` 立即定位到具体 call → 看到原始 (request, response) → 定位 prompt / 模型 / config 的根因 |
| **同一份代码、三种部署模式** | Online（生产路径每 call 触发）/ Sync（caller 主动调拿即时分数）/ Offline（dataset 跑历史复盘）共用同一个 `Evaluator` trait |
| **Stream-first 架构** | Pipeline 不知道下游有谁订阅；Pipeline 只发 `LlmCallFinished` 事件到 EventStore；evaluators 是 EventStore 的消费者+生产者。**Pipeline 跟 evaluators 互不依赖**，加 evaluator 不动 Pipeline，删 evaluator 不影响生产 |
| **Determinitic-first** | 内置 evaluators 全部 deterministic 且 cheap（schema parse / 集合包含 / regex / counting）。LLM-as-judge 类作为 escape hatch、显式异步、可采样 |
| **Replayable** | 历史 EventStore dump 上跑同一个 evaluator → 得到当时的指标快照。"上周 prompt 改前后哪一类 dimension 翻盘了"是一句 SQL |
| **Cross-pipeline 天然聚合** | 多个 Pipeline（不同 provider / 不同 role）写到同一个 EventStore → 一个仪表板看全部，不需要每个 Pipeline 各自接 metric backend |
| **Dimension-first，反对单一"correctness"分数** | 借 Rotten Tomatoes 教训——一个 0.87 分告诉你不了什么。每维度独立打分、独立追踪、可复合 |

**反目标**：

- **不做仪表板 / UI** —— tars 提供数据（EventStore + 一组 SQL view 模板），UI 是 consumer 的事
- **不强加 metric backend**（Prometheus / Statsig / DataDog）—— EventStore 是 source of truth，往外推哪种 backend 写 exporter；exporter 不是 v1
- **不做 alerting** —— SQL 查询 + 阈值是 consumer 责任；tars 不内置规则引擎
- **不替代 Doc 15 validation** —— evaluation 不影响 Response，验证不影响仪表板。两者并存且不交互
- **不做 LLM-as-judge as built-in** —— 太贵、不确定、容易变成 anti-pattern。trait 支持，但 built-in 全 deterministic

---

## 2. 架构总览

```
                   ┌──────────────────────────────────────────────────┐
                   │  EventStore (append-only stream, source of truth)│
                   │   trace_id   ts_ms   event                       │
                   │   ─────────  ─────   ───────                     │
                   │   abc123     1000    LlmCallFinished{req,resp}   │
                   │   abc123     1003    EvaluationScored{dim,value} │ ← evaluator A 写
                   │   abc123     1005    EvaluationScored{dim,value} │ ← evaluator B 写
                   │   def456     2500    LlmCallFinished{...}        │
                   │   def456     2505    EvaluationScored{...}       │
                   └──────────────────────────────────────────────────┘
                       ▲                            ▲
                       │ 写                          │ 读 + 写
                       │                            │
        ┌──────────────┴──────────┐    ┌────────────┴───────────────┐
        │  Pipeline (producer)    │    │  EvaluatorRunner            │
        │   - 完成 call 后写       │    │   - 订阅 LlmCallFinished     │
        │     LlmCallFinished      │    │   - 跑各 evaluator           │
        │   - 不知道 evaluator 存在 │    │   - 写 EvaluationScored      │
        └─────────────────────────┘    └─────────────────────────────┘
                                                    │
                                                    │ 同 evaluators,不同部署模式:
                                                    │
                ┌───────────────┬───────────────────┴────────────────┐
                ▼               ▼                                    ▼
         ┌──────────────┐ ┌──────────────┐                  ┌──────────────────┐
         │  Online      │ │  Sync        │                  │  Offline         │
         │  - 后台 task  │ │  - caller 调  │                  │  - dataset replay│
         │  - tail live  │ │    返即时分数 │                  │  - release report│
         │    EventStore │ │  - 阻塞 path  │                  │  - 实验对比      │
         └──────────────┘ └──────────────┘                  └──────────────────┘
```

**核心架构原则：Pipeline ↔ Evaluator 经事件解耦。**

Pipeline 完成调用时**无条件写**一条 `LlmCallFinished` 事件。它不知道有谁订阅、有几个 evaluator 会跑。

Evaluators 是 EventStore 的 consumer：订阅 `LlmCallFinished` → 跑 `score(req, resp)` → 把 `Vec<DimensionScore>` 写成 `EvaluationScored` 事件。

仪表板 / 离线 report / drill-down 工具都是更下游的 consumer，从 EventStore 读 `EvaluationScored` 出趋势。

---

## 3. 核心类型

### 3.1 `Evaluator` trait

```rust
// 位置: tars-eval (新 crate) 或 tars-runtime::eval (子模块,看实施量决定)
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

**为什么 sync 和 async 分两个 trait**：

- 大多数 evaluator 是确定性的、纯函数、毫秒级——同步运行最简单
- LLM-as-judge / 远程 KB lookup 是 async；混进 sync trait 强迫所有 evaluator 都 box::pin futures，污染 80% 的简单情况
- 两个 trait 让调用方一眼看清 cost——分开调度（sync 同步跑、async 走任务池 + 限速）

**值的语义统一**：

- ratio (`0..=1`) — 大多数维度，sample_size 必填
- count — value 是计数，sample_size 留 0
- ms — value 是毫秒，sample_size 留 0
- 文档（每个 built-in evaluator 的 doc-comment）声明值类型；下游聚合代码靠约定，不靠类型 enum——避免 over-engineering

### 3.3 `Hints` —— Pass-through Insights

`Hints` 是请求生命周期内由 middleware 写、由 evaluator 读的**单向 read-only 视图**。对应 `RequestContext.attributes` 的快照（在 Pipeline 写 `LlmCallFinished` 时一次性 freeze 进 event payload）。

```rust
#[derive(Clone, Debug, Default)]
pub struct Hints {
    inner: HashMap<String, serde_json::Value>,
}

impl Hints {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> { ... }
    pub fn contains_key(&self, key: &str) -> bool { ... }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &serde_json::Value)> { ... }
    // ↑ 只读 API —— evaluator 不能 mutate
}
```

**Standard hint namespaces**（middleware 写时按 owner 前缀，evaluator 读时跨 namespace 查；新增 prefix 时在本表登记，避免冲突）：

| Prefix | 谁写 | 例子 keys |
|---|---|---|
| `validation.*` | ValidationMiddleware (Doc 15) | `validation.format_corrections`：`["unescaped_newline_in_string"]`<br>`validation.dropped_findings`：`["finding_3", "finding_7"]` |
| `cache.*` | CacheLookupMiddleware | `cache.layer`：`"L1"` / `"L2"`<br>`cache.original_call_id`：原始 trace_id |
| `retry.*` | RetryMiddleware | `retry.attempted_providers`：fallback 时走过的 provider id 列表 |
| `routing.*` | RoutingMiddleware | `routing.tier`：`"reasoning"` / `"fast"`<br>`routing.fallback_chain`：考虑过的 candidate id |
| `caller.*` | 显式由 caller 在 RequestContext 注入的应用层提示 | `caller.session_id` / `caller.user_id` (按需) |

**约束**：
- Evaluator **只读**——不能 mutate `Hints`，保持 evaluator 不影响生产路径的核心约束
- Hint key 必须用 namespace 前缀（无 prefix 的扁平 key 视为 anti-pattern），避免不同 middleware 写同名 key 互相覆盖
- 写 hint 的 middleware 文档负责声明它写哪些 key、什么 schema——按需求加字段、不私下塞

**典型用例**：

```rust
// ValidationMiddleware Filter 时写:
ctx.attributes.write().unwrap().insert(
    "validation.format_corrections".into(),
    json!(["unescaped_newline_in_string", "trailing_comma_removed"]),
);

// FormatRobustnessEvaluator 消费:
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

这样 evaluator 不需要重跑正则去检测——validation 已经发现的事实直接消费。

### 3.4 新事件

```rust
// tars-runtime::event 加三条:

pub enum AgentEvent {
    ...existing...

    /// Pipeline 完成一次成功调用时发。Evaluator runner 订阅这条。
    /// Payload 用 `serde_json::Value` 而不是硬编码 ChatRequest /
    /// ChatResponse — 见 §3.5 "为什么 wire untyped, runtime typed"。
    LlmCallFinished {
        trace_id: TraceId,
        /// `chat` / `embedding` / `completion` / `Other(...)`,
        /// EvaluatorRunner 据此选 typed evaluator 路径。
        modality: Modality,
        /// Provider-shape-specific request payload 的序列化形态。
        /// 当前 Modality::Chat 下是 `ChatRequest` 的 JSON 序列化。
        request: serde_json::Value,
        /// 同上,Modality::Chat 下是 `ChatResponse` 的 JSON 序列化。
        response: serde_json::Value,
        /// `RequestContext.attributes` 的快照 (frozen at finish-time)。
        /// Middleware 写,Evaluator 通过 `Hints` 视图读 (§3.3)。
        processing_hints: serde_json::Value,
        ts_ms: u64,
    },

    /// Evaluator 算完一次 (req,resp) 的评分时写。一个 evaluator 一次
    /// run 只产生一条 EvaluationScored,内含 `Vec<DimensionScore>` —
    /// **同一批 dim 共用一个 timestamp,group-by 聚合时严格对齐**。
    EvaluationScored {
        trace_id: TraceId,                // 关联 LlmCallFinished
        evaluator_name: String,
        scores: Vec<DimensionScore>,      // ← 原子批量, 不再 per-dim 一条
        kind: EvalKind,
        ts_ms: u64,
    },

    /// Evaluator 自己挂了 (panic / timeout / IO error 等) 时写。
    /// **必须独立 variant,不混进 EvaluationScored**:
    /// - 仪表板能区分"模型输出退化" vs "evaluator 故障"
    /// - SQL `WHERE event_type='EvaluationFailed'` 直接拉故障窗口,
    ///   `AVG(value)` 不会被半数据污染
    /// - 故障 telemetry 跟成功 telemetry 永远不交叉污染查询
    EvaluationFailed {
        trace_id: TraceId,
        evaluator_name: String,
        error_kind: EvalErrorKind,
        message: String,                  // 详细错误 (panic msg / IO error)
        elapsed_ms: u64,                  // 失败前花了多少时间 (timeout 诊断)
        kind: EvalKind,
        ts_ms: u64,
    },
}

pub enum Modality {
    /// LLM chat / completion shape — `request: ChatRequest`,
    /// `response: ChatResponse`. 当前所有 tars-pipeline 走这个。
    Chat,
    /// Embedding 模型 — request: prompt(s),response: vector(s)。
    /// 当前未实现,留 enum 位避免后续破坏 EventStore schema。
    Embedding,
    /// 老式 (non-chat) completion — string in/out,无消息边界。
    Completion,
    /// Forward-compat: 跨版本回放老 EventStore dump 时,新出现的
    /// modality 在老 tars 里仍能 deserialize,只是 typed evaluator
    /// 跳过 — 通用 untyped evaluator (如果有) 看 raw JsonValue。
    Other(String),
}

pub enum EvalKind {
    /// Online — 生产 call 顺手出的 (Online runner 写)
    Online,
    /// Offline — dataset 跑出来的 (Offline runner 写),`dataset_id`
    /// 标识哪一份数据集 / 哪一次实验
    Offline { dataset_id: String },
}

pub enum EvalErrorKind {
    /// 同步 evaluator 抛了 panic — runner 的 catch_unwind 捕获
    Panic,
    /// async evaluator 触发 tokio::time::timeout — 通常 LLM-judge / IO
    Timeout,
    /// async evaluator 的外部依赖失败 (HTTP / DB / KB)
    AsyncIoError,
    /// source_lookup 之类的依赖在 evaluator 期望时刻不可用
    DependencyMissing,
    /// Payload schema 解析不出来 — LlmCallFinished 数据已经坏了
    /// (跨版本兼容 fallback 失败的常见 case)
    SchemaSkewed,
    /// 其他无法归类的内部错误
    Internal(String),
}
```

**Schema evolution 说明**：

- `LlmCallFinished.request` / `.response` 是 `serde_json::Value`,**EventStore 不绑死任何 typed schema**——`ChatRequest` 加字段、改字段、Embedding modality 加进来,EventStore 永远能反序列化 (JsonValue 是最宽容的形态)
- 新事件 variant (`Modality::Audio` 等) 用 `#[serde(other)]` 通配兜底,老 tars 跑新 dump 仍可读
- `EvalErrorKind` 用 `Internal(String)` 作 catch-all,加新 variant 也不破老事件

### 3.5 为什么 wire untyped, runtime typed

EventStore 的 wire format 用 `serde_json::Value`,**evaluator 看到的 API 用 typed `&ChatRequest` / `&Response`**。EvaluatorRunner 是边界翻译者:

| 层 | 形态 | 理由 |
|---|---|---|
| **EventStore wire** (event payload 字段) | `serde_json::Value` | 跨 tars 版本回放 / 跨 modality 兼容 / schema evolution 自由 / 第三方分析工具不需要 link tars-types |
| **Evaluator API surface** (evaluator 看到的参数) | typed `&ChatRequest, &Response, &Hints` | IDE 补全 / 编译期 schema 校验 / PyO3 wrap 给 Python evaluator 也是 typed dict (跟 `Pipeline.complete()` 返回 Response 形态一致) |

```rust
// EvaluatorRunner 内部边界翻译:
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
    Modality::Other(_) => { /* typed evaluator 跳过, 仅 untyped 跑 */ }
}
```

**反对"全 hardcode ChatRequest/ChatResponse"的核心**：把 wire format 跟 runtime types 绑死会让 schema 演化每次都需要 migration step。LLM 这领域 12 个月就翻一次 modality (chat → tool_use → reasoning → real-time-audio → multimodal),EventStore 不能跟着每次改。

**反对"全 JsonValue 也包括 evaluator API"的核心**：evaluator 失去 typed contract,写起来痛苦,IDE 帮不上忙,PyO3 binding 也只能给 dict——跟 tars 已经建立的 typed Response 形态不一致。

折中后 EventStore 是稳定的 facts log,代码是它的 typed interpretation。

---

## 4. 解耦的两层抽象：`CallEventChannel` + `MetricsSink`

Pipeline 在调用结束时**写一个 channel**（不直接写 EventStore），background worker pool 从 channel 消费、跑 evaluator、把结果写到 `MetricsSink`。两层抽象的目的：

| 抽象 | 职责 | v1 实现 | 未来扩展 |
|---|---|---|---|
| **`CallEventChannel`** | Pipeline → Evaluator pool 的传输 | `tokio::sync::mpsc<CallEvent>` (进程内) | `TcpEventChannel` (sidecar over network) |
| **`MetricsSink`** | Evaluator 结果 → 持久化/导出 | `SqliteEventStoreSink` (写本地 SQLite) | `PrometheusSink` / `DataDogSink` / `CompoundSink` (fan-out) |

**设计原则**：

- Pipeline 路径**永不阻塞、永不知道下游谁在听**——它只 `channel.send(event).await`，几微秒返回
- Worker pool 是 **out-of-band**——pool 满了 / evaluator 慢了 / sink 写失败，**主链路完全不受影响**
- Channel 实现可换：v1 进程内 mpsc，未来网络 channel 切 sidecar 部署，**Pipeline 代码一行不改**
- Sink 实现可叠：默认 SQLite，需要 Prometheus 加 `CompoundSink([SqliteSink, PrometheusSink])`，**framework 一行不改**

这跟 OpenTelemetry Collector 的 agent-vs-collector 分层思路一致——agent in-process by default，collector when scale demands。

### 4.1 `CallEventChannel` trait

```rust
// 位置: tars-eval::channel
pub trait CallEventChannel: Send + Sync {
    /// 发送一个 LlmCallFinished 事件给下游 evaluator pool。
    /// 实现应该是非阻塞的(或者最多很短的 backpressure block) ——
    /// Pipeline 主路径不能为 metrics 路径等。
    /// Drop 事件并返回 Err 也比阻塞好(metrics 是 best-effort)。
    fn send(&self, ev: CallEvent) -> Result<(), ChannelError>;
}

#[derive(Clone, Debug)]
pub enum CallEvent {
    /// LLM call 成功完成。Eval pool 跑 success-path evaluators。
    Finished(LlmCallFinishedPayload),
    /// LLM call 失败。Eval pool 跑 failure-path evaluators (如果有)
    /// + 写 sink 留 audit trail。
    Failed(LlmCallFailedPayload),
}

pub enum ChannelError {
    /// Channel 满了 — 配置 buffer size / pool concurrency 不够
    /// metrics 跟不上 production 调用速率。Pipeline 收到这个时
    /// 应该 log + drop 事件,不阻塞主路径。
    Full,
    /// Channel 关闭了 (sidecar 下线 / pool shutdown)
    Closed,
}

/// v1 进程内实现:有界 mpsc。
pub struct LocalChannel {
    tx: tokio::sync::mpsc::Sender<CallEvent>,
}

impl CallEventChannel for LocalChannel {
    fn send(&self, ev: CallEvent) -> Result<(), ChannelError> {
        // try_send: 满了就 drop + 返回 Full,不阻塞 Pipeline。
        self.tx.try_send(ev).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => ChannelError::Full,
            mpsc::error::TrySendError::Closed(_) => ChannelError::Closed,
        })
    }
}
```

**Buffer 满了的处置**：v1 默认 `try_send` + drop。**Metrics 是 best-effort,不能拖累生产**。tracing::warn 记录 drop 数量,如果 drop 频繁说明 evaluator pool 跟不上,需要增加 worker 数量或砍 evaluator(下游问题,不该由 main pipe 解决)。

### 4.2 `MetricsSink` trait

```rust
// 位置: tars-eval::sink
#[async_trait]
pub trait MetricsSink: Send + Sync {
    /// LlmCallFinished 来了 (eval pool 也想存 raw req+resp 给 drill-down)。
    async fn write_call_finished(&self, ev: LlmCallFinishedPayload)
        -> Result<(), SinkError>;
    
    /// Evaluator 跑成功了。
    async fn write_eval_scored(&self, ev: EvaluationScoredPayload)
        -> Result<(), SinkError>;
    
    /// Evaluator 自己挂了。
    async fn write_eval_failed(&self, ev: EvaluationFailedPayload)
        -> Result<(), SinkError>;
}

/// v1 实现:写 tars-storage::EventStore。
pub struct SqliteEventStoreSink {
    store: Arc<dyn EventStore>,
}

#[async_trait]
impl MetricsSink for SqliteEventStoreSink {
    async fn write_call_finished(&self, ev: LlmCallFinishedPayload) -> Result<(), SinkError> {
        // wrap LlmCallFinishedPayload 进 AgentEvent::LlmCallFinished
        // 调 EventStore::append
        ...
    }
    // 同理其他两个
}

/// 多 sink fan-out — 例:同时写 SQLite + Prometheus
pub struct CompoundSink {
    sinks: Vec<Arc<dyn MetricsSink>>,
}

#[async_trait]
impl MetricsSink for CompoundSink {
    async fn write_eval_scored(&self, ev: EvaluationScoredPayload) -> Result<(), SinkError> {
        // 并发写所有 sinks, 任意一个失败不影响其他
        let futures: Vec<_> = self.sinks.iter().map(|s| s.write_eval_scored(ev.clone())).collect();
        let results = futures::future::join_all(futures).await;
        // 至少一个成功就视为成功(metric 容错友好);全失败才报错
        if results.iter().any(|r| r.is_ok()) { Ok(()) } else { Err(...) }
    }
    // 同理其他
}
```

**未来 sink 实现示例**(都不在 v1 范围,只是证明 trait 形态够开放):

- `PrometheusSink` — `EvaluationScored` → counter / gauge metric
- `DataDogSink` — push metrics to DataDog API
- `OtelMetricsSink` — bridge to OpenTelemetry meter
- `S3ArchiveSink` — 把 LlmCallFinished 大 payload 归档到 S3,trace_id 留在主 sink

### 4.3 Pipeline 改造

Pipeline **只跟 `CallEventChannel` 打交道**,不直接接触 sink、不直接接触 EventStore:

```rust
// tars-pipeline::Pipeline
pub struct Pipeline {
    ...existing...
    /// 可选:不传则不发 metric 事件 (test fixtures / temp pipelines)。
    /// 生产中 builder 必传。
    call_channel: Option<Arc<dyn CallEventChannel>>,
}

impl Pipeline {
    // 在 outermost wrapper 处:
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
            // Non-blocking send. 满了/关闭 → log + drop, NEVER 阻塞主路径。
            if let Err(e) = ch.send(ev) {
                tracing::warn!(error = ?e, "metrics channel send failed");
            }
        }
        
        result
    }
}
```

**关键不变量**：
- Pipeline 调 `ch.send()` 是**同步、非阻塞、永不 await**——失败 log 并丢
- 整个 metric 路径的延迟代价 = `try_send` 一次原子操作 ≈ 几纳秒
- Sink 写慢、evaluator 跑慢、网络写不通——这些**都在 worker pool 那一侧的世界里发生**,跟 Pipeline 主链路完全隔离

### 4.2 SQL 查询模式

EventStore 用 `tars-storage::SqliteEventStore` 时,`payload` 字段是 JSON 字符串。`EvaluationScored.scores` 是数组,常用查询会用 `json_each` 展开。SQLite 1.38+ / PostgreSQL 都支持。

**过去 1h 某 dim 的 moving avg**（`json_each` 展开 scores 数组）：

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

**Drill-down — 过去 1h 中 schema_compliance < 0.5 的 trace_id + 具体 details**：

```sql
SELECT
  json_extract(e.payload, '$.trace_id')        AS tid,
  CAST(json_extract(s.value, '$.value') AS REAL) AS score,
  json_extract(s.value, '$.details')           AS why  -- ← drill-down 直接拿到原因
FROM events e, json_each(json_extract(e.payload, '$.scores')) s
WHERE e.event_type = 'EvaluationScored'
  AND json_extract(s.value, '$.dim') = 'schema_compliance'
  AND CAST(json_extract(s.value, '$.value') AS REAL) < 0.5
  AND e.ts_ms > strftime('%s', 'now', '-1 hour') * 1000;
```

**用上面的 trace_id 拉具体 call 的 raw req+resp**：

```sql
SELECT payload FROM events
WHERE event_type = 'LlmCallFinished'
  AND json_extract(payload, '$.trace_id') IN (...);
```

**Cross-dim correlation — 同时段某 dim 和 retry_count 的关系**：

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

**Evaluator 健康度 — 区分"分数低"vs"评估器挂了"**（关键 health-check）：

```sql
-- 同时统计每个 evaluator 的 success rate 和 avg score。
-- avg_score 来自 EvaluationScored,failure_count 来自 EvaluationFailed,
-- 两个查询 union 起来一眼对比是哪边出问题。
SELECT
  evaluator_name,
  -- 成功路径
  COUNT(*) FILTER (WHERE event_type = 'EvaluationScored') AS scored,
  AVG(CASE WHEN event_type = 'EvaluationScored'
           THEN (
             SELECT AVG(CAST(json_extract(s.value, '$.value') AS REAL))
             FROM json_each(json_extract(payload, '$.scores')) s
           ) END) AS avg_score,
  -- 故障路径
  COUNT(*) FILTER (WHERE event_type = 'EvaluationFailed') AS failed,
  -- 故障原因 top
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

**Evaluator 故障窗口 drill-down**：

```sql
-- "过去 1h 哪些 evaluator panic / timeout 了"
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

我们提供 `tars-eval::sql::common_queries` 模块作为模板,consumer 拿来直接套,不必每个 ARC / agouflow / future 自己写 `json_each` 展开。

---

## 5. 三种部署模式

同一套 `Evaluator` trait，三种部署方式，区别只在"事件源 + 调度 + 写位置"。

### 5.1 Online（推荐主路径）

**用例**：生产路径上每 call 出指标，仪表板实时看趋势。

```rust
// 启动后台 runner:
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

// Runner 内部:
//   tail EventStore;
//   for each LlmCallFinished:
//       for each evaluator (cheap, sync): score, append EvaluationScored
//       for each evaluator (expensive, async): tokio::spawn(score_async, append)
```

特征：
- 跟生产 pipeline **完全解耦**——生产路径不感知 evaluator 存在
- 延迟：cheap evaluators 通常毫秒级落到 EventStore；expensive 异步、几秒到几十秒
- Replay-safe：runner 重启后从 last checkpoint 续跑（见 §6.1 offset 跟踪）
- 失败隔离：单个 evaluator 抛异常只影响自己那一行 EvaluationScored 缺失，不影响其他 evaluator 也不影响生产

### 5.2 Sync（escape hatch）

**用例**：caller 想立即拿到分数（很罕见，例：基于评分立即决定走 fallback）。

```rust
// 单个 call 后立即 score:
let resp = pipeline.complete(req, ctx).await?;
let scores: Vec<DimensionScore> = sync_evaluators
    .iter()
    .flat_map(|e| e.score(&req, &resp))
    .collect();
if scores.iter().any(|s| s.dim == "schema_compliance" && s.value < 0.3) {
    // Caller 决定降级 / 重试 / 报错
}
```

特征：
- 阻塞 Response 返回——只用 cheap evaluators
- caller 拿到 `Vec<DimensionScore>` 当场决策
- **不是默认模式**——大多数 caller 应该走 Online，sync 只有当"分数当场要影响行为"时用
- 注意：如果你需要的是"分数低就 reject 重试"，用的不是 evaluation，是 **Doc 15 validation**——别混

### 5.3 Offline（release gate / 实验对比）

**用例**：发布前对历史 dataset 跑一遍出 release report；prompt A/B 实验后对比。

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

特征：
- 跑历史 EventStore dump，不接触生产
- 可以跑 LLM-as-judge expensive evaluators（不影响生产延迟）
- `DatasetId` 标记这一批 EvaluationScored 来自哪个实验，方便后续 SQL 过滤
- **同一个 evaluator 实现，跟 online 完全一致**——这是架构的关键好处。online 写得好用，offline 也直接好用

---

## 6. EvaluatorRunner 实现

### 6.1 Online runner

**v1 设计:从 `CallEventChannel` 消费,写 `MetricsSink`。**进程内 tokio task pool,worker 数可配。

```rust
pub struct OnlineEvaluatorRunner {
    /// 接收 Pipeline 发的 LlmCallFinished / Failed
    rx: tokio::sync::mpsc::Receiver<CallEvent>,
    /// 结果落点 — 默认 SqliteEventStoreSink, 可换 / 可叠
    sink: Arc<dyn MetricsSink>,
    sync_evals: Vec<Arc<dyn Evaluator>>,
    async_evals: Vec<Arc<dyn AsyncEvaluator>>,
    /// 几个 worker tokio task 同时消费 channel。建议: CPU 核数 / 2。
    worker_count: usize,
    /// expensive evaluator 的全局并发上限 — 防 LLM-as-judge 把后台拖爆。
    async_concurrency_cap: Arc<Semaphore>,
}

impl OnlineEvaluatorRunner {
    pub async fn run(self: Arc<Self>) {
        // 起 worker_count 个 worker, 共享 rx via Arc<Mutex<Receiver>>
        // (或用 broadcast / task pool 模式 — 实现细节看 trait 选择)
        let mut handles = Vec::new();
        for _ in 0..self.worker_count {
            let runner = self.clone();
            handles.push(tokio::spawn(async move { runner.worker_loop().await }));
        }
        // 等所有 worker 退出 (channel 关闭时)
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
        // 0. 持久化 raw call 给后续 drill-down
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
            // Embedding / Completion / Other: typed evaluator 跳过, 通用 untyped 路径(未来)
            _ => return,
        };
        let hints = Hints::from_value(&payload.processing_hints);

        // 2. Sync evaluators — panic-safe 直接跑
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
                let _permit = permit;  // 持有到 task 结束
                let started = Instant::now();
                let timeout = Duration::from_secs(30);  // 默认 timeout, 可配
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

**关键决定**:

- **`catch_unwind` 守 sync evaluator panic** — 单个 evaluator 崩不影响其他和 worker 自身
- **`tokio::time::timeout` 守 async evaluator timeout** — 防 LLM-judge 卡死整个 worker
- **panic / timeout 都转成 `EvaluationFailed` 事件**写到 sink — 仪表板能看到"evaluator 故障"独立信号
- **worker_count 可配** — Mac 笔记本 1-2,服务器按 CPU 数。worker pool 满了 backpressure 通过 channel `Full` 信号传给 Pipeline,**Pipeline drop event + log,主链路无阻塞**

**Checkpoint 持久化**:进程内 mpsc 没 checkpoint 概念 — channel 关闭即停。**v1 不做持久 checkpoint** — 重启后**只处理新事件**,历史 dump 用 OfflineRunner 跑。如果 sidecar 化,channel 那一侧自己负责 checkpoint(参考 Kafka consumer offset 模式),tars 主进程不背这个责任。

### 6.2 Offline runner

```rust
pub struct OfflineEvaluatorRunner {
    /// 通常是 SqliteEventStore::iter_calls() 拿到的迭代器,扫
    /// 历史 dump 出所有 LlmCallFinished
    events: Box<dyn Iterator<Item = LlmCallFinishedPayload>>,
    sync_evals: Vec<Arc<dyn Evaluator>>,
    async_evals: Vec<Arc<dyn AsyncEvaluator>>,
    /// 标记这一批 score 来自哪个数据集 / 实验。仪表板用 dataset_id
    /// 区分 `prompt-v1.2` vs `prompt-v1.3` 等。
    dataset_id: DatasetId,
    /// 同 OnlineRunner — sink 落点,默认是 SqliteEventStoreSink
    /// 但带 `kind: Offline { dataset_id }` 标识。
    sink: Arc<dyn MetricsSink>,
}

impl OfflineEvaluatorRunner {
    pub async fn run_to_completion(self) -> Report {
        let mut report = Report::new();
        for payload in self.events {
            // 跟 Online runner 同样 deserialize
            let (req, resp) = match deserialize_chat(&payload) {
                Ok(pair) => pair,
                Err(e) => { report.record_error(...); continue; }
            };
            let hints = Hints::from_value(&payload.processing_hints);

            // Sync evaluators — 直接跑,不 catch_unwind(offline 让 panic 暴露 bug)
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
            // Async evaluators — offline 不限速,顺序跑,因为这里
            // 不在生产路径,跑完所有 evaluator 才有意义
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

`Report` 类型聚合所有 dimensions × all calls,提供 `to_csv` / `to_json` / `summary()` 等便利方法。同时所有 EvaluationScored **也写进 sink** (带 `kind: Offline { dataset_id }`),让后续 SQL on EventStore 也能查到 offline 跑出来的数据。

**Online vs Offline 的代码主体几乎完全一致** — 这就是 `MetricsSink` + 同一个 `Evaluator` trait 抽象的胜利:换 deployment 模式,90% 代码不动。

---

## 7. Built-in evaluators

按 cost class 分。**v1 先 deterministic & cheap**——LLM-judge 类不进 v1 built-in，trait 支持但留给 caller 自己实现。

### 7.1 Cheap / deterministic（v1 内置）

#### `SchemaComplianceEvaluator`

```rust
pub struct SchemaComplianceEvaluator {
    schema: serde_json::Value,
}
```

行为：尝试 parse `resp.text` 为 JSON，再用 jsonschema 验证。
输出：`schema_compliance` 维度，0.0 或 1.0。

#### `RubricGroundingEvaluator`

```rust
pub struct RubricGroundingEvaluator {
    json_path: String,             // "$.findings[*].rule_id"
    allowed: HashSet<String>,
}
```

输出多个维度：
- `rubric.grounded_rate`：在白名单的 rule_id 比例（ratio, sample_size = total findings）
- `rubric.ad_hoc_rate`：ad-hoc 比例
- `rubric.hallucinated_rate`：既不在白名单也不是 ad-hoc 的比例

#### `FieldFilledRateEvaluator`

```rust
pub struct FieldFilledRateEvaluator {
    json_path: String,             // "$.findings[*].evidence"
    name: String,                  // "evidence_filled" → 维度名
}
```

输出：`<name>` 维度，非空字段比例。例：ARC 的 evidence tag 完整率。

#### `RegexMatchCountEvaluator`

```rust
pub struct RegexMatchCountEvaluator {
    field: ResponseField,         // Text | Thinking | ToolCallsArgs
    pattern: regex::Regex,
    name: String,
    /// `Count` = 输出原始计数;`PerThousandChars` = 计数 / (chars/1000),
    /// 后者用于不同长度 response 之间可比。
    normalize: NormalizeMode,
}
```

通用工具——给 caller 拼"违规模式出现频率"等。

#### `LengthEvaluator`

```rust
pub struct LengthEvaluator {
    field: ResponseField,
    name: String,
}
```

输出 `<name>.chars`、`<name>.tokens` 维度。最低成本，每 call 加上做 baseline。

#### `SnippetGroundingEvaluator`

```rust
pub struct SnippetGroundingEvaluator {
    json_path: String,            // "$.findings[*].snippet"
    /// Source provider — 给一个 snippet 判断是不是真在源文件里出现。
    /// 注入构造时;运行时不做 IO。
    source_lookup: Arc<dyn Fn(&ChatRequest) -> Option<String> + Send + Sync>,
}
```

输出：`snippet.grounded_rate` —— finding 的 snippet 在原文里出现的比例。

注意 `source_lookup` 是闭包 + 构造时注入——保持 evaluator 同步无 IO。需要数据库查源码的就要用 async evaluator（见 §7.2）。

### 7.2 Expensive / non-deterministic（v1 trait 支持，不内置）

`AsyncEvaluator` trait 允许 caller 实现。最常见的两类：

- **LLM-as-judge** —— 调一个判定模型对 response 打分。慢、贵、非确定。tars 不内置；caller 想要时自己写一个 `LlmJudgeEvaluator { judge_pipeline: Arc<Pipeline>, prompt_template: String }`。
- **Ground-truth retrieval** —— 跟标注库比对（precision / recall）。需要标注数据 + 检索。tars 不内置。

文档章节给一个**示例实现**——但作为 example，不作为 built-in，避免诱导用户在不该用的地方用。

---

## 8. 反模式（明确避免）

借鉴的源头：[Fractional AI: Your evals have a Rotten Tomatoes problem](https://blog.fractionalai.com)（2026-02）。本节直译三条最重要的并应用到 tars 设计。

### 8.1 单一 "correctness" 分数 = Rotten Tomatoes

❌ 写一个 `OverallQualityEvaluator` 输出一个 `quality` 分数。

理由：把所有维度压扁成单一数字。0.87 → 0.81 时不知道哪里降了。

✅ 拆维度：schema_compliance / grounding / hallucination_rate / evidence_filled / length / 各自独立追踪。每个是单独的 SQL 查询，每个有独立的趋势线。

复合在**消费侧**做：仪表板上面"我关心的 quality"是几个 dim 的加权——这是**配置**，不是 tars 的内置 evaluator。

### 8.2 LLM-as-judge 默认上手

❌ 任何评估问题第一反应丢一个 `Rate this 0-1: ...` 的 prompt 给另一个 LLM。

理由：
- 慢——每个 dim 多一次 LLM call
- 贵——计入 token 成本
- 非确定——两次同样输入分数不同，仪表板抖动看不清趋势
- 容易要求 judge 在一个 query 里判断太多事，又掉回 §8.1

✅ 决定性优先：schema check / set membership / regex / counting / grounding-by-substring。这些覆盖 80%+ 的实际维度需求，毫秒级、确定、可重复。

只有真的 deterministic 不可表达时（"这段回复的 tone 是不是恰当"），才上 LLM-judge——而且做成 async evaluator、采样、限速。

### 8.3 把 evaluation 当 gate

❌ 仪表板看到 schema_compliance < 0.8 就报警让 SRE 干预；或者 evaluator 内部 if-score-low-then-reject。

理由：评估是仪表板，不是开关。用它做 gate 立刻把它劣化成 §8.1 的单一 correctness 分数，并且把开关延迟绑死在 evaluation runner 上（异步）——延迟 + 不确定 = 假阳性 + 漏报兼有。

✅ 评估 + 验证两件事：
- 看仪表板掉了 → 调查根因 → 改 prompt / config / 模型
- 想做 gate（"score 低就 reject 重试"）→ 写 Doc 15 OutputValidator

### 8.4 对整个 pipeline 做 evaluation 而不是单 LLM call

❌ 只在最终 `run_task` 输出层加 evaluator："这次 task 总分多少"。

理由：pipeline 里有多个 LLM call（orchestrator + worker + critic）。最终分掉的时候不知道哪个 call 的哪个 stage 退化了。

✅ 给每个 LLM call 装 evaluator——由于 events 是按 call 写的（`LlmCallFinished` per call，不是 per task），evaluator 已经天然是 per-call 粒度。仪表板维度可以加 `agent_role` 标签区分 critic / worker / orchestrator——这是 SQL filter，不是新 evaluator。

### 8.5 跨语言用单一 base class 包所有 outcome

不在我们的 design 里——`Evaluator` 输出 `Vec<DimensionScore>` 而不是 `EvaluationOutcome` enum。每个 dim 是独立数据点，不存在 "Filter / Reject / Annotate" 这种处置。这条留个反例提醒：**evaluator 不是 validator，不要复用 `ValidationOutcome`。**

---

## 9. 使用范式

### 9.1 ARC dogfood 用例

```python
# arc/eval/registry.py — caller 注册自己的 evaluators
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

# 启动 runner (一次, 在 arc 启动时):
event_store = tars.SqliteEventStore("./arc.db")
runner = tars.eval.OnlineEvaluatorRunner(event_store, EVALUATORS)
asyncio.create_task(runner.run())  # background

# Pipeline 不动 — 它只发 LlmCallFinished, runner 自己消费:
critic_pipeline = tars.Pipeline.from_default("qwen_coder_local")
                       .with_event_store(event_store)
```

仪表板查询：

```python
# arc/eval/dashboard.py — 读 EventStore 出趋势数据
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

### 9.2 Release gate 用例（offline）

```python
# 跑历史 dataset 出 v1.2 vs v1.3 prompt 对比 report
# 注意: dataset 是 EventStore dump, 不是手动标注集
events_v12 = tars.SqliteEventStore("./snapshot-prompt-v1.2.db")
events_v13 = tars.SqliteEventStore("./snapshot-prompt-v1.3.db")

report_v12 = tars.eval.OfflineEvaluatorRunner(events_v12, EVALUATORS, "v1.2").run()
report_v13 = tars.eval.OfflineEvaluatorRunner(events_v13, EVALUATORS, "v1.3").run()

# 输出表格化对比:
print(report_v12.compare(report_v13).markdown())
```

---

## 10. 维度命名与跨语义约定

随着 evaluators 加多，维度名集合膨胀。约定避免命名冲突：

### 10.1 Namespace 规则

`<evaluator_name>.<sub_dim>` —— 单 evaluator 多 dim 时用点号。例：
- `rubric.grounded_rate` / `rubric.ad_hoc_rate` / `rubric.hallucinated_rate`
- `length.chars` / `length.tokens`
- `schema_compliance`（单 dim 直接用名字）

### 10.2 单位约定

- **Ratios (`*_rate`)**：值 ∈ `[0, 1]`，sample_size 必填
- **Counts (`*_count`)**：值是非负整数，sample_size 留 0
- **Durations (`*_ms`)**：值是毫秒数
- **Token (`*_tokens`)**：值是 token 数
- **Chars (`*_chars`)**：值是字符数

### 10.3 文档要求

每个 built-in evaluator 的 doc-comment 必须声明：
- 输出哪些 dim
- 每个 dim 的取值范围 + 单位
- sample_size 的语义

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

## 11. 实施路径

参考 [Doc 14 §9](./14-implementation-path.md) milestone 体例。本文档对应 **M9 wave 2**——紧跟 Doc 15 wave 1（validation）。

### 11.1 Wave 拆分

| 阶段 | 内容 | 估时 |
|---|---|---|
| **W2.1** | `Evaluator` / `AsyncEvaluator` traits + `DimensionScore` + `EvalKind` enum + `LlmCallFinished` / `EvaluationScored` 事件 | 1 天 |
| **W2.2** | `Pipeline` 改造：完成 call 时 `tokio::spawn` 写 `LlmCallFinished` 到 EventStore；构造时接受 `event_store: Option<...>` | 1 天 |
| **W2.3** | `OnlineEvaluatorRunner` + `OfflineEvaluatorRunner` 实现 + concurrency cap | 1.5 天 |
| **W2.4** | Built-in evaluators：Schema / RubricGrounding / FieldFilledRate / RegexMatchCount / Length / SnippetGrounding | 1.5 天 |
| **W2.5** | tars-py 暴露：`tars.eval.Evaluator` base class + Built-in 一一映射 + `Pipeline.with_event_store` API | 1 天 |
| **W2.6** | SQL 查询模板模块 `tars-eval::sql::common_queries` + 文档示例 | 0.5 天 |
| **W2.7** | 单元 + 集成测试 + ARC dogfood 切换 evaluator + CHANGELOG | 1 天 |
| **总计** | | **~7.5 天** |

### 11.2 落地后立刻动作

1. ARC 把 `_known_rule_ids` 的 metric 部分（demote count）做成 `RubricGroundingEvaluator` instance，old inline 逻辑删——validation 部分迁到 Doc 15 那边
2. ARC dogfood 仪表板从手挑数字 → SQL on EventStore
3. ARC 加一个 `EvidenceFilledRateEvaluator`，立刻看到 evidence 字段填充率
4. tars 这边监控 OnlineEvaluatorRunner 的 lag——`LlmCallFinished` 写入到 `EvaluationScored` 写入的时间差，p95 应该 < 1s

### 11.3 v1 不做（推到 v2）

- LLM-as-judge built-in evaluator —— anti-pattern §8.2
- Per-tenant evaluator 配置 —— 等 multi-tenant 落地（Doc 06 §3）
- Persistent runner checkpoint —— 启动时从头扫接受重复 score 即可
- Metric backend exporter（Prometheus / DataDog） —— 等真有 user 要再加
- Streaming evaluation（token-by-token 中段评估） —— 跟 Doc 15 §13.2 一样不做
- Evaluator 之间显式依赖图 —— evaluator 之间互不依赖（同一个 LlmCallFinished 各自跑）

---

## 12. 跨文档引用

- **Doc 02 Middleware Pipeline** — Pipeline 的 outermost wrapper 在哪写 `LlmCallFinished` 事件
- **Doc 04 Agent Runtime** — `AgentEvent` enum 在那里加 `LlmCallFinished` / `EvaluationScored` 两个变体
- **Doc 09 Storage Schema** — `EventStore` trait + SQL schema；本文档加的两个事件按那里的规范持久化
- **Doc 15 Output Validation** — 对比文档：validation 是同步、改 Response 的 gate；evaluation 是异步、出指标的仪表板
- **Doc 14 Implementation Path** — milestone 排序：本文档对应 M9 wave 2

---

## 13. Open questions

### 13.1 多 EventStore 写入的一致性

如果一个 Pipeline 同时面向多个 EventStore（生产 + staging），哪个写哪个？

判断：单 EventStore per Pipeline 是 v1 假设。多 store 是未来的 multi-tenant 问题。

### 13.2 Evaluator scoring window

某些维度（"过去 100 call 的 ad-hoc 率"）天然需要 window 计算，不是 per-call score。

判断：两条路。（A）evaluator 持有 sliding window 状态——破坏"无状态纯函数"约定，不取。（B）单 call 出原始计数 score，window 在仪表板/SQL 那一侧 group-by 算——更干净。**取 B**。

### 13.3 失败 LlmCall 也写 event 吗？

`LlmCallFinished` 当前定义包含 `response`。失败时（`Err(ProviderError)`）没 response。

判断：写一个 `LlmCallFailed` 兄弟变体——失败的 case 也是评估输入（"这次 retry 是不是错误率高"）。但 v1 只先做 success 路径，failed 等 evaluator 真要这条数据时再加。

### 13.4 Evaluator 跑出来的分数和 `Response.telemetry` 关系

`Response.telemetry.cache_hit` 已经在 LlmCallFinished payload 里——通过 `response.telemetry.cache_hit`。那 evaluator 还需要单独"cache_hit_rate" dim 吗？

判断：不需要——SQL 直接 `AVG(json_extract(...))` 出来。Evaluator 只产 evaluator 计算出的事实，不重复 telemetry 已经写了的事实。

### 13.5 跟 OpenTelemetry metrics 的关系

OTel metrics 也是观测数据。EvaluationScored 事件 + OTel metrics 两套数据点位会不会重？

判断：EvaluationScored 是**语义维度**（rubric_grounded_rate）；OTel metrics 是**infra 维度**（http_request_duration_ms）。两个不重叠。未来可以做 `EvaluationScored → OTel meter` 的 exporter，但 source-of-truth 还是 EventStore。
