# 文档 05 — Tool / MCP / Skill 接入设计

> 范围：定义 Agent 可调用的外部能力——本地工具、MCP server、复合 Skill——的统一抽象，权限模型，调用生命周期，副作用追踪。
>
> 上游：被 Doc 04 Agent Runtime 调用（`AgentContext::tool_registry`）。
>
> 下游：可能调用 Doc 01 LlmProvider（Skill 内部如果含 LLM 步骤）、外部进程（MCP / CLI）、HTTP API。

---

## 1. 三个概念的关系

很多框架把 Tool / MCP / Skill 混为一谈，导致权限模型 / 生命周期 / 调用语义全部纠缠。本文档严格区分三层抽象：

```
                    ┌──────────────────────┐
                    │   Skill              │   高级能力,可能含多步 LLM 推理
                    │ (Composed Capability)│   例:"安全审查整个 PR"
                    └──────────┬───────────┘
                               │ 内部可能编排
                               ▼
                    ┌──────────────────────┐
                    │   Tool               │   单一原子调用,有明确输入输出 schema
                    │ (Atomic Function)    │   例:"读取文件" / "执行 SQL"
                    └──────────┬───────────┘
                               │ 实现可来自
                ┌──────────────┼──────────────┐
                ▼              ▼              ▼
         ┌──────────┐   ┌────────────┐  ┌──────────┐
         │ Built-in │   │ MCP Server │  │ HTTP API │
         │ (Rust fn)│   │ (子进程)   │  │ (REST)   │
         └──────────┘   └────────────┘  └──────────┘
```

| 概念 | 抽象层级 | 调用者 | 可见性 | 示例 |
|---|---|---|---|---|
| **Tool** | 原子函数 | Agent (Worker / Orchestrator) | LLM 在 tool_use 中按名引用 | `read_file(path)`, `query_postgres(sql)` |
| **MCP Server** | Tool 的来源/容器 | ToolRegistry 内部 | Tool 通过 MCP 协议被发现和调用 | `mcp-filesystem`, `mcp-github` |
| **Skill** | 复合能力 | Orchestrator 在 PlanIssued 中引用 | LLM 不直接看到 Skill 内部，看到的是更高层的 capability | "审查这个 PR"（内部含 read_file + AST 解析 + LLM 推理 + 写评论） |

**核心区分**：
- Tool 是**原子的**——一次调用，一个结果，无 LLM 介入（除非 Tool 实现刚好包了 LLM）
- Skill 是**复合的**——可以编排多个 Tool 调用 + LLM 推理 + 中间状态
- MCP 是**协议**而非概念——它定义了"远程 Tool 怎么发现和调用"，是 Tool 的运输方式之一

---

## 2. 设计目标

| 目标 | 说明 |
|---|---|
| **三层清晰** | Tool / MCP / Skill 各有独立的 trait、生命周期、权限模型，不互相污染 |
| **Tool schema 严格** | 输入输出走 JSON Schema strict mode，与 Doc 01 §9 Provider 端 structured output 一致 |
| **Idempotency 必填** | 所有 Tool 调用接受 idempotency_key，满足 Doc 04 §7 replay 契约 |
| **副作用分类强制** | 每个 Tool 声明 SideEffectKind（Doc 04 §4.4），Runtime 据此决定能否在中间步骤执行 |
| **IAM gating** | 每个 Tool 有最小权限要求，Principal 不满足时调用前拒绝 |
| **MCP 子进程复用** | 长生命周期 MCP server，跨请求共享，参考 Doc 01 §6.2 CLI 模式 |
| **Skill 可移植** | Skill 定义与具体 Tool 实现解耦，相同 Skill 在不同 Tool 实现下表现一致 |
| **流式输出** | 长时间 Tool（grep 大代码库 / 跑测试套件）支持 stream，不是一锤子返回 |

**反目标**：
- 不让 LLM 自由选择 Tool / Skill——可调用集合由 Orchestrator 在代码里圈定
- 不在 Tool 层做 prompt 拼装——Skill 内部如果调 LLM，走 Doc 02 Pipeline
- 不允许 MCP server 直接接收外网流量——所有 MCP 调用经过本进程的 ToolRegistry
- 不暴露 Tool 的内部 idempotency 状态给 LLM——LLM 看到的是干净的"调用 + 结果"

---

## 3. Tool 核心抽象

### 3.1 Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn id(&self) -> &ToolId;
    fn descriptor(&self) -> &ToolDescriptor;
    
    /// 原子调用
    async fn invoke(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError>;
    
    /// 流式调用 (默认实现：一次性返回包成单元素流)
    async fn invoke_stream(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<BoxStream<'static, Result<ToolEvent, ToolError>>, ToolError> {
        let result = self.invoke(args, ctx).await?;
        Ok(Box::pin(futures::stream::once(async move { 
            Ok(ToolEvent::Complete(result)) 
        })))
    }
}

pub struct ToolDescriptor {
    pub id: ToolId,
    pub display_name: String,
    pub description: String,                 // 这就是给 LLM 看的 tool description
    pub input_schema: JsonSchema,
    pub output_schema: JsonSchema,
    pub side_effect: SideEffectKind,         // Doc 04 §4.4
    pub required_scopes: Vec<Scope>,         // IAM 最小权限
    pub idempotent: bool,                    // 是否天然幂等 (read 类一般是)
    pub typical_latency: Duration,           // 用于 budget 估算和超时设置
    pub timeout: Duration,                   // 硬上限
    pub source: ToolSource,                  // Built-in / Mcp / Http / Subprocess
    pub version: SemanticVersion,
}

pub struct ToolContext {
    pub trajectory: TrajectoryId,
    pub step_seq: u32,
    pub idempotency_key: String,             // Doc 04 §7 必填
    pub principal: Principal,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub budget: BudgetView,                  // Tool 调用也算 budget (时长 / 调用次数)
}

pub enum ToolEvent {
    Started { tool: ToolId, args_summary: String },
    Progress { message: String, percent: Option<f32> },
    PartialOutput { chunk: serde_json::Value },
    Complete(ToolOutput),
}

pub struct ToolOutput {
    pub result: serde_json::Value,           // 必须符合 descriptor.output_schema
    pub side_effects: Vec<SideEffect>,       // 实际产生的副作用清单
    pub usage: ToolUsage,                    // 资源消耗 (CPU 时间 / 网络 / token)
}
```

### 3.2 ToolError 分类

```rust
pub enum ToolError {
    InvalidArguments { schema_violation: String },
    PermissionDenied { required: Scope, principal: Principal },
    Timeout { elapsed: Duration, limit: Duration },
    Cancelled,
    
    /// 资源不存在 / 状态错误等业务级错误,LLM 可以学习
    BusinessError { code: String, message: String, retriable: bool },
    
    /// 工具实现内部错误 (MCP server crash / network 抖动)
    Infrastructure { source: Box<dyn StdError + Send + Sync> },
    
    /// 副作用执行成功但响应丢失 (网络断在 ack 之前)
    /// idempotency_key 让 retry 安全
    AmbiguousOutcome { idempotency_key: String, message: String },
}

impl ToolError {
    pub fn class(&self) -> ToolErrorClass {
        use ToolError::*;
        match self {
            InvalidArguments { .. } | PermissionDenied { .. } => ToolErrorClass::Permanent,
            BusinessError { retriable: false, .. } => ToolErrorClass::Permanent,
            Timeout { .. } | Cancelled | Infrastructure { .. } 
                | AmbiguousOutcome { .. } => ToolErrorClass::Retriable,
            BusinessError { retriable: true, .. } => ToolErrorClass::Retriable,
        }
    }
}
```

**重要**：BusinessError 的 retriable 字段必须由 Tool 实现明确标注。Agent / LLM 不该自己猜——"文件不存在"是永久错误，"暂时连不上数据库"是可重试错误。

### 3.3 输入输出 Schema 与 LLM 协同

LLM 看到的工具签名直接来自 `ToolDescriptor.input_schema`：

```rust
// Provider 适配层 (Doc 01 §8) 翻译为对应 provider 的 tool spec
fn descriptor_to_openai_tool(desc: &ToolDescriptor) -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": desc.id.as_str(),
            "description": desc.description,
            "parameters": desc.input_schema,
            "strict": true,
        }
    })
}
```

**强制规则**：
1. input_schema 必须可以被 OpenAI strict mode / Anthropic tool use / Gemini functionCalling 同时接受。这意味着子集语法（不能用 `oneOf` / `not` / 复杂 `pattern`）。
2. output_schema 不只是文档——Runtime 在 Tool 返回时强制校验，违反时报 `BusinessError { code: "schema_violation", retriable: false }`。
3. 描述（description）必须解释**何时使用**，不只是**做什么**。LLM 选 Tool 主要看 description。

---

## 4. ToolRegistry

### 4.1 核心 trait

```rust
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// 列出某 Agent role 在某 Principal 下可见的所有 Tool
    async fn list_for(
        &self, 
        agent: &AgentId, 
        principal: &Principal,
    ) -> Vec<Arc<dyn Tool>>;
    
    /// 按 ID 查找
    async fn get(&self, tool_id: &ToolId) -> Option<Arc<dyn Tool>>;
    
    /// 调用 (经过 IAM / 超时 / audit)
    async fn invoke(
        &self,
        tool_id: &ToolId,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError>;
    
    /// 流式调用
    async fn invoke_stream(
        &self,
        tool_id: &ToolId,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<BoxStream<'static, Result<ToolEvent, ToolError>>, ToolError>;
}
```

### 4.2 Tool 调用 Pipeline

每次 Tool 调用都经过自己的 mini middleware stack（比 LLM Pipeline 轻量但同型）：

```
Tool Invocation Request
        │
        ▼
┌─────────────────┐
│ IAM Gate        │  ← required_scopes ⊆ principal.scopes ?
└────────┬────────┘
         ▼
┌─────────────────┐
│ Idempotency     │  ← 同 key 已有完成的调用 → 直接返回缓存结果
│ Cache           │  
└────────┬────────┘
         ▼
┌─────────────────┐
│ Side Effect     │  ← 中间步骤不允许 Irreversible
│ Gate            │
└────────┬────────┘
         ▼
┌─────────────────┐
│ Budget Check    │  ← Tool 调用次数 / 预估时长
└────────┬────────┘
         ▼
┌─────────────────┐
│ Audit Log       │  ← 写入事件日志 (可重放)
└────────┬────────┘
         ▼
┌─────────────────┐
│ Timeout Wrap    │  ← descriptor.timeout
└────────┬────────┘
         ▼
   Tool::invoke
```

每层失败都是显式的 `ToolError`，不静默吞错误。

### 4.3 Idempotency Cache

Tool 调用结果按 `idempotency_key` 缓存（与 Doc 03 LLM cache 是不同的 store，不要混淆）：

```rust
pub struct IdempotencyCache {
    /// idempotency_key -> ToolOutput
    /// TTL 通常 24h,与 trajectory 的最长生命周期匹配
    backend: Arc<dyn KVStore>,
}

impl IdempotencyCache {
    pub async fn get_or_compute<F, Fut>(
        &self,
        key: &str,
        compute: F,
    ) -> Result<ToolOutput, ToolError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ToolOutput, ToolError>>,
    {
        if let Some(cached) = self.backend.get(key).await? {
            return Ok(cached);
        }
        let result = compute().await?;
        self.backend.put(key, &result, Duration::from_hours(24)).await?;
        Ok(result)
    }
}
```

**关键边界**：
- 只缓存 **成功** 的输出。失败不缓存，否则 retry 永远拿到相同错误
- 副作用类 Tool 的"幂等"语义由 **Tool 实现** 配合 idempotency_key 保证（用 key 作为外部 ID 去重），不是 cache 层模拟

### 4.4 IAM Gate

每个 Tool 有 `required_scopes`，调用时校验 `principal.scopes ⊇ required_scopes`：

```rust
async fn check_iam(tool: &dyn Tool, principal: &Principal) -> Result<(), ToolError> {
    let required = &tool.descriptor().required_scopes;
    for scope in required {
        if !principal.has_scope(scope) {
            return Err(ToolError::PermissionDenied { 
                required: scope.clone(), 
                principal: principal.clone() 
            });
        }
    }
    Ok(())
}
```

**与 Doc 02 §4.2 IAM Middleware 的区别**：
- LLM Pipeline 的 IAM 校验"调用方能不能跟这个模型对话"
- Tool 的 IAM 校验"调用方能不能调这个 Tool"
- 两层是独立的——Principal 可能能调 LLM 但不能调 `delete_database`

---

## 5. MCP 集成

### 5.1 MCP 协议简述

Model Context Protocol（Anthropic 提出）定义三类资源：
- **Tools**：可调用函数（与本文 §3 的 Tool 同语义）
- **Resources**：可读数据（URI 寻址）
- **Prompts**：可复用 prompt 模板

传输方式：
- **stdio**：子进程 + JSON-RPC over stdin/stdout
- **SSE**：HTTP server-sent events
- **HTTP**：纯 REST

我们的 Runtime 主要消费 **Tools**，对 Resources 和 Prompts 提供有限支持（详见 §5.4）。

### 5.2 MCP Server 适配

MCP server 是 Tool 的来源之一，由 `McpToolProvider` 适配为 `Tool` trait：

```rust
pub struct McpToolProvider {
    server_id: McpServerId,
    transport: McpTransport,
    discovered_tools: Arc<RwLock<HashMap<String, McpToolHandle>>>,
}

pub enum McpTransport {
    Stdio { 
        binary: PathBuf, 
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Sse { url: Url, auth: Auth },
    Http { url: Url, auth: Auth },
}

impl McpToolProvider {
    /// 启动时调 MCP list_tools 把 server 提供的工具注册到 ToolRegistry
    pub async fn discover(&self) -> Result<Vec<ToolDescriptor>, McpError>;
    
    /// 把每个 MCP tool 包装成 Rust Tool 实现
    pub fn wrap_as_tool(&self, mcp_handle: McpToolHandle) -> Arc<dyn Tool> {
        Arc::new(McpToolAdapter { 
            provider: self.clone(),
            handle: mcp_handle,
        })
    }
}

struct McpToolAdapter {
    provider: McpToolProvider,
    handle: McpToolHandle,
}

#[async_trait]
impl Tool for McpToolAdapter {
    async fn invoke(&self, args: serde_json::Value, ctx: ToolContext) 
        -> Result<ToolOutput, ToolError> 
    {
        // 1. 通过 provider 的 transport 发 call_tool 请求
        let response = self.provider.call_tool(&self.handle.name, args, &ctx).await?;
        
        // 2. 翻译 MCP 响应为 ToolOutput,校验 output schema
        Ok(ToolOutput {
            result: response.content,
            side_effects: response.declared_side_effects,
            usage: ToolUsage::from_mcp(&response.usage),
        })
    }
}
```

### 5.3 Stdio MCP Server 的子进程管理

Stdio MCP server 的生命周期管理与 Doc 01 §6.2 Claude CLI 完全同型——长生命周期、按 session 复用、空闲超时杀进程、kill_on_drop：

```rust
pub struct StdioMcpProcess {
    config: McpStdioConfig,
    sessions: Arc<DashMap<SessionId, Arc<McpSession>>>,
}

struct McpSession {
    child: Mutex<tokio::process::Child>,           // .kill_on_drop(true)
    stdin_tx: mpsc::Sender<JsonRpcRequest>,
    response_dispatcher: broadcast::Sender<JsonRpcResponse>,
    last_used: AtomicInstant,
    janitor: JoinHandle<()>,
}
```

**关键决策**（与 Doc 01 §6.2 一致）：
- **每个 SessionId 一个子进程**——会话内 MCP 状态共享，跨会话隔离
- **空闲 5 分钟杀进程**——避免泄漏
- **多租户必须独立 HOME**——MCP server 的 auth state（如 OAuth token）按租户隔离
- **kill_on_drop(true)** 兜底
- **Cancel 安全**：进行中的 tool 调用 Drop 时发 JSON-RPC `cancelled` notification 给 server

### 5.4 Resources 与 Prompts 的有限支持

| MCP 概念 | Runtime 支持 | 理由 |
|---|---|---|
| Tools | ✅ 完整 | 核心用例 |
| Resources | ⚠️ 仅在 Skill 内部使用 | LLM 不直接访问 URI；Skill 实现可以读 resource 然后注入 prompt |
| Prompts | ❌ 不集成 | Prompt 拼装是 Doc 04 §11 PromptBuilder 的职责，不让 MCP server 决定 prompt |

**为什么不让 MCP 提供 prompt**：MCP server 是不可信的外部代码——让它定义 system prompt 等于把"我们 Agent 怎么思考"的控制权交出去。Prompt 必须在我们自己的代码里拼装。

### 5.5 MCP Server 的安全边界

MCP server 是**部分可信代码**——可能是社区维护的、第三方提供的、用户自己装的。对待方式：

1. **不允许 MCP server 在主进程地址空间执行**——必须是子进程或远程
2. **Stdio MCP server 必须配置允许的 binary 白名单**——不允许任意路径执行
3. **HTTP/SSE MCP server 的 URL 必须在配置中显式声明**——不允许 dynamic URL discovery
4. **MCP server 声明的 tool 仍要在我们的 ToolRegistry 中显式启用**——不能 auto-register
5. **MCP tool 的 required_scopes 由配置覆盖**——MCP server 自己声明的 scope 仅作 hint，最终权限以本地配置为准

```toml
[mcp_servers.filesystem]
type = "stdio"
binary = "/usr/local/bin/mcp-filesystem"
args = ["--root", "/srv/projects"]
auth = { kind = "delegate", per_tenant_home = true }

[mcp_servers.filesystem.tools.read_file]
enabled = true
required_scopes = ["fs:read"]                # 覆盖 server 自己的声明
override_timeout_secs = 30

[mcp_servers.filesystem.tools.delete_file]
enabled = false                               # 完全禁用,即使 server 提供
```

---

## 6. Skill 抽象

### 6.1 Skill trait

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn id(&self) -> &SkillId;
    fn descriptor(&self) -> &SkillDescriptor;
    
    /// 执行 Skill —— 内部可能多步 (tool call + LLM + 状态机)
    /// 返回的是事件流,与 Tool::invoke_stream 类似但语义更复杂
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: SkillContext,
    ) -> Result<BoxStream<'static, Result<SkillEvent, SkillError>>, SkillError>;
}

pub struct SkillDescriptor {
    pub id: SkillId,
    pub display_name: String,
    pub description: String,
    pub input_schema: JsonSchema,
    pub output_schema: JsonSchema,
    pub required_scopes: Vec<Scope>,
    pub required_tools: Vec<ToolId>,         // 需要哪些 Tool 才能跑
    pub estimated_cost: CostEstimate,
    pub max_duration: Duration,
    pub source: SkillSource,
}

pub enum SkillSource {
    /// 纯 Rust 实现,确定性逻辑
    Native,
    /// 由 prompt 模板 + tool 编排定义,LLM 驱动
    Declarative { spec_file: PathBuf },
    /// 完全是子 Agent
    SubAgent { agent_id: AgentId },
}

pub struct SkillContext {
    pub trajectory: TrajectoryId,            // 父 trajectory
    pub principal: Principal,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub budget: BudgetView,
    
    /// Skill 可以调用 Tool 和 LLM
    pub tool_registry: Arc<dyn ToolRegistry>,
    pub llm_service: Arc<dyn LlmService>,
}

pub enum SkillEvent {
    Started { skill: SkillId },
    SubStepStarted { step_name: String },
    ToolCalled { tool: ToolId, args_summary: String },
    LlmInvoked { model_tier: ModelTier },
    PartialResult { artifact: ArtifactPreview },
    SubStepCompleted { step_name: String },
    Completed { final_output: serde_json::Value, usage: SkillUsage },
}
```

### 6.2 Skill 与 Agent / Tool 的关系

Skill 在抽象上夹在 Agent 和 Tool 之间：

| 维度 | Tool | Skill | Agent |
|---|---|---|---|
| 调用方 | Agent (Worker / Orchestrator) | Agent (通常是 Orchestrator) | Runtime |
| 内部状态 | 无 | 可能有（多步） | 有（trajectory state） |
| 含 LLM | 通常无（除非 Tool 包装的就是 LLM 服务） | 可能有 | 几乎一定有 |
| 副作用追踪 | 单点（descriptor 声明） | 累积（每个内部 Tool 调用累加） | 累积（事件日志） |
| 失败语义 | 单一错误 | 部分完成 + 错误 | 整条 trajectory abandon + 补偿 |

### 6.3 Skill 的两种实现模式

**Native（推荐用于关键路径）**：
```rust
pub struct CodeReviewSkill { /* ... */ }

#[async_trait]
impl Skill for CodeReviewSkill {
    async fn execute(&self, args: serde_json::Value, ctx: SkillContext) 
        -> Result<BoxStream<...>, SkillError> 
    {
        // 1. 解析 args
        let CodeReviewArgs { repo, pr_number } = serde_json::from_value(args)?;
        
        // 2. 调 Tool 拉 diff
        let diff = ctx.tool_registry.invoke(
            &"git.fetch_pr_diff".into(),
            json!({ "repo": repo, "pr": pr_number }),
            tool_ctx_from(&ctx, 1),
        ).await?;
        
        // 3. 解析 AST (纯计算,不算 Tool 调用)
        let ast = parse_diff_to_ast(&diff)?;
        
        // 4. 调 LLM 审查
        let review = ctx.llm_service.clone().call(
            ChatRequest { /* ... */ },
            req_ctx_from(&ctx),
        ).await?;
        
        // 5. 写评论 (副作用 - 必须在 commit phase)
        // ... 
    }
}
```

Native skill 的全部逻辑在 Rust 代码里——可读、可测、可重构。适合稳定的、关键路径的能力。

**Declarative（适合实验性 / 用户可定制）**：
```yaml
# skills/security_audit.yaml
id: security_audit
description: "全面安全审查"
required_tools: [git.fetch_pr_diff, sast.run_semgrep]
steps:
  - name: fetch_diff
    tool: git.fetch_pr_diff
    args:
      repo: "${input.repo}"
      pr: "${input.pr}"
  - name: run_sast
    tool: sast.run_semgrep
    args:
      diff: "${steps.fetch_diff.output}"
  - name: synthesize
    llm:
      tier: reasoning
      prompt_template: security_synthesis.j2
      input:
        diff: "${steps.fetch_diff.output}"
        sast_findings: "${steps.run_sast.output}"
output: "${steps.synthesize.output}"
```

Declarative skill 由通用 SkillExecutor 解释执行。**风险**：YAML 表达力有限，复杂逻辑（条件分支 / 循环 / 错误处理）会让 spec 变得难以维护。建议只用于线性流水线。

复杂的、有分支的、需要 critic loop 的——直接做成 SubAgent（见 §6.4）。

### 6.4 Skill as SubAgent

最高复杂度的 Skill 直接是一个 SubAgent——它有自己的 trajectory、自己的事件日志、自己的 critic loop，对外仍然是一次 Skill 调用：

```rust
pub struct SubAgentSkill {
    inner_runtime: Arc<dyn Runtime>,
    agent_blueprint: AgentBlueprint,
}

#[async_trait]
impl Skill for SubAgentSkill {
    async fn execute(&self, args: serde_json::Value, ctx: SkillContext) 
        -> Result<BoxStream<...>, SkillError> 
    {
        // 在父 trajectory 之下创建子 task
        let sub_task = TaskSpec {
            parent_trajectory: Some(ctx.trajectory),
            blueprint: self.agent_blueprint.clone(),
            input: args,
            budget: ctx.budget.subdivide(0.3).unwrap_or_default(),  // 子任务分得 30% 预算
            ...
        };
        
        let handle = self.inner_runtime.submit(sub_task, ctx.principal.clone()).await?;
        let inner_events = self.inner_runtime.subscribe(handle.task_id);
        
        // 把内部 TrajectoryEvent 翻译为外部 SkillEvent
        let mapped = inner_events.map(|ev| Self::project(ev));
        Ok(Box::pin(mapped))
    }
}
```

**关键**：子任务的 trajectory 是父 trajectory 的子节点（Doc 04 §3.1 `parent` 字段），事件流被父 trajectory 的事件日志吸纳。回溯 / 恢复语义完全继承。

---

## 7. 工具调用生命周期（与 Doc 04 副作用分类协同）

每次 Tool 调用产出一组 `SideEffect`，按 Doc 04 §4.4 分类。Runtime 在调用前后的处理：

### 7.1 调用前

```rust
fn check_side_effect_allowed(
    tool: &dyn Tool,
    trajectory_phase: TrajectoryPhase,
) -> Result<(), ToolError> {
    let kind = tool.descriptor().side_effect.clone();
    
    match (kind, trajectory_phase) {
        // Pure / Isolated / Reversible 在任何阶段都允许
        (SideEffectKind::PureCompute, _) => Ok(()),
        (SideEffectKind::IsolatedWrite { .. }, _) => Ok(()),
        (SideEffectKind::ReversibleResource { .. }, _) => Ok(()),
        
        // Irreversible 只允许在 Commit Phase
        (SideEffectKind::Irreversible { .. }, TrajectoryPhase::Commit) => Ok(()),
        (SideEffectKind::Irreversible { resource }, _) => {
            Err(ToolError::PermissionDenied {
                required: Scope::CommitPhaseAccess,
                principal: ctx.principal.clone(),
            })
        }
    }
}
```

### 7.2 调用后注册补偿

成功的 ReversibleResource 类调用必须把补偿动作登记到 trajectory 的 pending_compensations：

```rust
async fn invoke(&self, ...) -> Result<ToolOutput, ToolError> {
    let result = self.actual_invoke(...).await?;
    
    for effect in &result.side_effects {
        if let SideEffectKind::ReversibleResource { cleanup, .. } = &effect.kind {
            self.trajectory_store.append_compensation(
                ctx.trajectory,
                CompensationAction {
                    id: CompensationId::new(),
                    action: cleanup.clone(),
                    target: effect.resource.clone(),
                    idempotency_key: effect.idempotency_key.clone(),
                },
            ).await?;
        }
    }
    
    Ok(result)
}
```

回溯时（Doc 04 §6）按 LIFO 顺序执行这些补偿。

---

## 8. 流式 Tool 输出

某些 Tool 天然是长任务且产出渐进式：
- `grep_codebase` 边搜边吐结果
- `run_test_suite` 测试一个个跑完
- `download_dataset` 进度条
- `query_postgres` 大查询的 row stream

`Tool::invoke_stream` 返回 `BoxStream<Result<ToolEvent>>`，对接 Frontend 的实时进度展示。

**约束**：流式 Tool 仍然必须能聚合为一个 ToolOutput，给 LLM 当 tool result。聚合策略由 Tool 自己实现（concat 文本 / merge JSON / 取最后一个）。

---

## 9. 配置形态

```toml
# 内置 Tool
[[tools.builtin]]
id = "git.fetch_pr_diff"
required_scopes = ["git:read"]
timeout_secs = 30

[[tools.builtin]]
id = "fs.read_file"
required_scopes = ["fs:read"]
timeout_secs = 5

# MCP Server
[mcp_servers.github]
type = "stdio"
binary = "/usr/local/bin/mcp-github"
args = []
mode = "long_lived"
session_idle_timeout_secs = 300
auth = { kind = "delegate", per_tenant_home = true }

[mcp_servers.github.tools.create_issue]
enabled = true
required_scopes = ["github:write"]
side_effect_override = "Irreversible"        # 强制升级为不可逆,只能在 commit phase

[mcp_servers.github.tools.read_repo]
enabled = true
required_scopes = ["github:read"]

# Skill
[[skills]]
id = "security_audit_full"
source = { type = "native", impl = "crate::skills::SecurityAuditSkill" }
required_scopes = ["security:read", "github:read"]
required_tools = ["git.fetch_pr_diff", "sast.run_semgrep"]
max_duration_secs = 600

[[skills]]
id = "code_smell_detection"
source = { type = "declarative", spec_file = "skills/code_smell.yaml" }

# Idempotency cache
[tool_invocation.idempotency]
backend = "redis"
ttl_secs = 86400

# 对每个 Agent role 的可见 Tool 集合
[agent_capabilities.orchestrator]
tools = ["fs.read_file", "git.fetch_pr_diff"]   # 只允许读类
skills = ["security_audit_full", "code_smell_detection"]

[agent_capabilities.code_review_worker]
tools = ["fs.read_file", "git.fetch_pr_diff", "sast.run_semgrep"]

[agent_capabilities.commit_phase_agent]
tools = ["github.create_issue", "git.push_branch"]   # 只有 commit phase agent 能调不可逆
```

---

## 10. 测试策略

### 10.1 Tool 单测

每个 Tool 独立测试，mock 外部依赖：

```rust
#[tokio::test]
async fn read_file_returns_business_error_on_missing() {
    let tool = ReadFileTool::new(MockFs::without_files());
    let result = tool.invoke(json!({ "path": "/nonexistent" }), test_ctx()).await;
    
    assert!(matches!(
        result,
        Err(ToolError::BusinessError { code, retriable: false, .. })
            if code == "file_not_found"
    ));
}
```

### 10.2 Schema Conformance

每个 Tool 实现都跑同一套 schema 校验测试：
- input_schema 接受 OpenAI strict mode（不含 `oneOf` / 复杂 pattern）
- output_schema 描述的字段在实际 output 中都存在
- input/output 都能成功 round-trip serde

### 10.3 MCP 集成测试

启动一个 reference MCP server (filesystem reference impl)，验证：
- discovery 列出所有声明的 tool
- list_for 按 IAM 过滤
- invoke 端到端成功
- subprocess 在空闲超时后被 kill
- subprocess crash 后下次 invoke 自动重启

### 10.4 Idempotency 测试

```rust
#[tokio::test]
async fn duplicate_invocation_returns_cached_output() {
    let tool = SpyTool::new();
    let registry = test_registry_with(&tool);
    
    let key = "test-key-1".to_string();
    let ctx1 = ToolContext { idempotency_key: key.clone(), ..test_ctx() };
    let ctx2 = ToolContext { idempotency_key: key, ..test_ctx() };
    
    let r1 = registry.invoke(&tool.id(), json!({}), ctx1).await.unwrap();
    let r2 = registry.invoke(&tool.id(), json!({}), ctx2).await.unwrap();
    
    assert_eq!(r1.result, r2.result);
    assert_eq!(tool.invocation_count(), 1);  // 实际只调了一次
}
```

### 10.5 副作用 Gate 测试

```rust
#[tokio::test]
async fn irreversible_tool_rejected_outside_commit_phase() {
    let registry = test_registry();
    let ctx = ToolContext { 
        trajectory_phase: TrajectoryPhase::Worker,
        ..test_ctx() 
    };
    
    let result = registry.invoke(
        &"github.create_issue".into(),
        json!({ "title": "x" }),
        ctx,
    ).await;
    
    assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
}
```

---

## 11. 反模式清单

1. **不要让 LLM 决定调用哪个 Skill**——Orchestrator 在代码里圈定可调用集合，LLM 只在集合内选 Tool。
2. **不要把 MCP server 声明的 scope 直接信任**——本地配置覆盖。
3. **不要在 ToolRegistry 之外直接 spawn MCP 子进程**——所有子进程生命周期统一管理。
4. **不要让 Tool 持有跨调用的可变状态**——状态外置（DB / cache）。
5. **不要在 Tool 实现里读 env vars**——通过 ToolContext 注入配置。
6. **不要把 Skill 实现得依赖 Tool 名字字符串硬编码**——通过 `required_tools` 声明 + 注入获取。
7. **不要在 Tool 内部启动 LLM 调用而不走 Pipeline**——Tool 实现里如果要用 LLM，必须通过 ctx.llm_service。
8. **不要把 Resources / Prompts 交给 MCP 控制系统行为**——Resources 仅供 Skill 内部使用，Prompts 完全不集成。
9. **不要用同一个 idempotency_key 跨 trajectory**——key 必须包含 trajectory_id，避免误命中。
10. **不要让 declarative skill 表达条件分支 / 循环**——这种复杂度上 SubAgent 或 native。
11. **不要忽略 Tool 调用的 cancel**——长任务必须监听 cancel.cancelled()。
12. **不要在 commit phase 之前调用 Irreversible Tool**——Runtime 强制 reject，但更应该在 Skill 设计时就避免。
13. **不要把 MCP tool 的 description 直接展示给最终用户**——可能是不可信内容（prompt injection 攻击面）。
14. **不要让 Tool 的 output_schema 用 `additionalProperties: true`**——所有字段必须显式声明，否则下游 Agent 拿到不可预期的结构。
15. **不要复用 LLM cache 来缓存 Tool 输出**——两套 store 独立，语义不同（LLM cache 看 prompt hash，Tool cache 看 idempotency_key）。

---

## 12. 与上下游的契约

### 上游 (Agent / Skill) 承诺

- 通过 `AgentContext::tool_registry` 获取 ToolRegistry，不绕开
- 每次调用提供完整 ToolContext（含 idempotency_key、trajectory、principal）
- 不假设 Tool 调用顺序——并行 tool call 必须能正确处理交错

### 下游 (MCP Server / HTTP API / Built-in) 契约

- 输入符合 input_schema 时必须能正确处理
- 输出符合 output_schema（Runtime 校验）
- 接受并尊重 idempotency_key（重复 key 必须返回相同结果或 Conflict 错误）
- 长任务支持 cancel 信号
- 副作用清单与 declared_side_effects 一致

### Runtime / Pipeline 边界

- Tool 调用的事件（ToolCalled / ToolCompleted / ToolFailed）写入 Doc 04 §3.2 的事件日志
- Tool 失败的错误分类（Permanent / Retriable）影响 Doc 04 §6 的 backtrack 决策
- Tool 的 IAM 校验**与** Doc 02 §4.2 LLM Pipeline 的 IAM 校验是**两层独立校验**——principal 通过 LLM IAM 不代表通过 Tool IAM

---

## 13. 待办与开放问题

- [ ] MCP 协议的版本演进策略（v1 → v2 时如何兼容老 server）
- [ ] Skill 的版本管理（同 Skill 不同版本是否可以共存，如何路由）
- [ ] Declarative Skill 的执行器选型（自研 mini DSL vs 复用 Step Functions / Argo Workflows）
- [ ] 跨进程 / 跨节点的 Tool 调用追踪（trace 透传 OTel context 进 MCP）
- [ ] MCP server 的资源限额（CPU / 内存 / 文件句柄）—— cgroups vs nsjail vs 信任
- [ ] Tool 的成本计费维度（按调用次数 / 按耗时 / 按数据量）
- [ ] Skill 内部产生的中间产物是否要持久化到 ContentStore（debug 价值 vs 存储成本）
- [ ] MCP Server 的健康检查协议（heartbeat / circuit breaker）
- [ ] **RAG / Vector Store 集成章节**: 目前文档明确不内置 vector store (定位为应用层),但应补一节"Tool/Skill 接入向量检索的钩子"。Personal mode 推荐 in-process mmap 方案 (faiss-rs / lance) 避免起独立 vector DB 进程;Team/SaaS mode 推荐 Qdrant/Milvus 作为外部 service tool。
