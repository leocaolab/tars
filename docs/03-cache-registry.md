# 文档 03 — Cache Registry

> 范围：定义 LLM 响应缓存的三级架构（L1 进程内 / L2 Redis / L3 Provider explicit cache）、Key 构造、租户隔离、stampede 防护、主动回收（Janitor）。
>
> 上游：被 Doc 02 §4.4 的 `CacheLookupMiddleware` 调用。
>
> 下游：消费 Doc 01 §10 的 `ExplicitCacheProvider` sub-trait 管理 Provider 侧缓存对象。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **租户绝对隔离** | 不同 tenant_id / IAM scope 的请求即使 prompt 相同，cache key 也必须不同 |
| **三级抽象统一** | L1 / L2 / L3 三层在调用方看来是同一接口，差异在 Registry 内部 |
| **零穿透** | Singleflight 保证同一 key 的并发请求只会有一次到达 Provider |
| **主动止损** | 不依赖 Provider TTL 自动过期，Janitor 主动 delete 控制成本 |
| **流式友好** | Cache write 异步执行，不阻塞响应返回 |
| **可观测** | 每次 lookup / hit / miss / write / evict 都有 metric |
| **可逆** | 任何缓存操作都不能破坏数据正确性——缓存损坏时降级为 miss，不阻断业务 |

**反目标**：
- 不做语义缓存（embedding 相似度匹配）——精度不可控，租户隔离做不干净，留给上层业务自己决定
- 不缓存 stream 中途的部分响应——只缓存完整成功的响应
- 不在 Cache 层做 IAM 决策——Cache Key 已经包含 IAM scope，Cache 只认 Key
- 不向上层暴露 L3 Provider cache 的 raw handle——必须经过 Registry 包装

---

## 2. 三级缓存模型

### 2.1 层级定位

| 层 | 介质 | 范围 | 命中延迟 | TTL 典型值 | 命中收益 | 失效成本 |
|---|---|---|---|---|---|---|
| **L1** | 进程内 `moka` LRU | 单实例 | <100µs | 5-15 min | 完全跳过网络 | 进程重启全失 |
| **L2** | Redis | 集群共享 | 1-3 ms | 1-24 h | 跳过 LLM API | 跨实例同步成本 |
| **L3** | Provider 侧 (Gemini cachedContent / Anthropic cache_control / OpenAI 隐式) | 单 Provider 账号 | 走完整网络但减少 input token 计费 | 5min - 1h | 大幅降低 input token 成本 | 仍要付 output 成本和延迟 |

**重要观察**：
- L1 和 L2 缓存的是**完整响应**——hit 时 Provider 不被调用
- L3 缓存的是**输入 prefix**——hit 时 Provider 仍被调用，只是 input token 不全额计费
- L3 必须显式管理（创建、引用、删除），L1/L2 是透明 KV
- 三层不是包含关系，是互补关系：L1/L2 命中跳过 LLM，L3 命中减半成本但仍走 LLM（适合每次输入不同但 system prompt 巨大的场景）

### 2.2 选择策略

不是所有请求都适合走全部三层：

| 请求特征 | L1 | L2 | L3 |
|---|---|---|---|
| 短 prompt + 高频重复 (FAQ) | ✅ | ✅ | ❌ 不必要 |
| 长 system prompt + 多轮对话 | ❌ 多轮变化大 | ❌ 同上 | ✅ system prompt 复用 |
| 长 RAG context + 单次推理 | ❌ context 唯一 | ❌ 同上 | ✅ 如果 context 重复 |
| temperature > 0 的创意生成 | ❌ 缓存伤害多样性 | ❌ 同上 | ✅ 仍可省 input cost |
| Tool use 链式调用中间步骤 | ✅ 严格 deterministic | ✅ | ❌ 通常 prefix 短 |

策略由 `CachePolicy` 表达，跟随请求传入：

```rust
pub struct CachePolicy {
    pub l1: bool,
    pub l2: bool,
    pub l3: bool,
    pub l1_ttl: Option<Duration>,   // None = 用默认
    pub l2_ttl: Option<Duration>,
    pub l3_ttl: Option<Duration>,
}

impl Default for CachePolicy {
    fn default() -> Self {
        // 默认开 L1+L2,L3 由请求显式开
        Self { l1: true, l2: true, l3: false, l1_ttl: None, l2_ttl: None, l3_ttl: None }
    }
}
```

调用方（通常是 Agent Runtime）通过 `RequestContext::attributes` 传入：

```rust
ctx.attributes.insert("cache.policy".into(), CachePolicy { l3: true, ..Default::default() }.into());
```

---

## 3. Cache Key 构造

### 3.1 致命的反例

最容易出的 IDOR 漏洞：

```rust
// ❌ 错误：仅哈希 prompt
let key = sha256(format!("{}{}{}", system, user_msg, model));
```

这意味着：实习生 B 与总监 A 各自请求同一段开源代码 review，cache key 完全相同。如果代码在某次请求中被替换为机密代码，B 会读到 A 的机密分析。**前面对话中讨论的核心安全问题**。

### 3.2 正确的 Key 构造

```rust
pub struct CacheKey {
    pub fingerprint: [u8; 32],     // 最终的 SHA-256
    pub debug_label: String,        // 仅用于日志,不参与匹配
}

pub struct CacheKeyFactory {
    hasher_version: u32,            // bump 这个值可以一键失效全部缓存
}

impl CacheKeyFactory {
    pub fn compute(
        &self,
        req: &ChatRequest,
        ctx: &RequestContext,
    ) -> Result<CacheKey, CacheError> {
        let mut h = Sha256::new();
        
        // —— 隔离域 (必须前置) ——
        h.update(self.hasher_version.to_le_bytes());
        h.update(b"\0TENANT\0");
        h.update(ctx.tenant_id.as_bytes());
        
        // IAM scopes 必须参与哈希,且要规范化排序
        let scopes: &Vec<String> = ctx.attributes.get("iam.allowed_scopes")
            .ok_or(CacheError::MissingIamScopes)?
            .downcast_ref()
            .ok_or(CacheError::MissingIamScopes)?;
        let mut sorted_scopes = scopes.clone();
        sorted_scopes.sort();
        h.update(b"\0SCOPES\0");
        for scope in &sorted_scopes {
            h.update(scope.as_bytes());
            h.update(b"\0");
        }
        
        // —— 模型身份 ——
        h.update(b"\0MODEL\0");
        match &req.model {
            ModelHint::Explicit(name) => h.update(name.as_bytes()),
            ModelHint::Tier(tier) => {
                // Tier 不能直接哈希,必须先经过 Routing 解析为 Explicit
                return Err(CacheError::UnresolvedModel);
            }
            ModelHint::Ensemble(_) => {
                // Ensemble 通常不该缓存
                return Err(CacheError::UncacheableRequest);
            }
        }
        
        // —— 决定输出的请求参数 ——
        h.update(b"\0PARAMS\0");
        // temperature > 0 时,理论上每次输出都不同 → 不应缓存
        // 但很多调用方不显式设 temperature,默认值由 Provider 决定
        // 策略：temperature 显式为 0 才进入缓存,其他全部跳过
        match req.temperature {
            Some(t) if t == 0.0 => h.update(b"t=0"),
            _ => return Err(CacheError::NonDeterministic),
        }
        if let Some(seed) = req.seed {
            h.update(b"\0SEED\0");
            h.update(seed.to_le_bytes());
        }
        if let Some(max) = req.max_output_tokens {
            h.update(b"\0MAX\0");
            h.update(max.to_le_bytes());
        }
        for stop in &req.stop_sequences {
            h.update(b"\0STOP\0");
            h.update(stop.as_bytes());
        }
        
        // —— 内容 ——
        h.update(b"\0SYSTEM\0");
        if let Some(sys) = &req.system {
            h.update(sys.as_bytes());
        }
        
        h.update(b"\0MESSAGES\0");
        for msg in &req.messages {
            hash_message(&mut h, msg);   // 规范化序列化
        }
        
        // Tools schema 也影响输出
        if !req.tools.is_empty() {
            h.update(b"\0TOOLS\0");
            let tools_canonical = serde_json::to_vec(&req.tools).map_err(CacheError::Serialize)?;
            h.update(&tools_canonical);
        }
        
        // Structured output schema
        if let Some(schema) = &req.structured_output {
            h.update(b"\0SCHEMA\0");
            let schema_canonical = serde_json::to_vec(&schema.schema).map_err(CacheError::Serialize)?;
            h.update(&schema_canonical);
        }
        
        let fingerprint: [u8; 32] = h.finalize().into();
        Ok(CacheKey {
            fingerprint,
            debug_label: format!(
                "tenant={} model={} msg_count={}",
                ctx.tenant_id, req.model.as_str(), req.messages.len()
            ),
        })
    }
}
```

**强制规则**：
1. **`hasher_version`** 是第一个进入哈希的字节——bump 这个常量可以一键作废全部 L1/L2 缓存（用于发现哈希算法 bug 时的紧急止血）
2. **TENANT + SCOPES 必须前置**——逻辑上是命名空间，把它们前置放有助于 debug 时通过原始字节流定位问题
3. **每个字段用 `\0` 分隔符 + 字段名 tag**——防止字段拼接歧义攻击（"abc" + "def" vs "ab" + "cdef"）
4. **temperature ≠ 0 直接拒绝缓存**——非确定性输出缓存毫无意义
5. **ModelHint 必须是 Explicit**——上游 Routing 层负责解析。这意味着 Cache Lookup 必须在 Routing 之后？不——前面 Doc 02 是 Cache 在 Routing 之前。**解决方案**：cache lookup 时只用 ModelHint::Tier 算"租户级 hint key"，命中要求是租户内任何 Explicit 模型都能服务该 Tier；二级精确匹配在 Routing 之后再做。详见 §4.2

### 3.3 Message 规范化

```rust
fn hash_message(h: &mut Sha256, msg: &Message) {
    match msg {
        Message::User { content } => {
            h.update(b"\0USER\0");
            for block in content {
                hash_content_block(h, block);
            }
        }
        Message::Assistant { content, tool_calls } => {
            h.update(b"\0ASSISTANT\0");
            for block in content {
                hash_content_block(h, block);
            }
            // tool_calls 必须按 id 排序
            let mut sorted = tool_calls.clone();
            sorted.sort_by(|a, b| a.id.cmp(&b.id));
            for call in &sorted {
                h.update(b"\0TC\0");
                h.update(call.id.as_bytes());
                h.update(b"\0");
                h.update(call.name.as_bytes());
                h.update(b"\0");
                // arguments 是 JSON,必须规范化(键排序)
                let canonical = canonical_json(&call.arguments);
                h.update(&canonical);
            }
        }
        Message::Tool { tool_call_id, content, is_error } => {
            h.update(b"\0TOOL\0");
            h.update(tool_call_id.as_bytes());
            h.update(b"\0");
            h.update(if *is_error { b"E" } else { b"O" });
            for block in content {
                hash_content_block(h, block);
            }
        }
    }
}

fn hash_content_block(h: &mut Sha256, block: &ContentBlock) {
    match block {
        ContentBlock::Text(text) => {
            h.update(b"\0T\0");
            h.update(text.as_bytes());
        }
        ContentBlock::Image { mime, data } => {
            h.update(b"\0IMG\0");
            h.update(mime.as_bytes());
            h.update(b"\0");
            // 不哈希原始图像字节(可能 MB 级),哈希 SHA-256 摘要
            h.update(data.content_hash());
        }
    }
}
```

**JSON 规范化**：必须用确定性序列化（键排序、无空格、固定数字格式），否则 `{"a":1,"b":2}` 和 `{"b":2,"a":1}` 会产生不同的 hash。Rust 生态用 `serde_json::to_string_pretty` **不可以**——用 `canonical_json` crate 或自己实现 RFC 8785。

---

## 4. CacheRegistry 抽象

### 4.1 核心 trait

```rust
#[async_trait]
pub trait CacheRegistry: Send + Sync {
    /// 多级查找 (L1 → L2)
    /// L3 不在此接口,L3 通过 lookup_l3_handle 单独查
    async fn lookup(
        &self,
        key: &CacheKey,
        policy: &CachePolicy,
    ) -> Result<Option<CachedResponse>, CacheError>;
    
    /// 写入 L1 + L2
    async fn write(
        &self,
        key: CacheKey,
        response: CachedResponse,
        policy: &CachePolicy,
        metadata: WriteMetadata,
    ) -> Result<(), CacheError>;
    
    /// 查找适配当前请求的 L3 handle (Provider 侧 cache)
    async fn lookup_l3_handle(
        &self,
        key: &CacheKey,
        provider: &ProviderId,
    ) -> Result<Option<ProviderCacheHandle>, CacheError>;
    
    /// 注册新创建的 L3 handle
    async fn register_l3_handle(
        &self,
        key: CacheKey,
        handle: ProviderCacheHandle,
        owner: SessionId,
    ) -> Result<(), CacheError>;
    
    /// 显式失效 (用于上游业务变更触发)
    async fn invalidate(&self, key: &CacheKey) -> Result<(), CacheError>;
    
    /// 租户级清理 (合规删除,GDPR 等)
    async fn purge_tenant(&self, tenant: &TenantId) -> Result<u64, CacheError>;
}

pub struct CachedResponse {
    pub response: ChatResponse,
    pub cached_at: SystemTime,
    pub origin_provider: ProviderId,
    pub original_usage: Usage,        // 原始消耗,用于"省了多少钱"统计
}

pub struct WriteMetadata {
    pub session_id: SessionId,
    pub trace_id: TraceId,
}
```

### 4.2 解决 ModelHint 二阶段问题

Cache Lookup 在 Routing 之前发生（Doc 02 的层序），此时 `req.model` 还是 `ModelHint::Tier(...)`。两阶段查找：

```rust
async fn lookup(&self, key: &CacheKey, policy: &CachePolicy) 
    -> Result<Option<CachedResponse>, CacheError> 
{
    // 阶段 1：按 Tier 查"该租户内任意模型的响应"
    if let Some(cached) = self.l1.get_by_tier_key(&key.tier_fingerprint).await {
        // 命中条件：缓存的响应来自当前 Tier 包含的某个 Explicit 模型
        return Ok(Some(cached));
    }
    // ...
}
```

**两个独立的 hash**：
- `tier_fingerprint`：用 Tier 名计算（"reasoning" / "fast"），租户内 Tier 级共享
- `explicit_fingerprint`：用 Explicit model 计算，精确匹配

**两套都存**。Lookup 阶段先查 tier_fingerprint，命中即返回（接受 Tier 内任意模型的响应）；Write 阶段同时写两套 key。这样：
- Routing 选哪个具体模型不影响命中率
- 不同 Tier 的请求互不污染（reasoning 缓存不会被 fast 命中）

代价：cache 体积翻倍。考虑到响应通常 1-10KB，可接受。

### 4.3 多级查找流程

```rust
impl CacheRegistry for DefaultCacheRegistry {
    async fn lookup(&self, key: &CacheKey, policy: &CachePolicy)
        -> Result<Option<CachedResponse>, CacheError>
    {
        // 健康度检查：缓存损坏时降级为 miss,不阻断业务
        let with_fallback = |result: Result<Option<CachedResponse>, CacheError>| {
            match result {
                Ok(v) => Ok(v),
                Err(e) => {
                    tracing::warn!(?e, ?key.debug_label, "cache lookup error, treating as miss");
                    self.metrics.record_lookup_error(&e);
                    Ok(None)
                }
            }
        };
        
        // L1
        if policy.l1 {
            if let Some(cached) = with_fallback(self.l1.get(key).await)? {
                self.metrics.record_hit(CacheLevel::L1, &cached);
                return Ok(Some(cached));
            }
        }
        
        // L2
        if policy.l2 {
            if let Some(cached) = with_fallback(self.l2.get(key).await)? {
                self.metrics.record_hit(CacheLevel::L2, &cached);
                // 回填 L1
                if policy.l1 {
                    self.l1.put(key.clone(), cached.clone(), policy.l1_ttl_or_default()).await;
                }
                return Ok(Some(cached));
            }
        }
        
        self.metrics.record_miss(key);
        Ok(None)
    }
}
```

**关键决策**：
- **缓存错误绝不传染业务**——Redis 宕机时 lookup 返回 None，请求继续走 Provider，告警但不阻断
- **L2 命中回填 L1**——下次同实例命中无需走 Redis
- **错误也要采样**——`record_lookup_error` 推动 SRE 关注

---

## 5. Cache Write 策略

### 5.1 写入时机

只有**完整成功**的响应才写入（`StopReason::EndTurn` / `StopReason::ToolUse`）：

```rust
fn should_cache(response: &ChatResponse, policy: &CachePolicy) -> bool {
    if !policy.l1 && !policy.l2 { return false; }
    
    matches!(
        response.stop_reason,
        StopReason::EndTurn | StopReason::ToolUse
    )
    // 拒绝缓存的情况：
    // - MaxTokens (响应被截断,不完整)
    // - Cancelled (上游主动中断)
    // - ContentFilter (Provider 拒答,无意义)
    // - StopSequence (语义不完整)
    // - Other (未知,保守起见不缓存)
}
```

### 5.2 写入路径（异步，不阻塞响应）

```rust
async fn write(
    &self,
    key: CacheKey,
    response: CachedResponse,
    policy: &CachePolicy,
    meta: WriteMetadata,
) -> Result<(), CacheError> {
    // L1 同步写 (本地内存,μs 级)
    if policy.l1 {
        self.l1.put(key.clone(), response.clone(), policy.l1_ttl_or_default()).await;
    }
    
    // L2 异步写 (避免阻塞响应返回)
    if policy.l2 {
        let l2 = self.l2.clone();
        let key = key.clone();
        let response = response.clone();
        let ttl = policy.l2_ttl_or_default();
        let metrics = self.metrics.clone();
        
        tokio::spawn(async move {
            if let Err(e) = l2.put(key.clone(), response, ttl).await {
                tracing::warn!(?e, ?key.debug_label, "l2 write failed");
                metrics.record_write_error(&e);
            }
        });
    }
    
    // L3 不在 write 路径处理 —— 由 ExplicitCacheProvider 在请求路径上创建,
    // 由 register_l3_handle 单独登记
    
    Ok(())
}
```

### 5.3 L3 (Provider explicit cache) 的写入路径

L3 与 L1/L2 完全不同——它在 Provider 调用过程中创建，不是事后写入：

```rust
// 上游 Cache Lookup Middleware 的逻辑 (简化)
async fn handle_l3(&self, req: &mut ChatRequest, key: &CacheKey, ctx: &RequestContext) {
    // 1. 查询是否已有可复用的 handle
    let provider_id = ctx.attributes.get("routing.provider_priority")
        .and_then(|p| p.first());
    
    if let Some(handle) = self.registry.lookup_l3_handle(key, provider_id).await? {
        // 命中：注入 directive,Provider 调用时直接 reference
        req.cache_directives.push(CacheDirective::UseExplicit { handle });
        return;
    }
    
    // 2. 未命中：判断是否值得创建 (启发式)
    if !worth_creating_l3(req) {
        return;
    }
    
    // 3. 标记请求,Provider 在响应里返回新创建的 handle id
    req.cache_directives.push(CacheDirective::MarkBoundary { 
        ttl: policy.l3_ttl_or_default() 
    });
    
    // 4. 响应到达后,从 ChatEvent::Started.cache_hit_info 提取 handle,
    //    调 register_l3_handle 登记
}

fn worth_creating_l3(req: &ChatRequest) -> bool {
    // L3 创建有成本(存储费 + 创建 latency),不是越多越好
    let prefix_size = estimate_prefix_tokens(req);
    prefix_size > 4096           // prefix 太短不值得
        && req.tools.is_empty()  // tool schema 频繁变化
}
```

### 5.4 流式写入的累积

CacheLookupMiddleware 在 inner stream 上 inspect 累积响应（Doc 02 §4.4 已展示），完成后调 `write()`：

```rust
let stream = inner_stream.inspect(move |event| {
    if let Ok(ev) = event {
        accumulator.apply_ref(ev);
        if matches!(ev, ChatEvent::Finished { .. }) {
            let response = accumulator.snapshot();
            if should_cache(&response, &policy) {
                let registry = registry.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    registry.write(key, response.into(), &policy, meta).await.ok();
                });
            }
        }
    }
});
```

**注意**：accumulator 必须复制一份原始 ChatEvent 而非引用——stream Drop 时引用失效。

---

## 6. Singleflight（防雪崩）

### 6.1 问题场景

热门请求（比如 100 个 Agent 同时调用同一个分类工具）→ 100 个并发 miss → 100 次 Provider 调用 → 100 倍成本 + 触发 rate limit。

### 6.2 Singleflight 模式

同一 key 的并发请求只允许一次到达下游，其他请求订阅这个 in-flight 操作的结果：

```rust
pub struct SingleflightGate<T> {
    inflight: Arc<DashMap<CacheKey, broadcast::Sender<Result<T, ProviderError>>>>,
}

impl<T: Clone + Send + 'static> SingleflightGate<T> {
    /// 返回 Either<Leader, Follower>
    /// Leader 必须执行真正的工作并 broadcast 结果
    /// Follower 等待 Leader 结果
    pub fn enter(&self, key: CacheKey) -> SingleflightHandle<T> {
        match self.inflight.entry(key.clone()) {
            Entry::Occupied(e) => {
                // 已有 leader,本请求是 follower
                SingleflightHandle::Follower {
                    rx: e.get().subscribe(),
                }
            }
            Entry::Vacant(e) => {
                // 我是第一个,成为 leader
                let (tx, _) = broadcast::channel(1);
                e.insert(tx.clone());
                SingleflightHandle::Leader {
                    tx,
                    key,
                    inflight: self.inflight.clone(),
                }
            }
        }
    }
}

pub enum SingleflightHandle<T> {
    Leader { 
        tx: broadcast::Sender<Result<T, ProviderError>>,
        key: CacheKey,
        inflight: Arc<DashMap<CacheKey, broadcast::Sender<Result<T, ProviderError>>>>,
    },
    Follower { 
        rx: broadcast::Receiver<Result<T, ProviderError>>,
    },
}

impl<T> Drop for SingleflightHandle<T> {
    fn drop(&mut self) {
        if let Self::Leader { key, inflight, .. } = self {
            inflight.remove(key);
        }
    }
}
```

### 6.3 集成到 Lookup 流程

```rust
async fn lookup_or_compute<F, Fut>(
    &self,
    key: CacheKey,
    policy: CachePolicy,
    compute: F,
) -> Result<CachedResponse, ProviderError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<CachedResponse, ProviderError>>,
{
    // 1. 普通 lookup
    if let Some(cached) = self.lookup(&key, &policy).await? {
        return Ok(cached);
    }
    
    // 2. Singleflight
    match self.gate.enter(key.clone()) {
        SingleflightHandle::Leader { tx, .. } => {
            // 我执行 compute,广播结果
            let result = compute().await;
            
            // 写缓存 (成功才写)
            if let Ok(ref response) = result {
                self.write(key, response.clone(), &policy, default_meta()).await.ok();
            }
            
            let _ = tx.send(result.clone().map_err(|e| e.clone()));
            result
        }
        SingleflightHandle::Follower { mut rx } => {
            // 等 leader 结果
            match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) | Err(_) => {
                    // Leader 死了或超时 → 降级,自己再 compute 一次
                    tracing::warn!("singleflight follower fallback to direct compute");
                    compute().await
                }
            }
        }
    }
}
```

### 6.4 与流式响应的兼容

上面的 `compute` 返回的是 `CachedResponse`（完整响应，非 stream）。这意味着 Singleflight **将流式调用降级为非流式**——follower 拿到的是 leader 写入缓存后的完整响应。

代价：follower 失去了流式 TTFT 优势——必须等 leader 完整生成完才能拿到结果。

权衡：
- 对延迟敏感且非热门：跳过 singleflight，每个请求独立调 Provider
- 对成本敏感且热门：开启 singleflight，接受 follower 的延迟代价
- 默认策略：当 in-flight 数量 > 5 时自动启用 singleflight（"被打爆才进保护"）

---

## 7. Provider-side Cache 生命周期管理

### 7.1 Handle 注册表（内容寻址 + 引用计数）

**核心语义**：L3 handle 是**内容寻址**的——同一个 `CacheKey` 在同一 Provider 上只创建一次,被多个并发 session 共享。这是为什么 §3.2 强调 CacheKey 必须包含 TENANT + IAM_SCOPES：在隔离边界内最大化复用,在隔离边界外强制独立。

跨 session 共享 → **不能用单 session 所有权**,必须用引用计数管理生命周期：

```rust
pub struct L3HandleRegistry {
    /// (key, provider) -> handle record
    /// 同一语义 key 在不同 Provider 上独立存在
    by_key: Arc<DashMap<(CacheKey, ProviderId), Arc<L3HandleRecord>>>,
    
    /// 反向索引：session 持有的所有 handle 引用 (用于 session 退出时批量 release)
    by_session: Arc<DashMap<SessionId, HashSet<L3HandleId>>>,
    
    /// LRU 索引：按 last_used_at 排序 (用于配额淘汰候选)
    lru: Arc<Mutex<LruOrder>>,
    
    /// 持久化后端 (Postgres) - 防止进程重启丢失
    persistent: Arc<dyn L3Persistence>,
}

pub struct L3HandleRecord {
    pub id: L3HandleId,
    pub provider: ProviderId,
    pub external_id: String,                  // Provider 侧句柄,如 "cachedContents/abc123"
    pub key: CacheKey,
    pub tenant: TenantId,
    pub size_estimate_bytes: u64,
    pub created_at: SystemTime,
    pub expires_at: SystemTime,
    
    /// 当前引用本 handle 的 session 集合
    /// 共享语义：同 tenant + 同 IAM scopes + 同 content → 同一 handle
    pub referencing_sessions: RwLock<HashSet<SessionId>>,
    
    pub last_used_at: AtomicSystemTime,
    pub usage_count: AtomicU64,
    
    /// 标记为待删除 (ref_count 为 0 后进入此状态,等待 grace period)
    pub pending_eviction_since: RwLock<Option<SystemTime>>,
}

impl L3HandleRecord {
    pub fn ref_count(&self) -> usize {
        self.referencing_sessions.read().unwrap().len()
    }
}
```

### 7.2 Acquire / Release 协议

```rust
impl L3HandleRegistry {
    /// 获取或创建 handle。命中已有 handle 时增加引用计数
    pub async fn acquire(
        &self,
        key: CacheKey,
        provider_id: &ProviderId,
        session: SessionId,
        creator: impl FnOnce() -> BoxFuture<'_, Result<ProviderCacheHandle, ProviderError>>,
    ) -> Result<L3HandleId, CacheError> {
        // 1. 查找已有 handle
        if let Some(record) = self.by_key.get(&(key.clone(), provider_id.clone())) {
            // 共享：增加引用,清除 pending_eviction 标记
            let mut sessions = record.referencing_sessions.write().unwrap();
            sessions.insert(session.clone());
            *record.pending_eviction_since.write().unwrap() = None;
            
            self.by_session.entry(session).or_default().insert(record.id.clone());
            self.metrics.record_l3_share_hit(&record);
            return Ok(record.id.clone());
        }
        
        // 2. 未找到 → singleflight 创建 (防止并发 acquire 同一 key 时创建多份)
        let handle = self.creation_gate.run(key.clone(), creator).await?;
        
        let record = Arc::new(L3HandleRecord {
            id: L3HandleId::new(),
            provider: provider_id.clone(),
            external_id: handle.external_id,
            key: key.clone(),
            tenant: handle.tenant_namespace.parse().unwrap(),
            size_estimate_bytes: handle.size_estimate.unwrap_or(0),
            created_at: handle.created_at,
            expires_at: handle.expires_at,
            referencing_sessions: RwLock::new([session.clone()].into()),
            last_used_at: AtomicSystemTime::new(SystemTime::now()),
            usage_count: AtomicU64::new(0),
            pending_eviction_since: RwLock::new(None),
        });
        
        self.by_key.insert((key, provider_id.clone()), record.clone());
        self.by_session.entry(session).or_default().insert(record.id.clone());
        self.persistent.save(&record).await?;
        
        Ok(record.id.clone())
    }
    
    /// 释放引用。引用计数归零时进入 pending_eviction 状态
    /// 不立即删除 —— 给 grace period (默认 5 min) 让短时间内的复用还有机会
    pub async fn release(
        &self,
        handle_id: &L3HandleId,
        session: &SessionId,
    ) -> Result<(), CacheError> {
        let record = self.by_id(handle_id)?;
        let mut sessions = record.referencing_sessions.write().unwrap();
        sessions.remove(session);
        
        if sessions.is_empty() {
            *record.pending_eviction_since.write().unwrap() = Some(SystemTime::now());
        }
        
        self.by_session.get_mut(session).map(|mut s| s.remove(handle_id));
        Ok(())
    }
    
    /// Session 完全结束时,批量 release 该 session 持有的所有 handle
    pub async fn release_session(&self, session: &SessionId) -> Result<usize, CacheError> {
        let handles: Vec<_> = self.by_session.remove(session)
            .map(|(_, set)| set.into_iter().collect())
            .unwrap_or_default();
        
        for handle_id in &handles {
            self.release(handle_id, session).await.ok();
        }
        Ok(handles.len())
    }
    
    /// 标记 handle 已被使用 (用于 LRU 排序和 metrics)
    pub fn mark_used(&self, handle_id: &L3HandleId) {
        if let Ok(record) = self.by_id(handle_id) {
            record.last_used_at.store(SystemTime::now());
            record.usage_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}
```

**核心不变量**：
1. 同一个 `(CacheKey, ProviderId)` 在 Registry 中**最多存在一个 record**——通过 `by_key` 的 DashMap 原子性保证
2. 创建 handle 是 singleflight 的——并发 acquire 同一未知 key 时,只有一个会真正调 Provider create_cache,其他等待结果并复用
3. Session 释放是幂等的——重复 release 同一 (handle, session) 不会出错
4. ref_count 归零进入 pending_eviction,**不立即删除**——给 grace period 容忍"刚释放就有新请求进来"的常见模式

### 7.3 跨进程一致性

进程重启后，内存中的 `by_key` / `by_session` 索引清零。但 Provider 侧的 cache 对象仍然存在（按它们自己的 TTL）——如果不能在重启后重建索引，就会**泄漏 Provider 侧资源（仍在花钱）**。

解决：
1. **持久化到 Postgres**：每次 register / extend / delete 都同步到 DB
2. **启动时 reload**：进程启动时从 DB 读所有未过期 handle 重建内存索引
3. **periodic reconciliation**：每小时跟 Provider 对账（List API），删除内存里有但 Provider 已经过期的，告警 Provider 有但内存里没的（潜在泄漏）

```rust
async fn reconcile(&self) -> Result<ReconcileReport, CacheError> {
    let mut report = ReconcileReport::default();
    
    for provider_id in self.providers.list_explicit_cache_capable() {
        let provider = self.providers.get_explicit_cache(&provider_id)?;
        let remote = provider.list_caches().await?;        // 调 Provider List API
        let local: HashSet<_> = self.by_key.iter()
            .flat_map(|entry| entry.value().iter()
                .filter(|r| r.provider == provider_id)
                .map(|r| r.external_id.clone()))
            .collect();
        
        // 远端有,本地无 → 泄漏 (可能是其他实例创建的或者本实例之前崩溃没清理)
        for remote_id in &remote {
            if !local.contains(remote_id) {
                report.orphans.push((provider_id.clone(), remote_id.clone()));
            }
        }
        
        // 本地有,远端无 → 已过期,清理本地索引
        for record in self.records_for_provider(&provider_id) {
            if !remote.contains(&record.external_id) {
                self.remove_local_index(&record.id).await;
                report.cleaned_local += 1;
            }
        }
    }
    
    // 孤儿处理策略：跨实例可能误判 → 给 30 分钟宽限期再删
    self.schedule_orphan_cleanup(report.orphans.clone(), Duration::from_mins(30));
    
    Ok(report)
}
```

---

## 8. Janitor（主动回收）

Janitor 是后台 task,循环执行四类清理。**核心修正**：所有 L3 删除都必须先检查引用计数,只有 ref_count == 0 且超过 grace period 的 handle 才进入淘汰候选。Session 闲置不能直接触发删除——它只是触发 release,真正的删除取决于全局引用情况。

```rust
pub struct CacheJanitor {
    registry: Arc<DefaultCacheRegistry>,
    providers: Arc<ProviderRegistry>,
    config: JanitorConfig,
}

pub struct JanitorConfig {
    pub tick_interval: Duration,                 // 主循环间隔,默认 60s
    pub session_idle_timeout: Duration,          // session 闲置阈值,触发 release,默认 15min
    pub eviction_grace_period: Duration,         // ref_count=0 后的宽限期,默认 5min
    pub l3_storage_quota_bytes: u64,             // L3 总存储配额
    pub l3_storage_high_water: f64,              // 触发 LRU 淘汰的水位 (默认 0.8)
    pub l3_storage_low_water: f64,               // LRU 淘汰停止水位 (默认 0.6)
    pub reconcile_interval: Duration,            // 与 Provider 对账,默认 1h
}

impl CacheJanitor {
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.config.tick_interval);
        let mut last_reconcile = Instant::now();
        
        loop {
            tick.tick().await;
            
            // 1. Idle session 触发 release (不一定立即删除 handle)
            self.release_idle_sessions().await;
            
            // 2. ref_count=0 且过 grace period 的 handle → 实际删除
            self.evict_unreferenced_handles().await;
            
            // 3. LRU 淘汰 (容量超水位时,可强制删除即使 ref_count > 0)
            self.evict_lru_if_over_quota().await;
            
            // 4. 过期清理 (TTL 已过)
            self.evict_expired().await;
            
            // 5. 对账 (低频)
            if last_reconcile.elapsed() >= self.config.reconcile_interval {
                self.registry.reconcile().await.ok();
                last_reconcile = Instant::now();
            }
        }
    }
    
    /// Idle session 触发 release —— 不直接删 handle,只解除该 session 的引用
    /// 实际删除取决于其他 session 是否还在引用 (ref counting)
    async fn release_idle_sessions(&self) {
        let cutoff = SystemTime::now() - self.config.session_idle_timeout;
        let stale_sessions: Vec<_> = self.registry.sessions_idle_since(cutoff).collect();
        
        for session_id in stale_sessions {
            let released = self.registry.release_session(&session_id).await.unwrap_or(0);
            self.metrics.record_session_released(&session_id, released);
        }
    }
    
    /// 真正删除：ref_count == 0 且过了 grace period
    async fn evict_unreferenced_handles(&self) {
        let candidates = self.registry.unreferenced_for(self.config.eviction_grace_period);
        for record in candidates {
            self.delete_l3_handle_force(&record.id).await.ok();
        }
    }
    
    /// LRU 淘汰：容量超过水位时,即使 ref_count > 0 也要强制删除
    /// 这是最后的资金/容量防线 (LRU 优先选择 ref_count 低 + last_used 早的)
    async fn evict_lru_if_over_quota(&self) {
        let total_size = self.registry.total_l3_size();
        let high = (self.config.l3_storage_quota_bytes as f64 * self.config.l3_storage_high_water) as u64;
        if total_size < high {
            return;
        }
        
        let target = (self.config.l3_storage_quota_bytes as f64 * self.config.l3_storage_low_water) as u64;
        let mut freed = 0u64;
        
        // LRU 排序：优先 ref_count == 0,其次 last_used_at 最早
        for record in self.registry.lru_iter_weighted() {
            if total_size - freed <= target { break; }
            
            // 强制淘汰 ref > 0 的 handle 是有代价的 —— 引用方下次会 cache miss 并重建
            if record.ref_count() > 0 {
                tracing::warn!(
                    handle = ?record.id,
                    ref_count = record.ref_count(),
                    "force evicting referenced handle due to quota pressure"
                );
                self.metrics.record_forced_eviction(&record);
                
                // 通知所有引用 session: 你的 handle 即将失效
                for session in record.referencing_sessions.read().unwrap().iter() {
                    self.notify_handle_invalidation(session, &record.id);
                }
            }
            
            let size = record.size_estimate_bytes;
            if self.delete_l3_handle_force(&record.id).await.is_ok() {
                freed += size;
            }
        }
    }
    
    async fn delete_l3_handle_force(&self, handle_id: &L3HandleId) -> Result<(), CacheError> {
        let record = self.registry.get_handle(handle_id)
            .ok_or(CacheError::HandleNotFound)?;
        let provider = self.providers.get_explicit_cache(&record.provider)?;
        
        let provider_handle = ProviderCacheHandle {
            provider: record.provider.clone(),
            external_id: record.external_id.clone(),
            tenant_namespace: record.tenant.to_string(),
            created_at: record.created_at,
            expires_at: record.expires_at,
        };
        
        // Provider delete 失败时仍要从本地索引移除? 不行——会造成泄漏
        // 策略：标记为 "pending_delete",下次 reconcile 时重试
        match provider.delete_cache(&provider_handle).await {
            Ok(_) => {
                self.registry.remove_handle(handle_id);
                self.metrics.record_eviction(&record);
                Ok(())
            }
            Err(e) if e.is_not_found() => {
                // Provider 那边已经删了 (TTL 自然到期),本地清理就行
                self.registry.remove_handle(handle_id);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(?e, ?handle_id, "l3 delete failed, will retry");
                self.registry.mark_pending_delete(handle_id);
                Err(e.into())
            }
        }
    }
}
```

---

### 8.1 LRU 加权排序（淘汰优先级）

强制淘汰 referenced handle 是高成本操作（被淘汰的 session 下次 acquire 会 miss → 重建），所以 LRU 排序不能只看时间，必须**加权**：

```rust
fn eviction_priority(record: &L3HandleRecord) -> EvictionScore {
    let ref_count = record.ref_count() as i64;
    let idle_secs = record.last_used_at.elapsed().as_secs() as i64;
    let usage_count = record.usage_count.load(Ordering::Relaxed) as i64;
    
    // 越大越优先淘汰
    EvictionScore(
        // ref_count = 0 的优先级最高 (无副作用)
        if ref_count == 0 { 1_000_000 } else { 0 }
        // 闲置越久越优先
        + idle_secs
        // 使用次数越少越优先 (避免淘汰热点)
        - usage_count.saturating_mul(10)
    )
}
```

---

## 9. 失效与一致性

### 9.1 显式失效场景

- 上游 IAM 变更（用户被移出某项目 → 该项目的 cache 必须立即失效）
- 业务数据变更（被 review 的代码仓库有新 commit → 旧 review 缓存过时）
- 用户主动"重新生成"
- 合规删除（GDPR / 租户注销 → 该租户全部 cache 必须删）

### 9.2 失效粒度

```rust
#[async_trait]
pub trait CacheInvalidation {
    async fn invalidate_key(&self, key: &CacheKey) -> Result<()>;
    async fn invalidate_session(&self, session: &SessionId) -> Result<u64>;
    async fn invalidate_tenant(&self, tenant: &TenantId) -> Result<u64>;
    async fn invalidate_by_resource(&self, resource: &ResourceRef) -> Result<u64>;
}
```

### 9.3 跨实例失效

L1 是进程内缓存，单实例失效不够——必须广播给所有实例：

```rust
// 用 Redis pub/sub 实现跨实例失效广播
pub struct InvalidationBroadcaster {
    redis: Arc<RedisClient>,
    channel: String,                 // 例如 "tars:cache:invalidate"
    local_l1: Arc<L1Cache>,
}

impl InvalidationBroadcaster {
    pub async fn broadcast(&self, event: InvalidationEvent) -> Result<()> {
        // 1. 本地立即应用
        self.apply_local(&event).await;
        
        // 2. Redis L2 立即删
        self.delete_from_l2(&event).await;
        
        // 3. 广播给其他实例
        let payload = serde_json::to_string(&event)?;
        self.redis.publish(&self.channel, payload).await?;
        Ok(())
    }
    
    pub async fn run_subscriber(self: Arc<Self>) {
        let mut sub = self.redis.subscribe(&self.channel).await.unwrap();
        while let Some(msg) = sub.next().await {
            if let Ok(event) = serde_json::from_str::<InvalidationEvent>(&msg.payload) {
                self.apply_local(&event).await;
            }
        }
    }
}
```

### 9.4 最终一致性的诚实性

L1 跨实例失效有窗口（pub/sub 延迟通常 <100ms，但不保证）。这意味着：
- 同一 cache key 被失效后，可能有 < 100ms 的窗口内某实例仍返回旧响应
- 这对 LLM 缓存是可接受的——缓存内容本身就是过去某次 LLM 决策的快照
- 对**安全敏感的失效**（IAM 变更）必须强一致：应在 IAM 决策点同步检查权限版本，不能依赖 cache 失效广播

---

## 10. 跨租户隔离的安全保证

这是这个 Cache Registry 设计的核心安全性，从前面对话中的讨论提取的核心要求：

### 10.1 三道防线

**防线 1：CacheKey 命名空间前置**（§3.2）
TENANT + IAM SCOPES 是哈希的第一批字节，租户 A 与租户 B 即使 prompt 完全相同，key 也必然不同。

**防线 2：IAM 必须在 Cache Lookup 之前**（Doc 02 §4.2 强约束）
即使绕过 CacheKey 隔离（比如某个 bug），IAM 已经先拒绝了无权限的请求，根本不会进入 Cache 层。

**防线 3：CacheKey 反向校验**（防御深度）
Cache 命中时，再做一次廉价的所有权检查：

```rust
async fn lookup(&self, key: &CacheKey, policy: &CachePolicy) -> Result<Option<CachedResponse>, CacheError> {
    let cached = match self.l1.get(key).await? {
        Some(c) => c,
        None => return Ok(None),
    };
    
    // 防御性检查：cached 的 origin tenant 必须与当前 key 包含的 tenant 一致
    // 理论上不可能不一致 (因为 key 已经包含 tenant),但作为最后一道防线
    if !key.tenant_matches(&cached.cached_at_tenant) {
        tracing::error!(
            ?key.debug_label,
            cached_tenant = ?cached.cached_at_tenant,
            "CACHE TENANT MISMATCH - possible key collision or corruption"
        );
        self.metrics.record_security_alert("tenant_mismatch");
        return Ok(None);   // 当作 miss 处理,触发重新生成
    }
    
    Ok(Some(cached))
}
```

### 10.2 Provider-side Handle 的隔离

L3 handle 不能跨租户复用——`tenant_namespace` 字段强制：

```rust
pub struct ProviderCacheHandle {
    pub provider: ProviderId,
    pub external_id: String,
    pub tenant_namespace: String,    // 必填,Provider 适配器拒绝接受空值
    // ...
}
```

注入 L3 handle 到请求时，CacheLookupMiddleware 必须验证：

```rust
fn validate_l3_use(handle: &ProviderCacheHandle, ctx: &RequestContext) -> Result<()> {
    if handle.tenant_namespace != ctx.tenant_id.to_string() {
        return Err(CacheError::TenantMismatch);
    }
    Ok(())
}
```

### 10.3 系统 Prompt 注入 tenant marker

如前面讨论提到，Provider-side prefix cache 在 Provider 内部按 prompt 字节哈希。即使我们的 Registry 隔离干净，如果两个租户的 system prompt 字节级相同，Provider 自己的隐式缓存可能命中同一 entry——虽然不会泄漏内容，但可能产生计费归属错乱和性能侧信道（攻击者通过 TTFT 推断别人是否问过类似问题）。

**强制规则**：所有 Provider 适配器在构造请求时，自动在 system prompt 头部插入：

```
<!--
tenant_marker: {tenant_id_hash_first_8_chars}
session_marker: {session_id_hash_first_8_chars}
-->
```

这一行的存在让不同租户的 prefix 字节级不同，Provider 的隐式缓存自然隔离。这个注入在 Provider 适配器层完成，不在 Cache 层——因为这是"绕过 Provider 自身缓存的隔离漏洞"，与我们的 Cache Registry 无关。

### 10.4 Cache 句柄绝不外泄

Doc 01 §10 已规定：`ProviderCacheHandle.external_id` 是 Provider 侧的"不记名提货凭证"——任何持有该 ID 的请求都能用这份缓存。因此：

- 绝不返回给前端 / 客户端 / 日志（debug 模式下也只输出 hash 摘要）
- API 返回值里只暴露我们自己的 `L3HandleId`（不透明 UUID），需要使用时由 Registry 翻译为真实 external_id

---

## 10.5 跨 Session 共享与 Prompt 分层（设计指引）

Cache Registry 的 Key 构造（§3.2）已经决定了**共享边界 = Tenant ∩ IAM Scopes ∩ Model ∩ Static Content**——边界内最大化共享，边界外强制独立。这意味着上层（Agent Runtime / Prompt 拼装）必须按以下原则组织 prompt 才能让缓存真正生效：

### Prompt 三段式分层（命名避开 L1/L2/L3 冲突）

| 段位 | 内容 | 变化频率 | 缓存策略 |
|---|---|---|---|
| **Static Prefix (冷)** | System Prompt、Agent 角色定义、JSON Schema、全局工具规范 | 月级变动 | 必须放最前；进入 CacheKey；适合 L3 explicit cache |
| **Project Anchor (温)** | 当前项目的代码库快照（绑定 commit hash）、API 文档、依赖图 | 随 commit 变动 | 接 Static Prefix；进入 CacheKey；适合 L3 |
| **Dynamic Suffix (热)** | 用户当前 PR diff、即时对话、实时日志 | 每次请求都变 | **绝对不进 CacheKey**；普通调用附加 |

### 为什么这样分层能让跨 session 共享生效

- 总监 A 与实习生 B 在同一租户下、同时审查同一 commit `a1b2c3` → Static Prefix + Project Anchor 完全相同 → CacheKey 哈希相同 → §7.2 的 acquire 自动返回同一 L3 handle，引用计数 = 2
- 两人审查的 PR diff 不同 → Dynamic Suffix 不同 → LLM 输出不同 → L1/L2 响应缓存不会跨 PR 命中（这是对的，不应该）
- 但 L3 prefix cache 命中率 100%，input token 享受降价

### 上层的责任

Cache Registry 不负责 prompt 拼装顺序——这是 Agent Runtime 的工作。Cache Registry 只承诺：**只要你拼出的 Static 部分字节级相同（且其他参数也一致），我就能跨 session 共享**。

这意味着 Agent Runtime 必须遵守：
1. Static Prefix 的拼装顺序在配置变更前**永不改动**——加一个空格、调整一个 Markdown 标题，就会导致全公司的 cache 全部 miss
2. Static Prefix 不能包含动态变量（时间戳、请求 ID、随机生成的实例标识）——这些必须移到 Dynamic Suffix
3. Project Anchor 必须显式标注 commit/version——`Codebase` 是动态概念，`Codebase@a1b2c3` 是静态概念
4. Tools schema 如果不变化（同一项目内通常如此），应该作为 Static Prefix 的一部分

Agent Runtime 的 Prompt Builder 应该提供 `static_prefix() -> String` 和 `dynamic_suffix() -> String` 两个方法，Cache Lookup 只哈希前者。这个契约在 Doc 04（Agent Runtime）中详细约束。

---

## 11. 配置形态

```toml
[cache]
enabled = true
hasher_version = 1                      # bump 这个值一键作废全部 L1/L2

[cache.l1]
backend = "moka"
max_capacity = 10000
default_ttl_secs = 600
weighted_eviction = true                # 按 response 字节数加权

[cache.l2]
backend = "redis"
url = "redis://redis-cluster:6379"
default_ttl_secs = 86400
key_prefix = "tars:cache:"
compression = "zstd"                    # 大响应压缩,节省 50-70% 存储
serialization = "messagepack"

[cache.l3]
enabled = true
storage_quota_bytes = 10737418240       # 10 GB
high_water = 0.8
low_water = 0.6
default_ttl_secs = 3600

[cache.singleflight]
enabled = true
auto_threshold_inflight = 5             # in-flight > 5 时自动启用
follower_timeout_secs = 120

[cache.janitor]
tick_interval_secs = 60
session_idle_timeout_secs = 900
reconcile_interval_secs = 3600

[cache.invalidation]
broadcast_channel = "tars:cache:invalidate"

# 租户级覆盖
[tenants.acme_corp.cache]
l3_storage_quota_bytes = 53687091200    # 50 GB,大客户更高配额

# 安全旋钮 (生产中通常不动)
[cache.security]
strict_tenant_check = true              # 启用 §10.1 防线 3
require_iam_scopes = true               # 缺少 iam.allowed_scopes 直接报错(否则 fallback 为空)
```

---

## 12. 指标与可观测性

必须采集的 metrics：

| Metric | Type | Labels | 用途 |
|---|---|---|---|
| `cache.lookup.total` | counter | level, tenant, hit/miss | 命中率追踪 |
| `cache.lookup.latency` | histogram | level | L1/L2/L3 性能监控 |
| `cache.write.total` | counter | level, tenant | 写入量 |
| `cache.write.errors` | counter | level, error_kind | L2 故障告警 |
| `cache.evictions` | counter | reason (idle/lru/expired/explicit) | 容量调优 |
| `cache.l3.handles` | gauge | provider, tenant | L3 数量监控 |
| `cache.l3.storage_bytes` | gauge | provider, tenant | 容量水位 |
| `cache.l3.cost_saved_usd` | counter | provider, tenant | ROI 证明 |
| `cache.singleflight.coalesced` | counter | tenant | 雪崩防护效果 |
| `cache.security.alerts` | counter | kind | 异常告警 (tenant_mismatch 等) |
| `cache.invalidation.broadcast` | counter | scope | 失效事件 |

**节省成本的核算公式**：

```
cost_saved = sum(cached_response.original_usage 按当前 model 的定价) - 0
```

每次 L1/L2 命中时累加，反映"如果不命中需要花多少钱"。这是说服管理层投入缓存基建的关键 metric。

---

## 13. 反模式清单

1. **不要把 prompt 作为 cache key 的全部输入**——必须包含 TENANT + IAM_SCOPES + MODEL + 决定输出的所有参数。
2. **不要把 ProviderCacheHandle.external_id 暴露给前端 / API 调用方**——它是不记名凭证，泄漏 = 跨租户访问漏洞。
3. **不要在 cache 命中时省略 IAM 检查**——IAM 必须在 Lookup 之前发生（Doc 02 §4.2），这是安全防线。
4. **不要在写入路径上同步阻塞**——L2 写必须 spawn 异步任务，不阻塞响应返回。
5. **不要缓存非完整响应**——MaxTokens / Cancelled / ContentFilter 不进缓存。
6. **不要让 cache 错误传染业务**——Redis 宕机时降级为 miss，不阻断 LLM 调用。
7. **不要在没 hasher_version 的情况下变更 hash 算法**——升级算法时必须 bump version 一键失效旧缓存。
8. **不要相信 Provider 的 TTL 自动过期**——主动 delete 才是真正的成本控制（§8 Janitor）。
9. **不要让 Janitor 因为单个 delete 失败而停止工作**——失败标记 pending_delete，下次重试。
10. **不要假设 JSON 序列化是确定性的**——不同库 / 不同运行时输出可能不同字节，必须用 RFC 8785 风格的 canonical JSON。
11. **不要在 cache key 里包含 trace_id / timestamp / request_id**——这些每次都不同，必然 0% 命中率。
12. **不要让 singleflight 的 follower 等待超过合理时间**——Leader 死了 follower 自己 fallback，不能无限等待。
13. **不要混用不同 hasher_version 的缓存**——key 字节相同但语义不同，会读到错误数据。
14. **不要在 Cache Registry 里维护"业务对象 → cache key"的映射**——业务对象的失效应该通过 ResourceRef 触发 invalidate_by_resource，而不是 Cache 层反向跟踪业务依赖。

---

## 14. 与上下游的契约

### 上游 (CacheLookupMiddleware) 承诺

- 调用 lookup 之前，IAM 已经决策完毕，`ctx.attributes["iam.allowed_scopes"]` 已写入
- 调用 write 时，response 是完整的（已经过 `should_cache()` 过滤）
- ChatRequest 的 `cache_directives` 字段由本层填充，不修改其他字段

### 下游 (ExplicitCacheProvider) 承诺

- `create_cache` 返回的 handle 立即可用
- `delete_cache` 是幂等的（多次删除同一 handle 返回 NotFound 而非 error）
- `list_caches` 用于 Janitor reconcile，返回该账号下所有未过期的 cache external_id

### 跨实例契约

- 所有实例共享 L2 (Redis)
- L1 失效通过 Redis pub/sub 广播
- L3 handle registry 的真值在 Postgres，每个实例的内存索引是只读副本，重启后 reload

---

## 15. 待办与开放问题

- [ ] 确定 canonical JSON 实现：自研 vs `cjson-rs` vs `jcs` (RFC 8785)
- [ ] L2 Redis 的高可用方案：Sentinel vs Cluster
- [ ] L3 storage_quota 的多 Provider 维度建模（Gemini quota 与 Anthropic quota 是独立的）
- [ ] 跨数据中心的 L2 部署策略（每个 DC 独立 Redis vs 全局 Redis vs CRDT）
- [ ] L3 reconcile 在 Provider List API 不存在时的降级（Gemini 提供 List，Anthropic ephemeral cache 不提供）
- [ ] 缓存预热：是否值得为高频 prompt 提前生成 L3 handle
- [ ] cache_saved_usd metric 的统计精度（命中时使用原始 model 的当前定价 vs 命中时刻定价）
