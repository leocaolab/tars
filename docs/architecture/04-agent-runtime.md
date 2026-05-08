# 文档 04 — Agent Runtime 与 Trajectory Tree

> 范围：定义多 Agent 协同的运行时抽象——拓扑、状态模型、消息契约、回溯与恢复机制、Frontend 契约。
>
> 上游（消费方）：CI Mode / TUI Mode / Web Dashboard 等 Frontend Adapter，详见 Doc 07。
>
> 下游（依赖）：Doc 02 Middleware Pipeline → Doc 01 LlmProvider；Doc 03 Cache Registry；Doc 05 Tool/MCP（待定）。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **DAG 是计划，不是运行时** | 执行 shape 是带回溯、循环、放弃的状态机。Plan as DAG, execute as event-sourced trajectory tree |
| **崩溃恢复** | 进程死掉重启后能从最后一个 checkpoint 恢复任务，不丢失已完成的工作 |
| **回溯安全** | 任何已执行的副作用都必须能被补偿（或被设计为不进入中间步骤） |
| **预算硬约束** | 每个任务有 token / 时长 / agent hops / replan 次数的硬上限，到了直接 abort，不允许失控 |
| **Frontend 无关** | Runtime 只产出 `TrajectoryEvent` 流，不知道 CI / TUI / Web 的存在 |
| **多 Agent 强契约** | Agent 间消息走 Rust enum + provider strict structured output，禁止纯文本互喷 |
| **可观测可重放** | 完整事件日志 + LLM 响应捕获，任何历史任务都能在测试环境复现 |

**反目标**：
- 不做自由 mesh / P2P agent 通信（O(n²) 复杂度，无法 debug）
- 不让 LLM 决定路由 / 任务完成 / 工具选择（这些必须在代码里确定性决策）
- 不在 Runtime 层做 prompt 模板渲染（那是 Prompt Builder 的责任，详见 §4.5）
- 不内嵌特定 Frontend（trace 显示、对话 UI、Markdown 报告生成都是 Adapter 的事）

---

## 2. 拓扑：分层编排 + DAG Worker + Critic

### 2.1 强制结构

```
                    ┌────────────────────────┐
                    │  Orchestrator (Planner) │
                    │  L1 模型 + strict JSON  │
                    └────────────┬───────────┘
                                 │ 下发 task DAG
                ┌────────────────┼────────────────┐
                ▼                ▼                ▼
         ┌──────────┐     ┌──────────┐     ┌──────────┐
         │ Worker A │     │ Worker B │     │ Worker C │  ← 可并行的 leaf
         │ L2 / L2' │     │ L2 / L2' │     │ L2 / L2' │
         └────┬─────┘     └────┬─────┘     └────┬─────┘
              └────────────────┼────────────────┘
                               ▼
                    ┌──────────────────────┐
                    │  Aggregator          │
                    │  纯代码,无 LLM       │
                    └──────────┬───────────┘
                               ▼
                    ┌──────────────────────┐
                    │  Critic (独立轮次)    │
                    │  L3 (Worker 同档)    │
                    └──────────┬───────────┘
                               │
                  ┌────────────┴────────────┐
                  ▼                         ▼
            accept (commit phase)    replan (fork from parent)
```

### 2.2 强制规则

1. **Orchestrator 不做 reasoning**——它的输出永远是结构化的 task DAG（schema 约束）。需要思考的工作分发给 Worker。
2. **Worker 之间不直接通信**——所有协作走 Context Store 的 append-only 事件流（§6）
3. **Critic 是单独一轮**——不复用 Worker 的对话上下文，避免被推理路径污染
4. **Aggregator 是纯代码**——不调 LLM，做 schema 拼接、去重、排序等确定性操作
5. **mesh / P2P 通信被禁止**——任何 "Agent X 直接调用 Agent Y" 在编译期就拒绝

### 2.3 拒绝 mesh 的理由

mesh 拓扑（autogen 早期、CrewAI 自由模式）的失败模式：
- 消息复杂度 O(n²)
- token 成本指数增长（每条消息要带前序所有消息）
- debug 不可能（哪个 Agent 的哪句话导致了最终 bug？）
- 无法 budget 预算（环路无法静态分析）

工业级生产环境的多 Agent 系统，最终全部收敛到 hierarchical orchestrator + structured DAG + critic loop 的形态。LangGraph、OpenAI Swarm、CrewAI 的"hierarchical mode" 都是这个形状。

---

## 3. 核心数据模型

### 3.1 Trajectory Tree

```rust
pub struct Trajectory {
    pub id: TrajectoryId,
    pub root_task: TaskId,
    pub parent: Option<TrajectoryId>,         // 形成树
    pub branch_reason: BranchReason,           // 为什么从 parent 分出来
    pub status: TrajectoryStatus,
    pub head_state: StateRef,                  // 指向 event log 的 offset
    pub pending_compensations: Vec<CompensationAction>,
    pub budget_remaining: TaskBudget,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

pub enum TrajectoryStatus {
    Active,
    Suspended { reason: SuspendReason },
    Completed { result: TaskResult },
    Dead { cause: DeathCause },           // 被 backtrack 标记死亡
}

pub enum BranchReason {
    Root,                                    // 任务起点
    Replan { from: TrajectoryId, critic_feedback: String },
    Fork { from: TrajectoryId, hypothesis: String },     // tree-of-thoughts 风格
    Recovery { from: TrajectoryId, error: ErrorRef },    // 崩溃恢复时的新分支
}

pub enum DeathCause {
    BacktrackedAfterCriticReject,
    BacktrackedAfterError(ErrorRef),
    BudgetExhausted,
    DeadlineExceeded,
    ExplicitAbort,
}
```

### 3.2 事件日志（单一事实源）

```rust
pub enum AgentEvent {
    /// 任务诞生
    TaskCreated { task_id: TaskId, spec: TaskSpec, principal: Principal },
    
    /// 轨迹生命周期
    TrajectoryStarted { traj: TrajectoryId, parent: Option<TrajectoryId>, reason: BranchReason },
    TrajectorySuspended { traj: TrajectoryId, reason: SuspendReason },
    TrajectoryResumed { traj: TrajectoryId, by: ResumeTrigger },
    TrajectoryAbandoned { traj: TrajectoryId, cause: DeathCause },
    
    /// Agent 步骤
    StepStarted { 
        traj: TrajectoryId, 
        step_seq: u32,
        agent: AgentId, 
        idempotency_key: String,
        input_ref: ContentRef,           // 大输入存到 ContentStore,事件里只放引用
    },
    StepCompleted { 
        traj: TrajectoryId,
        step_seq: u32,
        output_ref: ContentRef,
        side_effects: Vec<SideEffectRef>,
        usage: Usage,
    },
    StepFailed { 
        traj: TrajectoryId,
        step_seq: u32,
        error: AgentError, 
        classification: ErrorClass,
    },
    
    /// 副作用补偿
    CompensationExecuted { 
        traj: TrajectoryId,
        compensation: CompensationRef,
        result: CompensationResult,
    },
    
    /// LLM 原始响应捕获 (replay 必需)
    LlmResponseCaptured { 
        traj: TrajectoryId,
        step_seq: u32,
        provider: ProviderId,
        raw_response: ContentRef,
    },
    
    /// 检查点 (减少 replay 成本)
    Checkpoint { 
        traj: TrajectoryId,
        step_seq: u32,
        state_snapshot: ContentRef,
    },
}
```

**关键不变量**：
1. **事件只追加不修改**——所有"修正"通过追加新事件实现（如 `TrajectoryAbandoned` 标记某条轨迹死亡）
2. **大 payload 走 ContentStore + ContentRef**——事件本身保持小（<4KB），便于按时间扫描和持久化
3. **`idempotency_key` 在 StepStarted 时确定**——格式 `hash(traj_id + step_seq + input_ref)`，所有外部调用（LLM、tool、DB）携带此 key，replay 时去重
4. **`LlmResponseCaptured` 与 `StepCompleted` 是两个事件**——前者保留 raw response 用于 replay，后者保存解析/聚合后的最终输出。这样即使解析逻辑后续改了，replay 还能用旧 raw response 重新解析

### 3.3 ContentStore：把大 payload 与事件解耦

```rust
#[async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, content: &[u8]) -> Result<ContentRef, StoreError>;
    async fn get(&self, refr: &ContentRef) -> Result<Vec<u8>, StoreError>;
    async fn delete(&self, refr: &ContentRef) -> Result<(), StoreError>;
}

pub struct ContentRef {
    pub hash: [u8; 32],              // SHA-256 内容寻址,自动去重
    pub size: u64,
    pub mime: String,
    pub backend: ContentBackend,     // Postgres bytea / S3 / FS
}
```

事件日志在 Postgres，ContentStore 可以是同一个 DB（小内容用 bytea），也可以是 S3 / 本地 FS（大内容如 LLM 全文响应、code review 报告）。

---

## 4. Agent 抽象与消息契约

### 4.1 Agent trait

```rust
#[async_trait]
pub trait Agent: Send + Sync {
    fn id(&self) -> &AgentId;
    fn role(&self) -> AgentRole;
    
    /// 单步执行：纯函数 (state, input) → (output, side_effects)
    async fn execute(
        &self,
        ctx: AgentContext,
        input: AgentInput,
    ) -> Result<AgentStepResult, AgentError>;
    
    /// 声明本 Agent 可能产生的副作用类型
    /// Runtime 根据这个清单准备补偿能力
    fn declared_side_effects(&self) -> &[SideEffectKind];
}

pub struct AgentContext {
    pub trajectory: TrajectoryId,
    pub step_seq: u32,
    pub task_budget: BudgetView,             // 剩余预算
    pub principal: Principal,
    pub deadline: Option<Instant>,
    pub cancel: CancellationToken,
    pub llm_service: Arc<dyn LlmService>,    // Doc 02 的 pipeline
    pub context_store: Arc<dyn ContextStore>,
    pub tool_registry: Arc<dyn ToolRegistry>, // Doc 05
}

pub enum AgentRole {
    Orchestrator,
    Worker { domain: String },               // "code_review" / "security_audit" / ...
    Critic,
    Aggregator,                              // 不调 LLM 的纯代码 Agent
}

pub struct AgentStepResult {
    pub output: AgentMessage,
    pub side_effects: Vec<SideEffect>,
    pub usage: Usage,
    pub raw_llm_response: Option<ContentRef>,  // 用于 replay
}
```

### 4.2 AgentMessage（强类型消息契约）

所有 Agent 间通信走同一个 enum。**禁止纯文本互喷**：

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum AgentMessage {
    /// Orchestrator 下发任务
    PlanIssued {
        plan_id: PlanId,
        nodes: Vec<PlanNode>,
        edges: Vec<PlanEdge>,
    },
    
    /// Worker 上报中间产物
    PartialResult {
        agent: AgentId,
        artifact: Artifact,
        confidence: f32,
    },
    
    /// Worker 需要更多上下文
    NeedsClarification {
        agent: AgentId,
        question: String,
        blocking: bool,
    },
    
    /// Critic 评审结论
    CriticVerdict {
        verdict: Verdict,
        rejected_reasons: Vec<String>,
        replan_hints: Vec<String>,
    },
    
    /// 失败上报
    Failed {
        agent: AgentId,
        error: AgentError,
        recoverable: bool,
    },
    
    /// 任务完成
    Completed {
        final_artifact: Artifact,
        total_usage: Usage,
    },
}
```

**与 LLM 的对接**：每个 Agent 在调 LlmService 时，`req.structured_output = Some(AgentMessage::json_schema_for_role(self.role()))`，让 provider 端 strict mode 直接产出 `AgentMessage` 子结构。Rust 端 `serde_json::from_str` 一步反序列化，零字符串拼接 / 正则提取。

### 4.3 模型分层（不让 LLM 自己路由）

```rust
impl Agent for OrchestratorAgent {
    async fn execute(&self, ctx: AgentContext, input: AgentInput) -> ... {
        let req = ChatRequest {
            model: ModelHint::Tier(ModelTier::Default),  // L1 主力档,要求稳定 JSON
            structured_output: Some(AgentMessage::schema_for(AgentRole::Orchestrator)),
            ...
        };
        // ...
    }
}

impl Agent for ReasoningWorker {
    fn execute(&self, ctx: AgentContext, input: AgentInput) -> ... {
        let req = ChatRequest {
            model: ModelHint::Tier(ModelTier::Reasoning),  // L2 顶级模型
            ...
        };
    }
}

impl Agent for FastClassifierWorker {
    // 不用 LLM,直接调 Doc 01 的 ClassifierProvider (DeBERTa)
}
```

四档分配：
- L1（Orchestrator / 工具选择）：稳定 JSON，中等成本（gpt-4o-mini / claude-haiku）
- L2（Reasoning Worker）：reasoning 主力（claude-opus / o1 / gemini-pro）
- L2'（Specialized Worker）：领域优化（qwen-coder 跑代码 / gemini-flash 跑长文档）
- L3（Critic）：与 Worker 同档或低半档
- L4（路由 / 抽取 / 分类）：本地小模型（DeBERTa / qwen-0.5B / bge-small）

### 4.4 副作用分类

```rust
pub enum SideEffectKind {
    /// 纯计算,无外部副作用 (LLM 推理本身,虽然花钱但语义可重做)
    PureCompute,
    
    /// 在隔离环境的写入 (temp dir / shadow branch / staging table)
    /// 自动补偿:删除/回滚
    IsolatedWrite { resource: ResourceRef, undo: UndoAction },
    
    /// 创建可逆的云资源 (cache handle / temp bucket)
    /// 显式补偿:调 delete API
    ReversibleResource { resource: ResourceRef, cleanup: CleanupAction },
    
    /// 不可逆的外部动作 (git push / 发邮件 / 付款 / 删生产数据)
    /// 必须延迟到 Commit Phase,不允许出现在中间步骤
    Irreversible { resource: ResourceRef },
}

pub struct SideEffect {
    pub kind: SideEffectKind,
    pub idempotency_key: String,          // 重放时去重
    pub created_at: SystemTime,
}
```

**强制约束**：Runtime 在每次 StepCompleted 后扫描 side_effects，如果发现 `Irreversible` 出现在非 Commit Phase，**直接 panic / 拒绝该步骤**。这是工程纪律层面的硬约束，不是 LLM 自觉。

---

## 5. Context Store：append-only 共享上下文

```rust
#[async_trait]
pub trait ContextStore: Send + Sync {
    /// Agent 完成后追加事件
    async fn append(&self, event: AgentEvent) -> Result<EventOffset, StoreError>;
    
    /// 拉取上游产出 (按 trajectory + 时间窗 / step 范围过滤)
    async fn fetch_for_agent(
        &self,
        traj: TrajectoryId,
        agent_input_spec: &AgentInputSpec,    // 该 Agent 声明感兴趣的 event 类型
    ) -> Result<Vec<AgentEvent>, StoreError>;
    
    /// 流式订阅 (Frontend Adapter 用)
    async fn subscribe(
        &self,
        traj_filter: TrajectoryFilter,
    ) -> Result<BoxStream<'static, AgentEvent>, StoreError>;
    
    /// 重放：从指定 offset 起按时间序回放
    async fn replay_from(
        &self,
        traj: TrajectoryId,
        from_offset: EventOffset,
    ) -> Result<BoxStream<'static, AgentEvent>, StoreError>;
}
```

### 5.1 Context Compactor

Agent 数量一多，原始事件流增长指数级——不能把所有原始事件都塞给下个 Agent。中间引入压缩环节：

```rust
pub trait ContextCompactor: Send + Sync {
    /// 把 raw 事件流压缩为下一个 Agent 需要的最小必要上下文
    async fn compact(
        &self,
        events: Vec<AgentEvent>,
        target_agent: &AgentId,
        max_tokens: u32,
    ) -> Result<CompactedContext, CompactorError>;
}
```

实现策略：
- **Schema-aware**：只保留 target agent 声明感兴趣的事件类型
- **Recency window**：默认只看最近 N 条事件
- **LLM 压缩 (fallback)**：如果上面两步还超 max_tokens，用 L4 模型做摘要

**关键**：压缩本身是 deterministic 的（schema/window 模式），LLM 摘要是降级方案。否则压缩本身引入非确定性，replay 不可重现。

### 5.2 持久化形态

```
Postgres:
  agent_events     (id BIGSERIAL, traj_id UUID, event JSONB, created_at TIMESTAMPTZ)
  trajectories    (id UUID, parent UUID, status, budget_remaining JSONB, ...)
  content_refs    (hash BYTEA PRIMARY KEY, size, mime, backend, payload BYTEA NULL)
  
S3 / FS (大 content):
  content/{first2}/{remaining30}.bin    -- ContentRef 内容寻址
```

事件日志按 `traj_id` 分区，按 `created_at` 索引。30 天后归档冷存储（S3 Glacier）。

---

## 6. Backtrack：Saga 补偿

### 6.1 触发场景

1. **Critic 拒绝**：fork from parent，注入 critic 反馈作为 replan hint
2. **Step 失败（永久错误）**：trajectory 死亡 + 补偿 + parent 接管
3. **Agent 卡死（超时）**：同上
4. **预算耗尽（剩余 < 当前 step 估算）**：suspend，不补偿（保留中间产物）

### 6.2 回溯流程

```rust
impl Runtime {
    async fn backtrack(
        &self,
        traj: TrajectoryId,
        target: BacktrackTarget,
    ) -> Result<TrajectoryId, RuntimeError> {
        let trajectory = self.load_trajectory(traj).await?;
        
        // 1. 标记当前轨迹死亡 (阻止任何并发写入)
        self.event_store.append(AgentEvent::TrajectoryAbandoned { 
            traj, 
            cause: target.death_cause() 
        }).await?;
        
        // 2. 反向执行补偿
        for comp in trajectory.pending_compensations.iter().rev() {
            match comp.execute().await {
                Ok(_) => {
                    self.event_store.append(AgentEvent::CompensationExecuted {
                        traj,
                        compensation: comp.id.clone(),
                        result: CompensationResult::Success,
                    }).await?;
                }
                Err(e) if e.already_undone() => continue,  // 幂等,跳过
                Err(e) => {
                    // 补偿失败 = 严重事件,升级人工
                    self.event_store.append(AgentEvent::CompensationExecuted {
                        traj,
                        compensation: comp.id.clone(),
                        result: CompensationResult::Failed(e.to_string()),
                    }).await?;
                    self.escalate_to_human(traj, &comp, &e).await?;
                    return Err(RuntimeError::CompensationFailed(e));
                }
            }
        }
        
        // 3. 找 fork 点
        let resume_state = match target {
            BacktrackTarget::ParentTrajectory => trajectory.parent_state(),
            BacktrackTarget::Checkpoint(cp_id) => self.load_checkpoint(cp_id).await?,
        };
        
        // 4. 开新 trajectory,带 hint 喂给 planner
        let new_traj = self.fork_trajectory(
            resume_state,
            BranchReason::Replan {
                from: traj,
                critic_feedback: target.feedback().to_string(),
            },
        ).await?;
        
        Ok(new_traj)
    }
}
```

### 6.3 补偿失败的处置

补偿失败比执行失败更可怕——意味着系统进入了不一致状态（部分副作用残留）。处置流程：

1. 立即冻结相关资源（资源访问短路返回 503，避免基于不一致状态做新决策）
2. 升级到 oncall（PagerDuty / Slack alert）
3. 把失败的补偿挂入"待人工处理"队列
4. 该 trajectory 的下游全部 suspend

绝不允许"补偿失败但程序继续跑"。

### 6.4 回溯深度上限

```rust
pub struct BacktrackPolicy {
    pub max_backtrack_depth: u8,         // 默认 3
    pub max_replans_per_task: u8,        // 默认 2
}
```

实际生产中：
- > 80% 回溯只回 1-2 步
- > 5 步的回溯通常意味着任务定义有问题，正确做法是 abandon 整个 task + 通知人

超过上限直接 suspend，不强行继续。

---

## 7. 恢复：Replay + Checkpoint + Idempotency

### 7.1 进程崩溃恢复

启动时扫描所有 `Active` / `Suspended` trajectory，对每条：

```rust
async fn recover_trajectory(&self, traj: TrajectoryId) -> Result<(), RuntimeError> {
    // 1. 找最近的 checkpoint,避免从头 replay
    let checkpoint = self.event_store.find_latest_checkpoint(traj).await?;
    let mut state = checkpoint.map(|c| self.load_state(&c.state_snapshot)).await?
        .unwrap_or_else(State::initial);
    
    // 2. Replay checkpoint 之后的事件
    let events = self.event_store.events_since(traj, checkpoint.offset).await?;
    let mut in_flight: Option<InFlightStep> = None;
    
    for event in events {
        match &event {
            AgentEvent::StepStarted { step_seq, agent, idempotency_key, .. } => {
                in_flight = Some(InFlightStep {
                    step_seq: *step_seq,
                    agent: agent.clone(),
                    idempotency_key: idempotency_key.clone(),
                });
            }
            AgentEvent::StepCompleted { .. } | AgentEvent::StepFailed { .. } => {
                in_flight = None;
            }
            _ => {}
        }
        state.apply(event);
    }
    
    // 3. 如果有 in-flight step (崩溃发生时正在执行),用 idempotency_key 重新触发
    //    LLM 调用通过 prompt hash 命中缓存 (Doc 03);tool 调用通过 idempotency_key 去重
    if let Some(step) = in_flight {
        self.retry_step_with_idempotency(traj, step).await?;
    }
    
    // 4. 恢复正常调度
    self.resume_normal_execution(traj, state).await
}
```

**关键不变量**：
- `idempotency_key = hash(traj_id + step_seq + input_ref)`——稳定，replay 时一致
- 所有外部调用（LlmService、ToolRegistry、ContextStore）必须接受并尊重 idempotency_key
- LLM 调用通过 Doc 03 cache 自然去重（同 prompt hash 命中）

### 7.2 Replay 的非确定性陷阱

`temperature > 0` 的 LLM 调用是非确定的，replay 出的 token 流和原始不一致 → state 发散。两个解：

**方案 A（默认）**：所有 LLM 响应通过 cache 强制确定化
- 第一次调用：cache miss + 真实生成 + 写入 cache
- replay：cache hit，拿到相同响应

**方案 B（更彻底）**：把 LLM raw output 作为事件持久化
- `AgentEvent::LlmResponseCaptured` 在每次 LLM 调用后追加
- replay 时检测到该事件，跳过真实 LLM 调用，直接从事件读 raw response
- 这是 Temporal / Cadence 的正统做法
- 代价：事件日志膨胀（每次响应可能几十 KB）

我们采用**方案 A 为主 + 方案 B 为备**：cache 是常规路径，LlmResponseCaptured 只在 trace level 启用（debug / 法规审计场景）。

### 7.3 Checkpoint 策略

```rust
pub struct CheckpointPolicy {
    pub strategy: CheckpointStrategy,
}

pub enum CheckpointStrategy {
    /// 固定每 N 步打一个 checkpoint
    EveryNSteps(u32),
    
    /// 按 step 耗时加权:LLM 调用步骤后必打,廉价本地步骤跳过
    Cost { llm_step_must_checkpoint: bool, local_step_threshold_ms: u64 },
    
    /// 按 trajectory 分支点打 (fork 之前)
    AtForkPoints,
    
    /// 组合
    Combined(Vec<CheckpointStrategy>),
}
```

默认策略：`Combined([EveryNSteps(5), AtForkPoints, Cost { llm_step_must_checkpoint: true, ... }])`

理由：LLM 调用是最大的恢复成本（钱 + 时间），打完 checkpoint 后崩溃只需重放后续廉价步骤。

### 7.4 Suspend / Resume

不是所有"未完成"都是失败——有些是正常等待：

```rust
pub enum SuspendReason {
    BudgetExhausted { 
        spent: Usage, 
        partial_result: Option<Artifact>,
    },
    AwaitingHumanApproval { 
        question: String, 
        choices: Vec<Choice>,
    },
    AwaitingExternalEvent { 
        event_type: String, 
        correlation_id: String,
    },
    RateLimited { 
        retry_after: Duration,
    },
}
```

Resume 是显式 API：

```rust
async fn resume(
    &self,
    traj: TrajectoryId,
    trigger: ResumeTrigger,
) -> Result<(), RuntimeError>;

pub enum ResumeTrigger {
    HumanApproval { decision: Choice },
    ExternalEventReceived { payload: serde_json::Value },
    BudgetIncreased { new_budget: TaskBudget },
    Manual,
}
```

---

## 8. 预算包络（TaskBudget）

```rust
pub struct TaskBudget {
    pub max_tokens_total: u64,        // 整个 task 全 Agent 加起来
    pub max_cost_usd: f64,             // 美金硬上限
    pub max_wall_clock: Duration,      // 墙钟时间
    pub max_agent_hops: u8,            // 最多多少次 Agent 调用,通常 ≤10
    pub max_replans: u8,               // Critic 触发 replan 的次数,通常 ≤2
    pub max_backtrack_depth: u8,       // 单次任务的回溯深度
    pub fallback: FallbackStrategy,
}

pub enum FallbackStrategy {
    /// 直接失败
    Fail,
    
    /// 用最后一个有效中间产物 + L1 模型生成总结
    BestEffortSummary,
    
    /// 挂起,通知人
    SuspendForHuman,
}
```

每次 Agent 调用前在 Middleware 里扣减；超额直接 abort 当前 step + 触发 fallback。

---

## 9. Cancel 传播

延续 Doc 02 §5 的链路，Runtime 是 cancel 的发起方之一：

```
Frontend 用户按 Ctrl+C / 点击"停止"
       │
       ▼
Frontend Adapter 调 runtime.cancel_task(task_id)
       │
       ▼
Runtime 设置 task 的所有 active trajectories 的 cancel_token
       │
       ▼
正在执行的 Agent 监听 ctx.cancel.cancelled() → 提前返回
       │
       ▼
LlmService stream Drop → Doc 01 §6.2.1 的 CLI subprocess interrupt
```

每个 Agent 必须在长操作（LLM 调用、tool 调用）配合 `tokio::select!` 监听 cancel：

```rust
async fn execute(&self, ctx: AgentContext, input: AgentInput) -> Result<...> {
    tokio::select! {
        result = self.do_work(&ctx, input) => result,
        _ = ctx.cancel.cancelled() => Err(AgentError::Cancelled),
    }
}
```

---

## 10. 与 Pipeline 的集成

Agent Runtime 是 Pipeline 的**消费者**，不是 wrapper。每个 Agent 通过 `AgentContext::llm_service` 调用 Pipeline，获得已经过 IAM / Cache / Guard / Routing 处理的 LLM 能力：

```rust
async fn execute(&self, ctx: AgentContext, input: AgentInput) -> Result<...> {
    let req = ChatRequest {
        model: ModelHint::Tier(ModelTier::Reasoning),
        system: Some(self.build_system_prompt()),     // 详见 §11
        messages: self.build_messages(&input),
        structured_output: Some(self.output_schema()),
        ...
    };
    
    let req_ctx = RequestContext {
        trace_id: ctx.trace_id(),
        tenant_id: ctx.tenant_id(),
        session_id: ctx.session_id(),
        principal: ctx.principal.clone(),
        deadline: ctx.deadline,
        cancel: ctx.cancel.clone(),
        budget: ctx.task_budget.handle(),
        attributes: HashMap::new(),
    };
    
    let mut stream = ctx.llm_service.clone().call(req, req_ctx).await?;
    let mut accumulator = AgentMessageAccumulator::new();
    while let Some(event) = stream.next().await {
        accumulator.apply(event?);
    }
    
    let message: AgentMessage = accumulator.finalize()?;
    Ok(AgentStepResult { 
        output: message, 
        side_effects: vec![],
        usage: accumulator.usage(),
        raw_llm_response: Some(accumulator.raw_response_ref()),
    })
}
```

Runtime 不知道 Pipeline 内部有几层 Middleware——对它来说就是一个 LlmService。

---

## 11. Prompt Builder 的契约（与 Cache 协同）

Doc 03 §10.5 已规定：上层（即 Agent Runtime）必须按 Static Prefix / Project Anchor / Dynamic Suffix 三段式拼装，让 Cache 能跨 session 共享。

Runtime 提供 `PromptBuilder` 抽象：

```rust
pub trait PromptBuilder: Send + Sync {
    /// 完全静态部分 (system + tools schema + 通用规范)
    /// 月级变动,变动时全公司 cache miss
    fn static_prefix(&self) -> &str;
    
    /// 项目锚定部分 (绑定 commit hash 的代码库 / 文档)
    /// 随 commit 变动,变动时该项目 cache miss
    fn project_anchor(&self, project: &ProjectRef) -> Result<String, PromptError>;
    
    /// 动态后缀 (本次请求特有内容)
    /// 每次都变,绝不进缓存
    fn dynamic_suffix(&self, input: &AgentInput) -> String;
    
    /// 拼装为 ChatRequest
    fn build(&self, project: &ProjectRef, input: &AgentInput) 
        -> Result<ChatRequest, PromptError>;
}
```

**强制约束**：
1. `static_prefix()` 字节级稳定——配置变更前不允许调整顺序、空格、Markdown 格式
2. `project_anchor()` 必须是项目状态的纯函数——同 commit 必须返回同字节串
3. `dynamic_suffix()` 不能影响 cache key——它在 request 里位置必须固定为 messages 末尾

Runtime 启动时校验 PromptBuilder 实现的稳定性（对同输入多次调用，输出字节级一致）；不一致时拒绝启动。

---

## 12. Frontend 契约：TrajectoryEvent 流

Runtime 对外只暴露事件流，不感知消费者：

```rust
#[async_trait]
pub trait Runtime: Send + Sync {
    /// 提交新任务
    async fn submit(
        &self,
        spec: TaskSpec,
        principal: Principal,
    ) -> Result<TaskHandle, RuntimeError>;
    
    /// 订阅任务的事件流 (Frontend Adapter 用)
    fn subscribe(
        &self,
        task: TaskId,
    ) -> BoxStream<'static, TrajectoryEvent>;
    
    /// 显式控制
    async fn cancel(&self, task: TaskId) -> Result<(), RuntimeError>;
    async fn suspend(&self, task: TaskId) -> Result<(), RuntimeError>;
    async fn resume(&self, task: TaskId, trigger: ResumeTrigger) -> Result<(), RuntimeError>;
    
    /// 历史查询
    async fn query(&self, filter: TaskFilter) -> Result<Vec<TaskSnapshot>, RuntimeError>;
}

/// Frontend 看到的事件 (是 AgentEvent 的子集 + 派生事件)
pub enum TrajectoryEvent {
    TaskStarted { task: TaskId, spec: TaskSpec },
    AgentInvoked { agent: AgentId, input_summary: String },
    AgentCompleted { agent: AgentId, output_summary: String, usage: Usage },
    AgentFailed { agent: AgentId, error: String, will_retry: bool },
    PartialArtifact { artifact: ArtifactPreview },        // 可流式呈现的中间产物
    TrajectoryForked { from: TrajectoryId, into: TrajectoryId, reason: String },
    BackingOff { reason: String, eta: Duration },
    Suspended { reason: SuspendReason },
    Completed { final_artifact: Artifact, total_usage: Usage, total_cost: f64 },
    Failed { reason: String, partial_result: Option<Artifact> },
}
```

**TrajectoryEvent 不是 AgentEvent 的一对一暴露**——后者是内部事件溯源的细粒度记录，前者是外部消费者关心的"业务事件"。中间有一个 `EventProjector` 做转换：

```rust
trait EventProjector {
    fn project(&self, agent_event: &AgentEvent) -> Option<TrajectoryEvent>;
}
```

这个分离允许 Runtime 内部 schema 演进而不破坏 Frontend Adapter。

---

## 13. 测试策略

### 13.1 Replay-based 测试

记录真实任务的事件日志作为 fixture，replay 时断言行为：

```rust
#[tokio::test]
async fn replays_known_task_deterministically() {
    let fixture = load_fixture("code_review_with_critic_reject.json");
    let runtime = test_runtime_with_recorded_llm_responses(&fixture).await;
    
    let task = runtime.submit(fixture.spec.clone(), test_principal()).await.unwrap();
    let events: Vec<_> = runtime.subscribe(task).collect().await;
    
    assert_event_sequence_matches(&events, &fixture.expected_events);
}
```

### 13.2 崩溃恢复测试

```rust
#[tokio::test]
async fn recovers_from_crash_mid_step() {
    let runtime = test_runtime();
    let task = runtime.submit(spec(), principal()).await.unwrap();
    
    // 模拟崩溃：在第 3 步完成前 abort runtime
    let abort = tokio::spawn(async {
        wait_for_step(&runtime, task, 3, StepPhase::Started).await;
        runtime.simulate_crash().await;
    });
    
    abort.await.unwrap();
    
    // 重启 runtime,验证从 step 3 恢复
    let runtime2 = test_runtime_resume_from_db().await;
    let final_events: Vec<_> = runtime2.subscribe(task).collect().await;
    
    assert_eq!(final_events.last().unwrap().step_seq(), expected_final_step);
}
```

### 13.3 补偿测试

```rust
#[tokio::test]
async fn backtrack_runs_compensations_in_reverse() {
    let comp_log = Arc::new(Mutex::new(Vec::new()));
    let runtime = test_runtime_with_compensation_recorder(comp_log.clone());
    
    let task = runtime.submit(spec_that_creates_3_resources_then_fails(), principal())
        .await.unwrap();
    runtime.wait_until_done(task).await;
    
    // 验证补偿按 R3, R2, R1 顺序执行
    assert_eq!(*comp_log.lock().unwrap(), vec!["R3", "R2", "R1"]);
}
```

### 13.4 预算硬上限测试

```rust
#[tokio::test]
async fn aborts_when_budget_exhausted() {
    let runtime = test_runtime();
    let task = runtime.submit(
        TaskSpec {
            budget: TaskBudget {
                max_tokens_total: 100,  // 极小,确保第一次 LLM 调用就超
                ..Default::default()
            },
            ..spec()
        },
        principal(),
    ).await.unwrap();
    
    let result = runtime.wait_until_done(task).await;
    assert!(matches!(result, TaskResult::Failed { reason, .. } 
        if reason.contains("budget")));
}
```

---

## 14. 反模式清单

1. **不要用 LLM 决定"任务是否完成"**——用确定性的 schema 校验 + Critic 评分阈值。LLM 自评有乐观偏置。
2. **不要让 Worker 之间直接调用**——所有协作经过 Context Store。可观测性和 replay 依赖这个不变量。
3. **不要在 prompt 里塞 "你可以调用以下工具，请选择"**——Orchestrator 在代码里决定，不让 LLM 当 router。
4. **不要忽略 OTel trace 透传**——每个 task 一个 trace_id，每个 agent hop 一个 span。线上 hallucination debug 全靠这个。
5. **不要让 Critic 和 Worker 共享对话上下文**——Critic 必须冷启动看产出。
6. **不要在中间步骤做不可逆动作**——所有 git push / 发邮件 / 付款延迟到 Commit Phase。
7. **不要把 idempotency_key 作为可选字段**——从 Day 1 就强制，事后补几乎不可能。
8. **不要在事件日志里直接存大 payload**——走 ContentStore + ContentRef，事件保持 < 4KB。
9. **不要假设 LLM 输出非确定性可以"概率上接受"**——必须通过 cache 或事件捕获强制确定化，否则 replay 不可靠。
10. **不要让回溯深度无限**——硬上限（max_backtrack_depth），超过 suspend + 通知人。
11. **不要在补偿失败时静默继续**——升级人工 + 冻结资源。
12. **不要让 Frontend Adapter 直接读 AgentEvent**——通过 EventProjector 暴露 TrajectoryEvent，保护内部 schema 演进自由。
13. **不要在 PromptBuilder 里引入时间戳 / 随机数**——任何动态变量必须移到 dynamic_suffix。
14. **不要让 Agent 自由修改 Context Store**——只能 append 自己产出的事件，不能改其他 Agent 的事件。
15. **不要让长任务一次性塞满所有 context**——通过 Context Compactor 控制传给下个 Agent 的窗口大小。

---

## 15. 与上下游的契约

### 上游 (Frontend Adapter) 承诺

- 通过 `Runtime::submit` 提交完整的 TaskSpec（含 budget、principal、deadline）
- 通过 `subscribe` 消费 TrajectoryEvent 流
- 不直接访问 AgentEvent / Trajectory / 内部数据结构
- 处理 Suspend / Cancel 信号的 UI 状态

### 下游 (Pipeline) 契约

- 每个 Agent 通过 `AgentContext::llm_service` 获取已经过 Middleware 处理的 LLM 能力
- LLM 响应通过 ChatEvent 流消费，AgentMessageAccumulator 聚合
- 不绕过 Pipeline 直接调 Provider

### Side trait 契约 (Tool / MCP - Doc 05)

- `ToolRegistry` 提供工具清单和调用入口
- 工具调用必须接受 idempotency_key
- 工具必须声明 SideEffectKind，Runtime 据此判断是否允许在中间步骤执行

---

## 16. 待办与开放问题

- [ ] Trajectory 树深度的实际上限（Postgres 索引性能 vs 可读性）
- [ ] Context Compactor 的具体压缩算法选型（schema-aware vs LLM 摘要 vs hybrid）
- [ ] 与 Temporal/Restate 的对比是否值得直接采用（重 vs 轻）
- [ ] AgentMessage enum 的版本演进策略（serde 兼容 vs 显式 schema migration）
- [ ] 跨任务的 Agent 复用（如果两个 task 都需要 same agent，是否共享实例还是按需创建）
- [ ] Replay 时的 wall clock 模拟（事件里的 created_at 应该被 replay 时尊重还是用当前时间）
- [ ] 大规模 Trajectory 树的可视化（10+ 分支时 TUI / Web 怎么呈现）
- [ ] **Best-of-N / Parallel Sampling**: 高级模式——让 Orchestrator 一次出 N 个 plan 并行跑,任一成功即返回。适合代码修复 (N 个 patch 并发跑测试) / 高 stakes 决策 (N 个 critic 投票)。对应 ChatGPT o1 内部机制。成本 N 倍但成功率显著高,可能成 Doc 16 Advanced Patterns 主题。
