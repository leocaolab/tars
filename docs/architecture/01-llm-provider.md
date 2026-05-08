# 文档 01 — LLM Provider 抽象设计

> 范围：定义统一的 LLM Provider 抽象层，支持 OpenAI / Anthropic / Gemini 的 API + CLI 双通道，以及本地推理引擎（vLLM、llama.cpp、嵌入式 mistral.rs、ONNX 分类器）。
>
> 不在本文档范围：上层 Agent 编排、Cache Registry、Middleware Pipeline——这些消费 Provider 接口，不在 Provider 层定义。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **后端无感** | 上层 Agent 代码切换 provider 不需要改业务逻辑，只改配置 |
| **能力协商** | 不同 provider 支持的特性差异极大（thinking、cache、tool use、structured output），通过 Capability 描述符让上层做出正确决策 |
| **CLI 复用本地认证** | 调用用户本机已登录的 `claude` / `gemini` CLI,零密钥管理,零运维 |
| **本地后端零开销** | 嵌入式推理（mistral.rs、ONNX）走同一抽象,但不进网络栈 |
| **流式与非流式统一** | 内部只有 stream，complete = stream + collect |
| **错误可分类** | 区分可重试 / 不可重试 / 内容过滤,让上层重试策略可写 |
| **Cancel 安全** | 上层 Drop 流时,底层资源（HTTP 连接、CLI 子进程 stdout 缓冲）必须被干净处理 |

**反目标**（明确不做的事）：
- 不在 Provider 层做 prompt 拼装、模板渲染、RAG 检索——这些是上层职责
- 不暴露 provider-specific 概念（`thinking_budget` 是 Anthropic 独有,不该出现在通用 trait 中）
- 不在 trait 里维护 conversation 状态——所有调用是无状态的,上层负责传完整 message 列表
- 不做 token 级精确预算控制——那是 Middleware 的事,Provider 只汇报 usage

---

## 2. 后端矩阵

按集成方式分三大类,共 9 种典型后端：

| 类别 | 后端 | 协议 | 认证 | Tool Use | Structured Output | Prompt Cache | 流式 |
|---|---|---|---|---|---|---|---|
| HTTP API | OpenAI | REST/SSE | Bearer | ✅ strict mode | ✅ strict JSON Schema | 隐式 (>1024 prefix) | ✅ SSE |
| HTTP API | Anthropic | REST/SSE | x-api-key | ✅ tool use | 通过 tool 实现 | ✅ 显式 cache_control | ✅ SSE |
| HTTP API | Gemini | REST/SSE | API key / ADC | ✅ functionCalling | ✅ responseSchema | ✅ 显式 cachedContent | ✅ SSE |
| HTTP API | OpenAI 兼容（vLLM / llama.cpp server / LM Studio / Groq / Together） | REST/SSE | none / Bearer | 部分支持 | 部分支持 | 取决于实现 | ✅ |
| HTTP API | Ollama | 自有 + OpenAI 兼容 | none | ✅ | ✅ | ❌ | ✅ |
| CLI 子进程 | Claude Code (`claude`) | JSONL over stdio | 委托（用户已登录） | ✅ | ✅ | 委托 | ✅ |
| CLI 子进程 | Gemini CLI (`gemini`) | 文本 stdout | 委托 | 通过 MCP | 部分 | 委托 | 部分 |
| 嵌入式 | mistral.rs | 进程内 FFI | n/a | ✅ | ✅ | ❌ | ✅ |
| 嵌入式 | ONNX Runtime（仅分类） | 进程内 | n/a | ❌ | ❌ | ❌ | ❌ |

**重要观察**：
- vLLM / llama.cpp server / LM Studio / Groq / Together / DeepSeek 全部走 OpenAI 兼容协议——意味着 OpenAIProvider 只要支持 `base_url` 覆盖就能复用,不需要 N 个独立实现。
- ONNX 后端不进 chat trait,单独走 `ClassifierProvider`（用于 PromptGuard）。

---

## 3. 核心 Trait

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn capabilities(&self) -> &Capabilities;

    /// 唯一的核心方法：返回事件流
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError>;

    /// 默认实现：消费 stream 并聚合
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<ChatResponse, ProviderError> {
        let mut s = self.stream(req, ctx).await?;
        let mut acc = ChatResponseBuilder::new();
        while let Some(ev) = s.next().await {
            acc.apply(ev?);
        }
        acc.finish()
    }

    /// 预估 token 数。fast=true 时允许返回估算值,false 时调 provider 的 count_tokens API
    async fn count_tokens(&self, req: &ChatRequest, fast: bool) -> Result<u64, ProviderError>;

    /// 根据 usage 计算成本（USD）。本地后端返回 0
    fn cost(&self, usage: &Usage) -> CostUsd;
}
```

### 3.1 关于 `BoxStream<'static>` 的生命周期约束

返回 `BoxStream<'static, ...>` 意味着流在被轮询时**不能借用** `&self`——HTTP 字节流、SSE 解析器、MPSC 接收器都被包进这个 stream,任何 provider 内部状态必须 move 进去。

**统一的实现模式**：trait 接收 `self: Arc<Self>`,所有实现内部用 `Arc` 持有可共享状态。`stream` 方法第一行 `let this = self.clone();`,然后把 `this` 移进返回的 `async-stream` 块：

```rust
impl LlmProvider for OpenAIProvider {
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let this = self.clone();
        let auth = this.auth_resolver.resolve(&this.config.auth, &ctx).await?;
        let url = this.build_url(&req);
        let body = this.translate_request(&req)?;
        
        // 整个 reqwest 调用在 stream 体内执行,reqwest::Response 的生命周期与 stream 绑定
        let stream = async_stream::try_stream! {
            let resp = this.http_client
                .post(url)
                .headers(this.build_headers(&auth))
                .json(&body)
                .send()
                .await
                .map_err(ProviderError::from)?;
            
            this.check_status(&resp)?;
            
            let mut sse = SseDecoder::new(resp.bytes_stream());
            let mut tool_buffer = ToolCallBuffer::new();   // 见 §8.1
            
            while let Some(frame) = sse.next().await {
                if let Some(event) = this.parse_event(frame?, &mut tool_buffer)? {
                    yield event;
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}
```

**约束**：所有 LlmProvider 实现的内部状态必须满足"通过 `Arc<Self>::clone()` 后所有操作仍可执行"——即 `&self` 上的方法都能在 `Arc<Self>` 上调用。reqwest::Client、AuthResolver、配置 struct 全部 wrap 在 Arc 里。

### 3.2 关键决策

- **stream 是基础,complete 是派生**——避免两套代码路径,避免上层"流式时丢 usage"这种 bug。
- **Item 是 `Result<ChatEvent, ProviderError>`**——而非 `ChatEvent`。流中途出错（连接断开、SSE 格式错误）也是常态,不能用 panic 表达。
- **不暴露同步版本**——所有 provider 都是 async,包括嵌入式（mistral.rs 推理本身是同步的,wrap 在 `spawn_blocking` 里）。
- **`RequestContext` 携带 trace_id / tenant_id / budget**——分离业务参数（ChatRequest）和运行时元数据（RequestContext）,让 provider 可以做 tracing 但不需要侵入业务字段。

---

## 4. 规范化的请求 / 响应模型

### 4.1 ChatRequest

```rust
pub struct ChatRequest {
    pub model: ModelHint,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub structured_output: Option<JsonSchema>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stop_sequences: Vec<String>,
    pub seed: Option<u64>,
    pub cache_directives: Vec<CacheDirective>,
    pub thinking: ThinkingMode,
}

pub enum ModelHint {
    Explicit(String),
    Tier(ModelTier),
    Ensemble(Vec<ModelHint>),
}

pub enum ModelTier {
    Reasoning,      // 顶级模型 (Opus / o1 / Gemini Pro)
    Default,        // 主力 (Sonnet / 4o / Flash)
    Fast,           // 路由 / 分类 (4o-mini / Haiku / Flash-8B)
    Local,          // 本地 (Qwen / Llama)
}

pub enum Message {
    User { content: Vec<ContentBlock> },
    Assistant { 
        content: Vec<ContentBlock>, 
        tool_calls: Vec<ToolCall>,
    },
    Tool { 
        tool_call_id: String, 
        content: Vec<ContentBlock>,
        is_error: bool,
    },
}

pub enum ContentBlock {
    Text(String),
    Image { mime: String, data: ImageData },
}
```

**ModelHint 是抽象层最重要的设计**：上层 Agent 写 `ModelTier::Fast` 而不是 `"gpt-4o-mini"`——同一份代码可以在切换 routing 配置时跑在 Anthropic、OpenAI、本地模型上而无需修改。

### 4.2 ChatEvent（流式事件）

```rust
pub enum ChatEvent {
    /// 元数据：实际选用的 model、命中的缓存等
    Started { actual_model: String, cache_hit: CacheHitInfo },
    
    /// 文本增量
    Delta { text: String },
    
    /// 思考过程（Anthropic thinking / OpenAI o1 reasoning）
    ThinkingDelta { text: String },
    
    /// Tool call 三段式 + index 用于并行调用聚合
    ToolCallStart { index: usize, id: String, name: String },
    ToolCallArgsDelta { index: usize, args_delta: String },
    ToolCallEnd { index: usize, id: String, parsed_args: serde_json::Value },
    
    /// 部分 provider 支持中途汇报
    UsageProgress { partial: PartialUsage },
    
    /// 终结事件
    Finished { 
        stop_reason: StopReason, 
        usage: Usage,
    },
}

pub enum StopReason {
    EndTurn,
    MaxTokens,                    // 撞到 max_output_tokens,非错误,上层可触发 continue
    StopSequence(String),
    ToolUse,
    ContentFilter,
    Cancelled,                    // 上层主动中断 (CLI 后端在收到 Drop 信号时会发出)
    Other(String),
}
```

**Tool call 设计要点见 §8.1**——不仅是三段式,还必须按 `index` 聚合以处理并行调用。

---

## 5. Capability 描述符

```rust
pub struct Capabilities {
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    
    pub supports_tool_use: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_structured_output: StructuredOutputMode,
    pub supports_vision: bool,
    pub supports_thinking: bool,
    pub supports_cancel: bool,                 // CLI 后端通过中断信号实现
    
    pub prompt_cache: PromptCacheKind,
    pub streaming: bool,
    
    pub modalities_in: HashSet<Modality>,
    pub modalities_out: HashSet<Modality>,
    
    pub pricing: Pricing,
}

pub enum StructuredOutputMode {
    None,
    JsonObjectMode,               // OpenAI 老 json_object（不带 schema 校验）
    StrictSchema,                 // OpenAI strict / Gemini responseSchema
    ToolUseEmulation,             // Anthropic 通过 tool 实现
}

pub enum PromptCacheKind {
    None,
    ImplicitPrefix { min_tokens: u32 },   // OpenAI
    ExplicitMarker,                       // Anthropic cache_control
    ExplicitObject,                       // Gemini cachedContent (单独 API)
    Delegated,                            // CLI 后端
}
```

上层 Routing 和 Middleware 据此做决策：
- 需要 strict JSON 输出 → 过滤掉 `StructuredOutputMode::None` 的 provider
- 长 system prompt 复用 → 优先 `ExplicitMarker` 或 `ExplicitObject`
- 视觉任务 → 过滤 `modalities_in.contains(Image)`

---

## 6. 后端实现分类

### 6.1 HTTP API 后端

共享一个底座 `HttpProviderBase`：
- 单例 `reqwest::Client`（连接池跨 provider 共享,wrap 在 Arc 里）
- 通用重试（指数退避 + jitter）
- 通用超时（区分 connect / read / total）
- SSE 解析器
- OpenTelemetry 注入

每个 provider 只实现：

```rust
trait HttpAdapter: Send + Sync + 'static {
    fn build_url(&self, model: &str) -> Url;
    fn build_headers(&self, auth: &ResolvedAuth) -> HeaderMap;
    fn translate_request(&self, req: &ChatRequest) -> serde_json::Value;
    fn parse_event(&self, raw: &SseFrame, buf: &mut ToolCallBuffer) -> Result<Option<ChatEvent>>;
    fn classify_error(&self, status: StatusCode, body: &[u8]) -> ProviderError;
}
```

OpenAIProvider 通过 `base_url` 同时服务于 vLLM / llama.cpp server / LM Studio / Groq / Together / DeepSeek。检测到对方不支持某些字段时（比如 vLLM 早期版本不支持 strict json schema）,通过 capability flags 告知上层,但不在 provider 层悄悄降级。

### 6.2 CLI 子进程后端

**Claude Code CLI 是当前 CLI 后端的金标准**——它有真正的双向 JSONL 协议：

```bash
claude --print \
       --output-format stream-json \
       --input-format stream-json \
       --include-partial-messages
```

stdin 喂 JSONL 用户消息,stdout 吐 JSONL 事件流。架构：

```rust
pub struct ClaudeCliProvider {
    config: Arc<ClaudeCliConfig>,
    sessions: Arc<DashMap<SessionId, Arc<ClaudeSession>>>,
}

struct ClaudeSession {
    child: Mutex<Child>,                            // tokio::process::Child
    stdin_tx: mpsc::Sender<InboundMessage>,
    event_dispatcher: broadcast::Sender<ChatEvent>,
    last_used: AtomicInstant,
    /// 当前正在被消费的 request,用于 cancel 时定位
    active_request: Mutex<Option<RequestId>>,
    janitor: JoinHandle<()>,
}
```

**关键决策**：
- **长生命周期子进程,不是 one-shot**——spawn 一次 ~200-500ms,对交互式响应延迟不可接受
- **每个 SessionId 一个进程**——会话内消息共享 conversation 历史,跨会话隔离
- **空闲 5 分钟（默认）后 janitor 主动 kill**——避免进程泄漏
- **stdout 由专用 reader task 持续解析 JSONL**,通过 broadcast channel 分发
- **认证完全委托**：不在配置里出现 API key,CLI 用用户 `claude login` 后的凭证。多租户场景下需要为每个租户维护独立的 `HOME` / `XDG_CONFIG_HOME` 沙盒目录。

#### 6.2.1 Cancel 安全（CLI 后端的核心难点）

HTTP 后端的 cancel 是隐式的：上层 Drop stream → reqwest::Response 析构 → TCP 连接关闭 → 服务端推断终止。干净。

**CLI 后端则危险**：上层 Drop stream 不会让正在生成 token 的 `claude` 子进程停下来。如果不处理,后果是：

1. 子进程继续吐 stdout → 阻塞在 pipe 写入上 → 下一次复用 session 时读到上一轮的残余,造成响应错乱
2. 强杀子进程则丢失常驻收益,每次 cancel 付 200-500ms 冷启动代价

**正确实现**：

```rust
impl ClaudeCliProvider {
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let this = self.clone();
        let session = this.acquire_session(&ctx.session_id).await?;
        let request_id = RequestId::new();
        
        // 注册 active request,cancel guard 在 stream Drop 时触发清理
        *session.active_request.lock().await = Some(request_id);
        let cancel_guard = CancelGuard::new(session.clone(), request_id);
        
        let mut event_rx = session.event_dispatcher.subscribe();
        session.stdin_tx.send(InboundMessage::user_turn(req)).await?;
        
        let stream = async_stream::try_stream! {
            // cancel_guard move 进闭包,stream Drop 时自动析构
            let _guard = cancel_guard;
            
            loop {
                match event_rx.recv().await {
                    Ok(event) if event.belongs_to(request_id) => {
                        let is_terminal = matches!(event, ChatEvent::Finished { .. });
                        yield event;
                        if is_terminal { break; }
                    }
                    Ok(_) => continue,                  // 不属于本次请求,丢弃
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        Err(ProviderError::Internal("event lag".into()))?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}

/// stream Drop 时触发：向子进程发中断指令,清理 active_request 标记
struct CancelGuard {
    session: Arc<ClaudeSession>,
    request_id: RequestId,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let session = self.session.clone();
        let request_id = self.request_id;
        
        tokio::spawn(async move {
            let mut active = session.active_request.lock().await;
            if *active != Some(request_id) {
                return;  // 已经自然结束了,无需操作
            }
            
            // 1. 发 JSONL 控制指令告诉 claude 中断当前 turn
            //    (claude --output-format stream-json 支持 {"type":"interrupt"} 指令)
            let _ = session.stdin_tx.try_send(InboundMessage::Interrupt);
            
            // 2. 等待子进程吐出 Cancelled 事件 (有 timeout)
            //    reader task 会把残余 stdout 全部读完,直到收到 Finished 事件
            //    在此之前 active_request 标记保留,后续请求会等待
            tokio::time::timeout(
                Duration::from_secs(3),
                session.wait_for_cancel_ack(request_id),
            ).await.ok();
            
            *active = None;
        });
    }
}
```

**额外保险**：
- `tokio::process::Command::new("claude").kill_on_drop(true)`——provider 实例被 drop 时,所有子进程随之死亡,杜绝进程泄漏
- 子进程如果在 3 秒内未响应 interrupt 指令,janitor 升级为 `child.kill()`,然后 session 标记为 `Poisoned`,下次 acquire 时重建
- `acquire_session` 在 `active_request.is_some()` 时阻塞等待或排队,不允许并发使用同一 session

### 6.3 嵌入式后端

**MistralRsProvider**：包装 `mistral.rs`,进程内推理。trait 适配层把 ChatRequest 翻译为 mistral.rs 的 `Request`,通过 mpsc channel 拿增量结果,再翻译回 `ChatEvent`。模型加载在 provider 构造时完成,不在请求路径上。

**OnnxClassifierProvider**：**不实现 LlmProvider trait**,单独定义 `ClassifierProvider` trait。强行让分类器 fit 进 chat 抽象会污染设计。

```rust
#[async_trait]
pub trait ClassifierProvider: Send + Sync {
    async fn classify(&self, text: &str) -> Result<ClassificationResult>;
}
```

---

## 7. 认证策略

```rust
pub enum Auth {
    Inline(SecretString),
    Env { var_name: String },
    SecretManager { backend: SecretBackend, key: String },
    GoogleAdc { scope: Vec<String> },
    Delegate { 
        per_tenant_home: bool,        // 多租户隔离：每个租户独立 HOME
    },
    None,
}
```

`AuthResolver` 是统一入口：

```rust
#[async_trait]
pub trait AuthResolver: Send + Sync {
    async fn resolve(&self, auth: &Auth, ctx: &RequestContext) 
        -> Result<ResolvedAuth, AuthError>;
}
```

实现层负责 secret 缓存、刷新（OAuth token 过期）、多租户隔离。**Provider 实现绝不直接读 env vars**,全部走 AuthResolver 注入。

---

## 8. Tool Calling 格式归一化

| Provider | 字段 | 参数格式 | 并行支持 |
|---|---|---|---|
| OpenAI | `tool_calls[].function.arguments` | **JSON 字符串**（需二次解析） | ✅ 通过 `index` |
| Anthropic | `content[].input` | JSON 对象 | ✅ 通过 `index` |
| Gemini | `functionCall.args` | JSON 对象 | ✅ |

统一为：

```rust
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,   // 永远是已解析对象,从不是字符串
}
```

### 8.1 并行 Tool Call 的流式聚合

OpenAI / Anthropic 的流式 args 增量**允许跨 index 交错**——实测大多数场景串行,但规范允许这样的事件序列：

```
ToolCallStart   { index: 0, name: "search" }
ToolCallStart   { index: 1, name: "fetch" }
ToolCallArgsDelta { index: 0, args_delta: "{\"q" }
ToolCallArgsDelta { index: 1, args_delta: "{\"u" }
ToolCallArgsDelta { index: 0, args_delta: "uery\":\"x\"}" }
ToolCallArgsDelta { index: 1, args_delta: "rl\":\"y\"}" }
```

Provider 内部的解析缓冲：

```rust
pub struct ToolCallBuffer {
    inflight: HashMap<usize, ToolCallAccumulator>,
}

struct ToolCallAccumulator {
    id: String,
    name: String,
    args_chunks: String,
}

impl ToolCallBuffer {
    pub fn on_start(&mut self, index: usize, id: String, name: String) {
        self.inflight.insert(index, ToolCallAccumulator { 
            id, name, args_chunks: String::new() 
        });
    }
    
    pub fn on_delta(&mut self, index: usize, delta: &str) {
        if let Some(acc) = self.inflight.get_mut(&index) {
            acc.args_chunks.push_str(delta);
        }
    }
    
    /// 流结束或显式 ToolCallEnd 时调用,执行解析 + 兜底修复
    pub fn finalize(&mut self, index: usize) -> Result<(String, String, serde_json::Value)> {
        let acc = self.inflight.remove(&index)
            .ok_or(ProviderError::Parse(format!("no tool call at index {}", index)))?;
        
        // 三段式兜底
        let parsed = serde_json::from_str::<serde_json::Value>(&acc.args_chunks)
            .or_else(|_| {
                // partial-json 修复（截断的 JSON 补齐括号 / 引号）
                partial_json::fix_and_parse(&acc.args_chunks)
            })
            .map_err(|e| ProviderError::Parse(format!(
                "tool call args parse failed for {}: {} | raw: {}", 
                acc.name, e, acc.args_chunks
            )))?;
        
        Ok((acc.id, acc.name, parsed))
    }
}
```

**关键**：解析只在 `finalize` 时执行,绝不在 delta 阶段尝试增量 parse（增量阶段的 args 一定是不完整的 JSON,会浪费 CPU 也会误报错误）。

---

## 9. Structured Output 归一化

```rust
pub struct JsonSchema {
    pub schema: serde_json::Value,
    pub strict: bool,
}
```

各 provider 翻译方式：
- **OpenAI**: `response_format = { type: "json_schema", json_schema: { strict: true, schema: ... } }`
- **Gemini**: `responseSchema = ...` + `responseMimeType = "application/json"`
- **Anthropic**: 注入隐藏 tool `__respond_with__`,`tool_choice = { type: "tool", name: "__respond_with__" }`,把 schema 作为 input_schema。响应时把 tool_use 的 input 当作最终输出。
- **vLLM**: 支持 `guided_json` / `guided_regex`,能力同 OpenAI strict
- **llama.cpp**: 支持 `json_schema`,也是 guided generation
- **本地无 guided 支持的后端**: capability 标 `StructuredOutputMode::None`,上层走 retry-with-error-feedback fallback

---

## 10. Prompt Cache 抽象

```rust
pub enum CacheDirective {
    /// 自动缓存（OpenAI 隐式）
    Auto,
    
    /// 在当前位置标记缓存边界（Anthropic）,该位置之前的内容会被缓存
    MarkBoundary { ttl: Duration },
    
    /// 引用已经创建好的缓存对象（Gemini cachedContent）
    UseExplicit { handle: ProviderCacheHandle },
}

pub struct ProviderCacheHandle {
    pub provider: ProviderId,
    pub external_id: String,         // 服务器侧 cache 句柄,绝不暴露给用户
    pub tenant_namespace: String,    // 强制租户隔离
    pub created_at: SystemTime,
    pub expires_at: SystemTime,
}
```

Provider 暴露独立 sub-trait 管理 explicit cache：

```rust
#[async_trait]
pub trait ExplicitCacheProvider: LlmProvider {
    async fn create_cache(&self, content: CacheableContent, ttl: Duration) 
        -> Result<ProviderCacheHandle>;
    async fn delete_cache(&self, handle: &ProviderCacheHandle) -> Result<()>;
    async fn extend_ttl(&self, handle: &ProviderCacheHandle, additional: Duration) 
        -> Result<()>;
}
```

不是所有 provider 都实现这个 sub-trait——OpenAI 只走 `CacheDirective::Auto`。

### 10.1 主动回收（不能依赖 TTL）

云厂商的 cachedContent 都按存储 + 输入 token 计费,TTL 只是兜底,**真正控制成本必须主动 delete**。

谁触发 delete：**Cache Lookup Middleware 维护一个全局 LRU + Janitor task**,Provider 层只暴露 `delete_cache` 能力,不主动调度。

触发场景：
1. **Session 闲置**：上层 Session 超时（默认 15 分钟无活动）→ 找出该 session 引用的所有 ProviderCacheHandle → 批量 delete
2. **LRU 淘汰**：全局 cache 数量或聚合存储量超阈值 → 按 last_used_at 淘汰最旧的
3. **租户切换**：用户/租户主动结束工作流 → 立即 delete
4. **预算告警**：当日 cache storage 成本超阈值 → 强制清理

伪代码（在 Middleware 层,非 Provider 层）：

```rust
pub struct CacheJanitor {
    registry: Arc<CacheRegistry>,                  // session_id -> Vec<ProviderCacheHandle>
    providers: Arc<ProviderRegistry>,
    config: JanitorConfig,
}

impl CacheJanitor {
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            self.evict_idle_sessions().await;
            self.evict_lru_if_over_quota().await;
        }
    }
    
    async fn evict_idle_sessions(&self) {
        let cutoff = Instant::now() - self.config.session_idle_timeout;
        let stale = self.registry.find_idle_since(cutoff);
        
        for (session_id, handles) in stale {
            for handle in handles {
                if let Some(provider) = self.providers.get_explicit_cache(&handle.provider) {
                    if let Err(e) = provider.delete_cache(&handle).await {
                        tracing::warn!(?e, ?handle, "cache delete failed; will retry");
                        continue;
                    }
                    self.registry.remove(&session_id, &handle);
                }
            }
        }
    }
}
```

**为什么放在 Middleware 而非 Provider**：Provider 层是无状态适配器,不应该持有"哪些 cache 属于哪个 session"这种业务知识。Provider 只暴露能力,Middleware 负责调度。这跟前面"Provider 不感知 IAM / Cache / Budget"的边界划分是一致的。

---

## 11. 错误归一化

```rust
pub enum ProviderError {
    Auth(String),
    RateLimited { retry_after: Option<Duration> },
    BudgetExceeded,
    InvalidRequest(String),
    ContentFiltered { category: String },
    
    /// 输入超过模型 context window —— 永久错误,需上层截断
    ContextTooLong { limit: u32, requested: u32 },
    
    ModelOverloaded,
    Network(Box<dyn std::error::Error + Send + Sync>),
    Parse(String),
    CliSubprocessDied { exit_code: Option<i32>, stderr: String },
    Internal(String),
}
```

### 11.1 ContextTooLong vs MaxTokens 的区分

两种"超出限制"在语义上完全不同,绝不能混淆：

| 情况 | 表现 | 分类 | 上层处理 |
|---|---|---|---|
| 输入太长 | 请求 prompt > `max_context_tokens` | `ProviderError::ContextTooLong` | 永久错误,必须摘要 / 滑窗 / 拆分 |
| 输出截断 | 推断中达到 `max_output_tokens` | `StopReason::MaxTokens`（**不是错误**） | 可继续 turn,追加 Assistant 消息触发 continue |

**HTTP Adapter 的责任**：很多 provider 在 prompt 过长时会返回 400 + 模糊的 "context_length_exceeded" 错误,适配器必须识别这种情况并映射到 `ContextTooLong { limit, requested }`,而不是抛 `InvalidRequest`。同时,响应里 `finish_reason == "length"` 必须映射到 `StopReason::MaxTokens`,而不是任何错误。

### 11.2 错误分类

```rust
impl ProviderError {
    pub fn class(&self) -> ErrorClass {
        use ProviderError::*;
        match self {
            RateLimited { .. } | ModelOverloaded | Network(_) => ErrorClass::Retriable,
            
            Auth(_) | InvalidRequest(_) | ContextTooLong { .. } 
                | ContentFiltered { .. } | BudgetExceeded => ErrorClass::Permanent,
            
            Parse(_) | Internal(_) | CliSubprocessDied { .. } => ErrorClass::MaybeRetriable,
        }
    }
    
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::ModelOverloaded => Some(Duration::from_secs(10)),
            _ => None,
        }
    }
}
```

上层重试 Middleware 只看 `class()` 决策,不需要 match 具体错误类型。

---

## 12. Routing 层

```rust
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn LlmProvider>>,
    routing: Arc<dyn RoutingPolicy>,
    metrics: Arc<ProviderMetrics>,
}

#[async_trait]
pub trait RoutingPolicy: Send + Sync {
    async fn select(
        &self,
        req: &ChatRequest,
        candidates: &[(ProviderId, Arc<dyn LlmProvider>)],
        metrics: &ProviderMetrics,
    ) -> Result<Vec<ProviderId>, RoutingError>;
}
```

内置策略：
- `ExplicitPolicy`：req.model 是 `ModelHint::Explicit(name)` 时直接定位
- `TierPolicy`：根据 `ModelTier` 查表
- `CostPolicy`：在能力满足前提下选最便宜
- `LatencyPolicy`：根据 metrics 里的 P50 延迟选最快
- `EnsemblePolicy`：并行调多个 provider,按策略合并（majority vote / fastest / longest）
- `FallbackChain<P>`：包装其他 policy,失败自动降级

配置示例：

```toml
[routing]
default = "tier"

[routing.tiers]
reasoning = ["claude_opus_api", "openai_o1", "gemini_pro"]
default   = ["claude_sonnet_api", "openai_4o", "gemini_flash"]
fast      = ["openai_4o_mini", "claude_haiku", "local_qwen_7b"]
local     = ["local_qwen_32b"]

[routing.fallback]
on_rate_limit = "next_in_tier"
on_overload   = "next_in_tier"
on_auth       = "fail"
```

---

## 13. 配置形态

```toml
[providers.claude_api]
type = "anthropic"
base_url = "https://api.anthropic.com"
auth = { kind = "secret_manager", backend = "vault", key = "anthropic/prod" }
default_model = "claude-opus-4-7"
timeout = { connect_ms = 5000, read_ms = 120000 }

[providers.openai_api]
type = "openai"
auth = { kind = "env", var_name = "OPENAI_API_KEY" }
default_model = "gpt-4o"

[providers.local_qwen]
type = "openai_compat"
base_url = "http://ryzen-node-1:8000/v1"
auth = { kind = "none" }
default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
capabilities_override = { supports_thinking = false, prompt_cache = "none" }

[providers.claude_cli]
type = "claude_code_cli"
binary = "claude"
mode = "long_lived"
session_idle_timeout_secs = 300
auth = { kind = "delegate", per_tenant_home = true }

[providers.gemini_cli]
type = "gemini_cli"
binary = "gemini"
mode = "one_shot"
auth = { kind = "delegate", per_tenant_home = true }

[providers.embedded_qwen]
type = "mistral_rs"
model_path = "/models/qwen2.5-7b-q4.gguf"
gpu_layers = 999
context_size = 32768

[providers.guard_classifier]
type = "onnx_classifier"
model_path = "/models/deberta-injection-int8.onnx"
threshold = 0.85
```

---

## 14. 测试策略

三层金字塔：

1. **单元测试**：用 `MockProvider`（基于 fixtures 回放预录响应）,跑业务逻辑断言。Fixtures 用 cassette 模式记录真实 provider 响应,存 git。
2. **Capability 一致性测试**：每个 provider 实现都跑同一套 conformance 测试集（tool use / structured output / streaming / cancel / 错误分类）,验证抽象层不漏。这一层是 nightly CI,调真实 API（成本可控,每次几分钱）。
3. **生产 smoke**：每个 release 前对每个 provider 跑一次极小请求,确认凭证和网络通。

**conformance 测试必须覆盖**：
- 流中途 cancel（包括 CLI 后端的 session 复用验证）
- 并行 tool call 的 index 交错场景（用 mock 注入,真实模型不一定触发）
- ContextTooLong 与 MaxTokens 的正确分类
- 显式 cache 的 create / use / delete 闭环

---

## 15. 反模式清单

1. **不要在请求路径上做精确 token 计数**。tokenizer 加载几百 MB,单次调用 5-50ms。预算检查用 `chars / 4` 估算,真值从 response.usage 读。
2. **不要把流式和非流式实现成两条代码路径**。collect 一遍 stream 是几行代码,避免 bug 翻倍。
3. **不要在 trait 里暴露 provider-specific 字段**。`thinking_budget`、`prompt_cache_ttl`、`safety_settings` 都是 provider-specific,污染 trait 后所有 provider 都得假装支持。
4. **不要让 CLI provider 每次都 spawn 进程**。Claude CLI cold start 200-500ms,对交互延迟是灾难。
5. **不要假设 CLI binary 在固定路径**。走 `which` 查找 + 配置覆盖。
6. **不要把 OpenAI 的 args 字符串原样抛给上层**。Provider 层解析、修复、报错三段式处理。
7. **不要在 provider 层重试 4xx 错误**。401 / 400 / 422 是配置或代码问题,重试只会浪费 quota。
8. **CLI 后端的 stderr 不要丢**。CLI 错误信息基本只在 stderr,要捕获并塞进 `ProviderError::CliSubprocessDied.stderr`。
9. **不要在 `stream()` 返回时就消费完所有事件**。返回 `BoxStream` 让调用方决定是否聚合,否则 streaming 优势消失。
10. **多租户场景下 CLI 后端必须隔离 HOME**。否则租户 A 的 `claude login` 会被租户 B 的 session 覆盖,权限边界穿透。
11. **不要把 `ContextTooLong` 错误归类为 `InvalidRequest`**。前者上层有明确处理路径（截断 / 摘要）,后者上层只能放弃。
12. **不要在增量 args 阶段尝试 JSON 解析**。增量必然不完整,只在 ToolCallEnd 或流终止时 finalize。
13. **不要假设 stream Drop = 资源自动释放**（仅 HTTP 后端成立）。CLI 后端必须显式 cancel。
14. **不要让 Provider 持有"cache 属于哪个 session"的业务状态**。Provider 只暴露 delete_cache 能力,调度归 Middleware。
15. **不要依赖云厂商的 cache TTL 自动过期**。主动 delete 才是真正的成本控制。

---

## 16. 与上层（Middleware / Agent）的边界

```
┌─────────────────────────────────────────┐
│  Agent Runtime (Orchestrator + Workers) │
└──────────────────┬──────────────────────┘
                   │ ChatRequest (with ModelHint)
                   ▼
┌─────────────────────────────────────────┐
│  Middleware Pipeline                    │
│  - IAM / Auth                           │
│  - Cache Lookup + Janitor (LRU 主动回收) │
│  - Budget Control                       │
│  - Prompt Guard                         │
│  - Telemetry                            │
└──────────────────┬──────────────────────┘
                   │ ChatRequest (model resolved)
                   ▼
┌─────────────────────────────────────────┐
│  ProviderRegistry + RoutingPolicy       │
└──────────────────┬──────────────────────┘
                   │ Provider 选定
                   ▼
┌─────────────────────────────────────────┐
│  LlmProvider impl                       │
│  - HTTP / CLI / Embedded                │
│  - 仅暴露能力,不持业务状态              │
└─────────────────────────────────────────┘
```

Provider 层**完全不感知** Cache、IAM、Budget、Tracing 业务概念——这些是 Middleware 在调用 Provider 之前/之后处理的。Provider 只负责"我是某个具体后端,给我标准请求,我吐标准事件流,我提供 cache delete 等原子能力"。

这个边界划清楚之后,新增一个 provider（比如 xAI Grok 出 API 了）只需要写一个 `XaiAdapter`,其余所有 Cache / Budget / Routing 自动可用。

---

## 17. 待办与开放问题

- [ ] mistral.rs 嵌入式后端在 macOS Apple Silicon 上的 Metal backend 验证
- [ ] Claude Code CLI 的 `interrupt` JSONL 控制指令规范确认（可能需要适配多个 CLI 版本）
- [ ] Gemini CLI 的 stream protocol 成熟度评估,是否值得从 one-shot 升级到 long-lived
- [ ] OAuth token 自动刷新机制（Anthropic 和 Google 有不同流程）
- [ ] 嵌入式 ONNX 分类器的多线程推理调度（避免单 model 实例被多请求竞争）
