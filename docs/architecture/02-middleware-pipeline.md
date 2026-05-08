# 文档 02 — Middleware Pipeline 与请求生命周期

> 范围：定义 LLM 请求从进入 Runtime 到拿到响应（或被拒）的完整生命周期，以及承载这条链路的 Middleware Pipeline 抽象。
>
> 上游：消费 Doc 01 定义的 `LlmProvider` trait。
>
> 下游：被 Agent Runtime（Doc 04）调用，承载所有跨业务的横切关注点。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **业务关注点解耦** | IAM / Cache / Budget / Guard 各自独立实现、独立测试、独立替换 |
| **顺序可配置** | 洋葱层顺序在配置中显式表达，安全敏感层（IAM）可强制锁定位置 |
| **流式友好** | 所有 middleware 必须能正确处理 stream，不允许"缓冲整个流再转发"作为默认实现 |
| **Cancel 安全** | 上层 Drop 时取消信号必须透传到 Provider 层，Doc 01 §6.2.1 的 CLI 中断机制依赖于此 |
| **多租户隔离** | 每层都通过 RequestContext 感知 tenant_id，租户配置覆盖全局默认 |
| **可短路** | 任何一层都可以决定"不继续走下去"，直接返回结果或错误 |
| **可观测** | 每层进出都有 OTel span，所有决策点都有可查询的事件 |

**反目标**：
- 不在 Middleware 层做 prompt 拼装 / RAG 检索 / Agent 编排——这些是上层职责
- 不让 Middleware 持有跨请求的可变业务状态——状态外置到 Cache Registry / Budget Store / 等专用组件
- 不在 Middleware 里隐藏 retry 和降级——所有重试和 fallback 是显式的层

---

## 2. 架构总览

```
                  ┌─────────────────────────────────────┐
                  │  Agent Runtime / Application Layer  │
                  └──────────────────┬──────────────────┘
                                     │ ChatRequest + RequestContext
                                     ▼
   ┌─────────────────────────────────────────────────────────────┐
   │  ▼ inbound                                       outbound ▲ │
   │ ┌─────────────────────────────────────────────────────────┐ │
   │ │  L1  Telemetry        (最外层,包裹一切)                  │ │
   │ │ ┌─────────────────────────────────────────────────────┐ │ │
   │ │ │  L2  Auth & IAM    (Cache 前置,不可绕过)            │ │ │
   │ │ │ ┌─────────────────────────────────────────────────┐ │ │ │
   │ │ │ │  L3  Budget Control                            │ │ │ │
   │ │ │ │ ┌─────────────────────────────────────────────┐ │ │ │ │
   │ │ │ │ │  L4  Cache Lookup                          │ │ │ │ │
   │ │ │ │ │ ┌─────────────────────────────────────────┐ │ │ │ │ │
   │ │ │ │ │ │  L5  Prompt Guard (Fast + Slow lane)   │ │ │ │ │ │
   │ │ │ │ │ │ ┌─────────────────────────────────────┐ │ │ │ │ │ │
   │ │ │ │ │ │ │  L6  Routing                       │ │ │ │ │ │ │
   │ │ │ │ │ │ │ ┌─────────────────────────────────┐ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │  L7  Circuit Breaker (per-prov) │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ ┌─────────────────────────────┐ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │  L8  Retry / Fallback       │ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │ ┌─────────────────────────┐ │ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │ │   LlmProvider call      │ │ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │ └─────────────────────────┘ │ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ └─────────────────────────────┘ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ └─────────────────────────────────┘ │ │ │ │ │ │ │
   │ │ │ │ │ │ └─────────────────────────────────────┘ │ │ │ │ │ │
   │ │ │ │ │ └─────────────────────────────────────────┘ │ │ │ │ │
   │ │ │ │ └─────────────────────────────────────────────┘ │ │ │ │
   │ │ │ └─────────────────────────────────────────────────┘ │ │ │
   │ │ └─────────────────────────────────────────────────────┘ │ │
   │ └─────────────────────────────────────────────────────────┘ │
   └─────────────────────────────────────────────────────────────┘
```

**入站顺序原则**：
1. **Telemetry 最外**——span 必须包住所有失败和短路路径
2. **Auth 与 IAM 紧随**——任何"绕过 IAM 的优化"都是安全漏洞
3. **Budget 在 Cache 之前**——预算耗尽时连缓存查询都不应该发生（轻微优化，但语义干净）
4. **Cache 在 Guard 之前**——命中缓存的请求已经在过去通过了 Guard 校验，不需要重复
5. **Guard 在 Routing 之前**——选哪个 provider 不影响 prompt 是否合法
6. **Routing → Circuit Breaker → Retry → Provider**——这三层互相耦合，按 provider 维度组合

**出站顺序**（自然是入站的反向）：
- L8 Retry 决定是否再试一次
- L4 Cache 把成功响应写入（异步，不阻塞返回）
- L3 Budget 扣减实际消耗（基于 Usage）
- L1 Telemetry 关闭 span，发射最终指标

---

## 3. 核心抽象

### 3.1 LlmService trait

Pipeline 的每一层都实现同一个 trait——`LlmService`，等价于"一个能回答 LLM 请求的东西"。最内层是 Provider 适配器，外层是逐步包装。

```rust
#[async_trait]
pub trait LlmService: Send + Sync {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError>;
}
```

注意：返回类型与 Doc 01 §3 的 `LlmProvider::stream` 完全一致——这是有意的，让 Provider 直接 impl LlmService 即可作为 pipeline 的最内层。

### 3.2 Middleware trait

```rust
pub trait Middleware: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    
    /// 把 inner service 包一层,返回新 service
    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService>;
}
```

这是 Tower::Layer 的等价形式。每个 Middleware 拿到内层 service，返回包装后的 service。Pipeline 构建：

```rust
pub fn build_pipeline(
    provider_registry: Arc<ProviderRegistry>,
    middlewares: Vec<Box<dyn Middleware>>,  // 顺序：从内到外
) -> Arc<dyn LlmService> {
    let mut svc: Arc<dyn LlmService> = provider_registry.into_service();
    for mw in middlewares {
        svc = mw.wrap(svc);
    }
    svc
}
```

调用方拿到一个 `Arc<dyn LlmService>`，对外形态和单个 Provider 完全一致——所有 middleware 对调用方透明。

### 3.3 RequestContext

```rust
pub struct RequestContext {
    pub trace_id: TraceId,
    pub tenant_id: TenantId,
    pub session_id: SessionId,
    pub principal: Principal,                  // 调用方身份
    pub deadline: Option<Instant>,             // 整个请求的截止时间
    pub cancel: CancellationToken,             // tokio_util CancellationToken
    pub budget: BudgetHandle,                  // 当前可用预算的快照
    pub attributes: HashMap<String, Value>,    // 自由扩展槽
}
```

**关键设计**：
- `cancel` 是 cooperative cancellation 的核心。任何一层都可以监听 `cancel.cancelled()`，任何一层也可以调 `cancel.cancel()` 主动终止。Doc 01 §6.2.1 的 CLI cancel guard 就是订阅这个 token 实现的。
- `budget` 是引用而非拷贝——多个并发请求共享同一个 BudgetHandle，扣减是原子的。
- `attributes` 故意保留——避免 Middleware 之间为了传递自定义状态而被迫改 RequestContext 字段。

### 3.4 短路返回

某些 Middleware 在入站阶段就决定"不继续了，直接返回"——比如 IAM 拒绝、Cache 命中、Guard 拦截。短路通过返回一个**预制流**实现：

```rust
pub fn short_circuit_with_response(
    response: ChatResponse,
) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
    let events = response.into_events();
    Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
}

pub fn short_circuit_with_error(
    err: ProviderError,
) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
    Err(err)
}
```

短路时**不调用 inner service**，外层 Middleware 也无法察觉差异——它们看到的就是一个正常的事件流（或一个错误）。这保持了抽象的一致性。

---

## 4. 各层详细设计

### 4.1 L1 — Telemetry

**职责**：为整个请求建立 OTel root span；为每层 Middleware 建子 span；流式事件转 metric。

```rust
pub struct TelemetryMiddleware {
    tracer: Arc<dyn Tracer>,
    metrics: Arc<TelemetryMetrics>,
}

impl Middleware for TelemetryMiddleware {
    fn wrap(&self, inner: Arc<dyn LlmService>) -> Arc<dyn LlmService> {
        Arc::new(TelemetryService {
            inner,
            tracer: self.tracer.clone(),
            metrics: self.metrics.clone(),
        })
    }
}

#[async_trait]
impl LlmService for TelemetryService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let span = self.tracer.start_span("llm.request")
            .with_attribute("tenant", ctx.tenant_id.as_str())
            .with_attribute("trace_id", ctx.trace_id.as_str());
        
        let start = Instant::now();
        let metrics = self.metrics.clone();
        
        let inner_stream = self.inner.clone().call(req, ctx).await
            .map_err(|e| {
                span.record_error(&e);
                metrics.record_error(&e);
                e
            })?;
        
        // 流式包装：观察每个事件,统计 TTFT、token rate、最终 usage
        let mut first_token_emitted = false;
        let stream = inner_stream.inspect(move |event| {
            match event {
                Ok(ChatEvent::Delta { .. }) | Ok(ChatEvent::ThinkingDelta { .. }) => {
                    if !first_token_emitted {
                        metrics.record_ttft(start.elapsed());
                        first_token_emitted = true;
                    }
                }
                Ok(ChatEvent::Finished { usage, stop_reason }) => {
                    metrics.record_usage(usage);
                    metrics.record_total_latency(start.elapsed());
                    span.record_attribute("stop_reason", stop_reason.as_str());
                    span.end();
                }
                Err(e) => {
                    metrics.record_error(e);
                    span.record_error(e);
                }
                _ => {}
            }
        });
        
        Ok(Box::pin(stream))
    }
}
```

**关键指标**（必须采集）：
- `llm.ttft_ms`（首 token 延迟）
- `llm.total_latency_ms`
- `llm.tokens.input` / `llm.tokens.output` / `llm.tokens.cached`
- `llm.cost_usd`
- `llm.stop_reason`（label）
- `llm.errors`（按 ErrorClass label）

### 4.2 L2 — Auth & IAM

**职责**：验证调用方身份；判断调用方是否有权对当前请求资源（tenant、session、引用的代码仓库）操作。

```rust
pub struct AuthMiddleware {
    authenticator: Arc<dyn Authenticator>,
    iam_engine: Arc<dyn IamEngine>,
}

#[async_trait]
impl LlmService for AuthService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        // 1. 认证：principal 是谁?
        if !self.authenticator.verify(&ctx.principal).await? {
            return short_circuit_with_error(ProviderError::Auth("invalid principal".into()));
        }
        
        // 2. 鉴权：principal 能不能访问 req 引用的资源?
        let resources = req.referenced_resources();   // 比如 "repo:tars", "session:xyz"
        let decision = self.iam_engine.evaluate(&ctx.principal, &resources, "llm:invoke").await?;
        
        if !decision.allowed {
            // 必须在进入 Cache Lookup 之前拦截 (Doc 03 §IAM 前置)
            return short_circuit_with_error(ProviderError::Auth(format!(
                "denied: {}", decision.reason
            )));
        }
        
        // 3. 把 IAM 决策结果记到 ctx.attributes,后续层可读
        let mut ctx = ctx;
        ctx.attributes.insert("iam.allowed_scopes".into(), decision.scopes.into());
        
        self.inner.clone().call(req, ctx).await
    }
}
```

**绝对不变量**：
1. IAM 决策必须在 Cache Lookup 之前完成。任何"先看缓存命中再判权限"的优化都是 IDOR 漏洞——详见 Doc 03。
2. IAM 失败永远是 `ErrorClass::Permanent`，绝不重试。
3. IAM 决策结果（允许的 scope、可见的项目）写入 `ctx.attributes`，Cache 层据此构造命名空间隔离的哈希因子。

### 4.3 L3 — Budget Control

**职责**：在请求出门前检查预算；流式过程中累计实际消耗；耗尽时主动 cancel。

预算分三档：
- **RPM / TPM**：每分钟请求数 / token 数（限流，防瞬时打爆）
- **Daily quota**：每日总额（成本控制）
- **Cost ceiling**：金额上限（最终防线）

```rust
pub struct BudgetMiddleware {
    store: Arc<dyn BudgetStore>,                 // Redis 实现
    estimator: Arc<TokenEstimator>,
}

#[async_trait]
impl LlmService for BudgetService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        // 1. 预扣：用估算 token 数预占预算
        let estimated_input = self.estimator.estimate(&req);
        let estimated_output = req.max_output_tokens.unwrap_or(2048) as u64;
        let estimated_cost = estimate_cost(&req.model, estimated_input, estimated_output);
        
        let reservation = self.store.reserve(
            &ctx.tenant_id,
            BudgetReservation {
                request_id: ctx.trace_id.clone(),
                tokens: estimated_input + estimated_output,
                cost_usd: estimated_cost,
            },
        ).await?;
        
        if !reservation.granted {
            return short_circuit_with_error(ProviderError::BudgetExceeded);
        }
        
        // 2. 调下游
        let inner_stream = match self.inner.clone().call(req, ctx.clone()).await {
            Ok(s) => s,
            Err(e) => {
                // 失败时立即释放预扣
                self.store.release(&reservation).await.ok();
                return Err(e);
            }
        };
        
        // 3. 流式追踪：累计 token,中途超额则 cancel
        let store = self.store.clone();
        let cancel = ctx.cancel.clone();
        let stream = inner_stream.inspect(move |event| {
            if let Ok(ChatEvent::UsageProgress { partial }) = event {
                // 部分 provider 中途汇报 usage
                if partial.output_tokens > reservation.tokens * 12 / 10 {
                    // 实际消耗超过预扣 20% → cancel
                    cancel.cancel();
                }
            }
            if let Ok(ChatEvent::Finished { usage, .. }) = event {
                // 最终结算：释放预扣 + 按真实 usage 扣减
                let actual_cost = compute_cost(&usage);
                tokio::spawn({
                    let store = store.clone();
                    let reservation = reservation.clone();
                    async move {
                        store.commit(&reservation, actual_cost).await.ok();
                    }
                });
            }
        });
        
        Ok(Box::pin(stream))
    }
}
```

**关键设计**：
- **预扣 + 实结算**两阶段：避免"流式生成到一半才发现超额"。预扣是悲观的（按 max_output_tokens 算），实际通常不会用满。
- **流式中途超额**：如果 provider 支持 `UsageProgress` 事件（OpenAI、Gemini 部分支持），可以在生成过程中检测异常超额（比如设置 max=2048 但已经吐了 3000）并提前 cancel。
- **token 估算用 fast 模式**（`chars / 4`），不在请求路径上加载 tokenizer——Doc 01 §15 反模式 1。
- **commit 异步执行**：不阻塞流返回，扣减失败只记日志告警。

### 4.4 L4 — Cache Lookup

**职责**：根据 IAM 加固的 cache key 查找已有响应；命中则短路返回；未命中则继续，并在响应回来后异步写入。

详细实现见 Doc 03，本层只描述与 Pipeline 的接口：

```rust
#[async_trait]
impl LlmService for CacheLookupService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        // 1. 构造命名空间隔离的 cache key (依赖上一层 IAM 写入的 scopes)
        let scopes = ctx.attributes.get("iam.allowed_scopes")
            .ok_or_else(|| ProviderError::Internal("iam scopes missing".into()))?;
        let key = self.compute_key(&req, &ctx.tenant_id, scopes);
        
        // 2. L1 内存查
        if let Some(cached) = self.l1.get(&key).await {
            return short_circuit_with_response(cached);
        }
        
        // 3. L2 Redis 查
        if let Some(cached) = self.l2.get(&key).await? {
            self.l1.put(key.clone(), cached.clone()).await;
            return short_circuit_with_response(cached);
        }
        
        // 4. L3 Provider explicit cache (Gemini cachedContent / Anthropic cache_control)
        //    注入到 req.cache_directives,Provider 层负责使用
        let mut req = req;
        if let Some(handle) = self.l3_lookup(&key, &ctx).await? {
            req.cache_directives.push(CacheDirective::UseExplicit { handle });
        }
        
        // 5. 调下游
        let inner_stream = self.inner.clone().call(req, ctx).await?;
        
        // 6. 流式包装：累积响应,完成后异步写入 cache
        let writer = self.writer.clone();
        let key_for_write = key.clone();
        let mut accumulator = ChatResponseBuilder::new();
        let stream = inner_stream.inspect(move |event| {
            if let Ok(ev) = event {
                accumulator.apply_ref(ev);
                if matches!(ev, ChatEvent::Finished { .. }) {
                    let response = accumulator.snapshot();
                    let writer = writer.clone();
                    let key = key_for_write.clone();
                    tokio::spawn(async move {
                        writer.write(key, response).await.ok();
                    });
                }
            }
        });
        
        Ok(Box::pin(stream))
    }
}
```

### 4.5 L5 — Prompt Guard（双通道）

**职责**：拦截 prompt injection、越狱指令、敏感内容。前面讨论过的快慢双通道在这一层落地。

```rust
pub struct PromptGuardMiddleware {
    fast: Arc<FastGuard>,                        // aho-corasick
    slow: Arc<dyn ClassifierProvider>,           // ONNX DeBERTa
    config: GuardConfig,
}

#[async_trait]
impl LlmService for PromptGuardService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        // 1. 提取要检测的文本：仅用户输入,不含 system / 历史 (避免性能浪费)
        let text_to_check = extract_user_input(&req);
        
        // 2. Fast lane：串行,<1ms 必须过
        if self.fast.scan(&text_to_check) {
            return short_circuit_with_error(ProviderError::ContentFiltered {
                category: "fast_heuristic".into(),
            });
        }
        
        // 3. Slow lane：并行启动,与下游 LLM 调用竞速
        let slow = self.slow.clone();
        let slow_check = tokio::spawn(async move {
            slow.classify(&text_to_check).await
        });
        
        // 4. 启动下游
        let inner_stream = self.inner.clone().call(req, ctx.clone()).await?;
        
        // 5. select! 模式：两条腿并行
        //    - 慢通道先返回 unsafe → cancel inner stream + 返回 ContentFiltered
        //    - inner stream 自然结束 → 慢通道结果用于审计但不影响返回
        let cancel = ctx.cancel.clone();
        let stream = async_stream::try_stream! {
            tokio::pin!(slow_check);
            let mut inner = inner_stream;
            let mut slow_resolved = false;
            
            loop {
                tokio::select! {
                    biased;  // 优先检查慢通道
                    
                    result = &mut slow_check, if !slow_resolved => {
                        slow_resolved = true;
                        match result {
                            Ok(Ok(classification)) if classification.is_unsafe() => {
                                // 拦截：取消下游,短路返回错误
                                cancel.cancel();
                                Err(ProviderError::ContentFiltered {
                                    category: format!("ml_classifier:{}", classification.label),
                                })?;
                            }
                            _ => continue,  // 安全或分类失败 → 继续走 inner
                        }
                    }
                    
                    Some(event) = inner.next() => {
                        yield event?;
                    }
                    
                    else => break,
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}
```

**关键决策**：
- **快通道串行 + 慢通道并行**——把 ML 推理的 10-30ms 隐藏在 LLM TTFT 后面，合法请求 0 安全延迟（讨论中提到的优化）
- **被拦截的请求浪费几十个 token 的 prefill**——可接受的代价
- **角色上下文分离**：classify 时附带 `role_hint`（"代码审查 / 文档生成 / 自由对话"），分类器据此校准。避免"用户提交的恶意代码样本被误判"的假阳性。
- **slow lane 失败不阻塞**：分类器宕机时降级为"仅 fast lane"，告警但不熔断业务

### 4.6 L6 — Routing

**职责**：根据 `ModelHint` 和 RoutingPolicy 选定 ProviderId，把 `req.model` 改写为 `ModelHint::Explicit(具体模型名)` 后传给下游。

实现细节见 Doc 01 §12，本层只是把 RoutingPolicy 包装成 Middleware：

```rust
#[async_trait]
impl LlmService for RoutingService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let candidates = self.registry.candidates_for(&req.model);
        let ranked = self.policy.select(&req, &candidates, &self.metrics).await?;
        
        // 把首选 provider 写入 ctx,下游 (Circuit Breaker / Provider 调用) 据此 dispatch
        let mut ctx = ctx;
        ctx.attributes.insert("routing.provider_priority".into(), ranked.into());
        
        self.inner.clone().call(req, ctx).await
    }
}
```

注意：Routing 不直接调 Provider，它只是**选择**，实际调用由更内层的 Retry/Fallback 层负责（拿到 priority 列表，第一个失败用第二个）。

### 4.7 L7 — Circuit Breaker

**职责**：跟踪每个 Provider 的健康状态；故障率超阈值时断路，快速失败而非排队。

```rust
pub struct CircuitBreakerMiddleware {
    breakers: Arc<DashMap<ProviderId, CircuitBreaker>>,
    config: BreakerConfig,
}

#[async_trait]
impl LlmService for CircuitBreakerService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let priority: Vec<ProviderId> = ctx.attributes.get("routing.provider_priority")
            .and_then(|v| v.clone().try_into().ok())
            .ok_or_else(|| ProviderError::Internal("routing priority missing".into()))?;
        
        // 过滤掉 open 状态的 breaker
        let healthy: Vec<_> = priority.into_iter()
            .filter(|p| self.breakers.get(p).map_or(true, |b| b.state() != CircuitState::Open))
            .collect();
        
        if healthy.is_empty() {
            return short_circuit_with_error(ProviderError::ModelOverloaded);
        }
        
        let mut ctx = ctx;
        ctx.attributes.insert("routing.provider_priority".into(), healthy.into());
        
        self.inner.clone().call(req, ctx).await
        // 出站时根据响应更新 breaker state (在 stream Finished/Error 事件里)
    }
}
```

**配置**：
- `failure_threshold`: 滑动窗口内失败率 (默认 50%)
- `min_requests`: 触发断路的最小样本数 (默认 20，防止冷启动误判)
- `open_duration`: open 状态持续时间 (默认 30s)
- `half_open_max_requests`: 半开状态允许的探测请求数 (默认 3)

### 4.8 L8 — Retry / Fallback

**职责**：单 provider 内重试可恢复错误；耗尽重试后切换到 priority 列表的下一个 provider。

```rust
#[async_trait]
impl LlmService for RetryService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let priority: Vec<ProviderId> = ctx.attributes.get("routing.provider_priority")
            .and_then(|v| v.clone().try_into().ok())
            .unwrap();
        
        for (idx, provider_id) in priority.iter().enumerate() {
            let mut attempt = 0;
            loop {
                if ctx.cancel.is_cancelled() {
                    return Err(ProviderError::Internal("cancelled".into()));
                }
                
                let provider = self.registry.get(provider_id)?;
                match provider.clone().stream(req.clone(), ctx.clone()).await {
                    Ok(stream) => return Ok(stream),
                    Err(e) => {
                        match e.class() {
                            ErrorClass::Retriable if attempt < self.config.max_retries => {
                                attempt += 1;
                                let backoff = compute_backoff(attempt, e.retry_after());
                                tokio::time::sleep(backoff).await;
                                continue;
                            }
                            ErrorClass::Retriable | ErrorClass::MaybeRetriable => {
                                // 切下一个 provider
                                if idx + 1 < priority.len() {
                                    tracing::warn!(?e, ?provider_id, "fallback to next provider");
                                    break;
                                }
                                return Err(e);
                            }
                            ErrorClass::Permanent => return Err(e),
                        }
                    }
                }
            }
        }
        
        Err(ProviderError::Internal("all providers exhausted".into()))
    }
}
```

**关键决策**：
- **Permanent 错误绝不切换**——4xx / 内容过滤 / 预算耗尽 / context 太长，换 provider 也会失败
- **退避算法**：指数 + jitter + 尊重 `retry_after` 提示
- **流式响应不重试 mid-stream**——如果 stream 已经吐了几个 token 然后断，必须传给上层处理（部分结果 + 错误），不能静默重试导致重复内容

### 4.9 出站层（Schema Validation 等）

Schema Validation 在概念上是"出站"层，但实现上仍然是 Middleware——它在 inner stream 上加包装，等 stream 结束时校验完整响应：

```rust
#[async_trait]
impl LlmService for SchemaValidationService {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let schema = match &req.structured_output {
            Some(s) => s.clone(),
            None => return self.inner.clone().call(req, ctx).await,  // 不需要校验,直通
        };
        
        let inner_stream = self.inner.clone().call(req, ctx.clone()).await?;
        
        let mut accumulator = TextAccumulator::new();
        let validator = self.validator.clone();
        
        let stream = async_stream::try_stream! {
            let mut inner = inner_stream;
            while let Some(event) = inner.next().await {
                let event = event?;
                if let ChatEvent::Delta { text } = &event {
                    accumulator.push(text);
                }
                if let ChatEvent::Finished { .. } = &event {
                    // 流结束,校验完整文本
                    let full = accumulator.into_string();
                    match validator.validate(&full, &schema) {
                        Ok(_) => yield event,
                        Err(e) => Err(ProviderError::Parse(format!(
                            "schema validation failed: {}", e
                        )))?,
                    }
                } else {
                    yield event;
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}
```

**Schema 校验在 Provider strict mode 已经启用时通常是冗余的**——但作为防御深度保留，并且对于 Provider 不支持 strict mode 的场景（本地 Ollama）是必需的。

---

## 5. Cancel 信号的传播

```
Application Layer
       │ creates RequestContext { cancel: CancellationToken::new() }
       ▼
Middleware L1 (Telemetry)
       │ inspects cancel,start span
       ▼
... (middleware chain) ...
       │
       ▼
Middleware L5 (PromptGuard) ──── slow lane returns unsafe ────► cancel.cancel()
       │
       ▼
Middleware L8 (Retry) ───── stream.next() returns error due to cancel ───► return Err
       │
       ▼
LlmProvider (CLI backend)
       │ CancelGuard::drop() ──► send Interrupt JSONL ──► subprocess stops
```

每一层都必须做两件事：
1. **传递 token**：`ctx.cancel.clone()` 移交到 inner service
2. **响应 token**：长时间操作（包括 await stream.next()）配合 `select!` 监听 cancel

错误模式：某层做了阻塞操作（同步 IO、阻塞 mutex）而没监听 cancel——整条链路就卡住了。在 Rust 里这种问题通过 `tokio::select!` 和 `CancellationToken::cancelled()` 显式表达。

---

## 6. 流式处理的特殊挑战

### 6.1 中途短路的实现模式

三种典型短路时机：
1. **Prompt Guard 慢通道判恶意**（§4.5）→ select! 模式
2. **Budget 中途超额**（§4.3）→ inspect + cancel.cancel()
3. **超时 / 上游主动取消**（应用层）→ deadline + cancel

统一原则：**短路 = cancel + 返回错误事件**，不是直接关闭 stream。这样上层 inspect/observability 仍能看到一个完整的事件序列（包括"为什么提前结束"）。

### 6.2 观察 vs 消费

- **Telemetry / Cost Accounting**：观察事件，原样转发（`inspect`）
- **Cache Store**：累积副本写缓存，原样转发（`inspect` + 异步 spawn）
- **Schema Validation**：累积副本，最后校验，原样转发或替换为错误
- **Prompt Guard**：可能 cancel + 替换流尾

只有 Schema Validation 在异常时**修改流**，其他都是只读观察。这个区分让性能分析容易：只读层零拷贝，写层可能引入分配。

### 6.3 流式包装的成本

每层 `inspect` / `async_stream` 都引入一次 BoxStream 包装。在 hot path 上，10 层 middleware 意味着 10 次堆分配（流本身）+ N×事件数 次 vtable 调用。

实测 LLM 流式吞吐通常 50-200 events/s，包装成本可忽略。但是：
- 不要在 inspect 闭包里做 syscall（写文件、网络、锁）
- 不要在 inspect 闭包里做 deep clone
- 异步任务（cache write、metrics emit）一律 `tokio::spawn`，不阻塞流

---

## 7. 配置形态

```toml
[pipeline]
# 顺序：从外到内
order = [
  "telemetry",
  "auth",
  "iam",
  "budget",
  "cache_lookup",
  "prompt_guard",
  "schema_validation",      # 出站逻辑,但层序仍在外
  "routing",
  "circuit_breaker",
  "retry",
]

# 强制锁定的位置约束：违反时启动失败
[pipeline.constraints]
"iam" = { must_be_before = ["cache_lookup"] }
"auth" = { must_be_before = ["iam"] }
"telemetry" = { must_be_outermost = true }

[middleware.budget]
store = "redis"
default_tpm = 100000
default_rpm = 60
default_daily_cost_usd = 50

[middleware.cache_lookup]
l1_capacity = 1024
l1_ttl_secs = 300
l2_backend = "redis"
l3_enabled = true

[middleware.prompt_guard]
fast_lane = "regex"
fast_lane_patterns_file = "/etc/tars/guard_patterns.txt"
slow_lane = "onnx"
slow_lane_provider = "guard_classifier"
slow_lane_threshold = 0.85
slow_lane_mode = "parallel"        # vs "serial"

[middleware.retry]
max_retries = 3
backoff_initial_ms = 200
backoff_max_ms = 10000
jitter_ratio = 0.3

[middleware.circuit_breaker]
failure_threshold = 0.5
min_requests = 20
open_duration_secs = 30
half_open_max_requests = 3
```

**租户级别覆盖**：

```toml
[tenants.acme_corp.middleware.budget]
default_tpm = 500000          # 大客户更高额度
default_daily_cost_usd = 500
```

---

## 8. 错误处理与短路语义

每个 Middleware 在入站可能返回三种结果：
1. **Continue**：正常调 `inner.call()`，把结果（流）返回
2. **ShortCircuit success**：返回预制响应流（如 Cache hit）
3. **ShortCircuit error**：返回 Err（如 IAM 拒绝、Budget 耗尽）

入站短路时**不调用 inner.call()**，因此 inner middleware 完全不感知。出站短路（在流中替换错误）则会被 inner middleware 看到。

错误传播的总原则：
- 永久错误（4xx、IAM、内容过滤）→ 立即向上抛，不重试不切换
- 可恢复错误（5xx、网络、超时）→ Retry/Fallback 层吸收
- 流中错误 → 作为 `ChatEvent::Err` 抛出，由调用方决定是否上报

---

## 9. 测试策略

### 9.1 每层独立单测

用 `MockLlmService` 作为 inner，验证每层在各种输入下的行为：

```rust
#[tokio::test]
async fn iam_blocks_unauthorized() {
    let mock_inner = MockLlmService::new();
    let iam = AuthService::new(MockAuthenticator::accept_all(), MockIam::deny_all(), mock_inner);
    
    let result = Arc::new(iam).call(test_request(), test_ctx_unauthorized()).await;
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}
```

### 9.2 顺序约束测试

```rust
#[test]
fn pipeline_rejects_invalid_order() {
    let config = PipelineConfig {
        order: vec!["cache_lookup", "iam"],  // IAM 必须在 Cache 之前!
        ..Default::default()
    };
    
    let err = Pipeline::build(config).unwrap_err();
    assert!(err.to_string().contains("iam must be before cache_lookup"));
}
```

### 9.3 端到端集成测试

完整 pipeline + MockProvider，断言事件流的形状、次序、metrics 输出：

```rust
#[tokio::test]
async fn cache_hit_short_circuits_provider() {
    let provider_call_count = Arc::new(AtomicU32::new(0));
    let mock_provider = MockProvider::counting(provider_call_count.clone());
    
    let pipeline = build_test_pipeline(mock_provider);
    
    // 第一次 - miss,命中 provider
    pipeline.call(req.clone(), ctx.clone()).await?.collect::<Vec<_>>().await;
    assert_eq!(provider_call_count.load(Ordering::SeqCst), 1);
    
    // 第二次 - hit,不应调 provider
    pipeline.call(req, ctx).await?.collect::<Vec<_>>().await;
    assert_eq!(provider_call_count.load(Ordering::SeqCst), 1);
}
```

### 9.4 Cancel 传播测试

```rust
#[tokio::test]
async fn cancel_propagates_to_provider() {
    let provider_cancelled = Arc::new(AtomicBool::new(false));
    let mock = MockProvider::observing_cancel(provider_cancelled.clone());
    
    let pipeline = build_test_pipeline(mock);
    let ctx = test_ctx();
    let cancel = ctx.cancel.clone();
    
    let mut stream = pipeline.call(req, ctx).await?;
    let _ = stream.next().await;        // 拿到第一个事件
    cancel.cancel();                     // 主动取消
    drop(stream);                        // Drop 触发 Provider cancel
    
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(provider_cancelled.load(Ordering::SeqCst));
}
```

---

## 10. 反模式清单

1. **不要在 Middleware 里持有跨请求的可变状态**——状态外置（Cache Store、Budget Store、Metrics Registry）。
2. **不要在 hot path 做 syscall**（除了 Cache 查询和 metrics emit，且必须 async）。
3. **不要忽略 cancel signal**——长时间操作必须 select! 配合 cancel.cancelled()。
4. **不要在 streaming middleware 里缓冲整个流再转发**——除非 Schema Validation 这种本质需要完整文本的场景。即使如此，也要考虑增量校验。
5. **不要在 middleware 里直接调 Provider**——所有 provider 调用都通过 inner.call()，让上层（Routing/Retry/Breaker）介入。
6. **不要让 IAM 决策依赖 Cache 数据**——Cache 是性能优化，不是安全边界。
7. **不要在 retry middleware 里重试 Permanent 错误**——浪费配额，可能触发 provider 侧的滥用检测。
8. **不要让 Telemetry 层处理业务逻辑**——只观察，不决策。Telemetry 出错绝不应影响业务路径。
9. **不要让多个 middleware 都尝试解析同一份响应**——累积副本一份，跨层共享（通过 ctx.attributes 传 ResponseAccumulator handle）。
10. **不要在 Middleware 配置里硬编码 ProviderId**——通过 RoutingPolicy 抽象，避免 Pipeline 与具体 Provider 耦合。
11. **不要在每次请求都创建新的 reqwest Client / DB connection**——复用 Arc 实例，连接池在 Middleware 构造时初始化。
12. **不要让 Middleware 之间的依赖隐式**（"这层假设上一层写了 ctx.attributes 某字段"）——文档化或用类型化的 Extension 容器表达。

---

## 11. 与上下游的边界

### 上游（Agent Runtime）契约

Agent Runtime 调用 pipeline 时承诺：
- 提供完整的 RequestContext（trace_id、tenant_id、principal、cancel token）
- 设置合理的 deadline
- 在 stream 不再需要时 Drop 它（触发 cancel 传播）

### 下游（Provider）契约

Pipeline 调用 Provider 时承诺：
- ChatRequest 已经过 prompt 拼装、IAM 校验、Guard 检查
- `req.model` 已经是具体模型名（`ModelHint::Explicit`）
- 不会重试 Provider 已经返回 Permanent 错误的请求

### Middleware 之间的契约（通过 ctx.attributes）

| Key | 写入方 | 读取方 |
|---|---|---|
| `iam.allowed_scopes` | L2 Auth | L4 Cache Lookup |
| `routing.provider_priority` | L6 Routing | L7 Circuit Breaker, L8 Retry |
| `cache.hit` | L4 Cache | L1 Telemetry (作为 metric label) |
| `budget.reservation_id` | L3 Budget | L3 Budget (出站 commit 时) |

新增 Middleware 时，必须在文档里登记其读写的 attributes，避免隐式依赖。

---

## 12. 待办与开放问题

- [ ] Pipeline 启动时的依赖注入容器选型（hand-rolled vs `shaku` / `dependency-inversion`）
- [ ] 多租户配置的热加载机制（不重启服务的前提下应用新配额）
- [ ] Budget Store 的 Redis 实现细节（Lua 脚本原子扣减 vs WATCH/MULTI）
- [ ] Schema 校验失败的自动重试反馈循环（当前是抛错，未来可以喂回 LLM 让它自修）
- [ ] Telemetry 层与 OpenTelemetry Collector 的协议选型（OTLP gRPC vs HTTP）
- [ ] Pipeline metrics 暴露格式（Prometheus pull vs OTel push）
