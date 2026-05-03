# 文档 00 — 总览与导航

> 本套文档定义 **TARS Runtime** 的完整架构设计——一个 Rust 实现的、面向多 Agent 协同的通用 LLM Runtime。
>
> 状态：设计先于实现。所有文档代表**目标架构**,实现按里程碑逐步对齐。

---

## 1. 这是什么

TARS 是一个**通用 Agent Runtime**,核心定位：

- **基础设施而非应用**——提供 Agent 编排 / LLM 调用 / Tool 集成的统一底座,业务 Agent 在其上构建
- **Rust 优先**——核心引擎用 Rust 写,追求高并发、低延迟、内存安全;通过 FFI / HTTP 暴露给其他语言
- **多 Provider 抽象**——同时支持 OpenAI / Anthropic / Gemini 的 API + CLI + 本地推理 (vLLM / mistral.rs / ONNX)
- **多 Agent 协同**——分层 Orchestrator + 并行 Worker + Critic Loop + Trajectory 树 + 事件溯源
- **多部署形态**——Personal Local-First (BYOK) / Team Self-Hosted / SaaS Multi-tenant / Hybrid Cloud Plane
- **生产级**——多租户硬隔离 / 安全护栏 / 可观测 / 备份恢复 / 应急响应一应俱全

**它不是什么**：
- 不是新的 LLM 模型
- 不是 ChatGPT 类产品 (是构建此类产品的底座)
- 不是 LangChain 替代品 (定位更底层,更工业)
- 不是单 Agent 框架 (默认设计为多 Agent)

---

## 2. 设计哲学

整套文档贯穿以下 8 条核心原则:

### 2.1 Layered, Not Monolithic
每层职责单一,相邻层通过 trait 解耦。任何层的实现可独立替换 (例如 Provider 换厂商 / Storage 换 Postgres 到 SQLite)。

### 2.2 Plan as DAG, Execute as State Machine
Agent 编排在计划阶段表达为 DAG,运行时实际是带回溯、循环、放弃的事件溯源状态机。两者不混用。

### 2.3 Tenant Isolation is Sacred
跨租户的数据 / 计算 / 副作用泄漏视为最严重缺陷。任何性能优化 (例如 cache 共享) 都不能突破租户边界。

### 2.4 Fail Closed
所有安全机制失败时拒绝请求,绝不"默认放行"。Auth / IAM / Cache / Budget / Schema / Side Effect 均如此。

### 2.5 Observable by Construction
可观测性 (M/E/L/T) 不是事后加上去的,而是从架构上保证——每个组件、每次调用、每个状态变更都产生可查询的信号。

### 2.6 Trust Nothing You Didn't Compute
LLM 输出、用户输入、Tool 返回、MCP server 行为——一切外部都不可信,经过显式过滤器才能影响系统状态。

### 2.7 Cost is a First-Class Concern
LLM 调用占成本 95%+。所有架构决策 (cache / routing / model tier / budget) 围绕成本可控展开。

### 2.8 Single Source of Truth
Rust trait 是真理之源,HTTP API / gRPC / Python / TypeScript 等是其投影。不允许某个 binding 偏离核心语义。

---

## 3. 整体架构

```
                       ┌──────────────────────────────┐
                       │   Frontend Adapters (Doc 07) │
                       │   CLI / TUI / Web / CI       │
                       └──────────────┬───────────────┘
                                      │
                       ┌──────────────▼───────────────┐
                       │   API Layer (Doc 12)         │
                       │   Rust / HTTP+SSE / gRPC     │
                       │   Python(PyO3) / TS(napi-rs) │
                       └──────────────┬───────────────┘
                                      │
                       ┌──────────────▼───────────────┐
                       │   Agent Runtime (Doc 04)     │
                       │   Trajectory Tree + Events   │
                       │   + Backtrack + Recovery     │
                       └────┬─────────────────┬──────┘
                            │                 │
              ┌─────────────▼─────┐  ┌────────▼─────────┐
              │  Tools / Skills   │  │  PromptBuilder   │
              │  (Doc 05)         │  │  (Doc 04 §11)    │
              │  Tool/MCP/Skill   │  │  Static Prefix / │
              │  3-layer abstract │  │  Project Anchor /│
              └─────────┬─────────┘  │  Dynamic Suffix  │
                        │            └────────┬─────────┘
                        │                     │
                        └──────────┬──────────┘
                                   │
                       ┌───────────▼───────────────┐
                       │  Middleware Pipeline      │
                       │  (Doc 02)                 │
                       │  Telemetry → Auth → IAM   │
                       │  → Budget → Cache → Guard │
                       │  → Routing → Breaker      │
                       └───────────┬───────────────┘
                                   │
              ┌────────────────────┼─────────────────┐
              │                    │                 │
   ┌──────────▼──────┐  ┌──────────▼─────┐  ┌──────▼──────┐
   │ Cache Registry  │  │ LLM Provider   │  │ Tool/MCP    │
   │ (Doc 03)        │  │ (Doc 01)       │  │ Subprocess  │
   │ L1/L2/L3 + ref  │  │ HTTP / CLI /   │  │ (Doc 05)    │
   │ counting +      │  │ Embedded       │  │ long-lived  │
   │ Janitor         │  │ adapters       │  │ + isolation │
   └─────────────────┘  └────────────────┘  └─────────────┘

   ▲ 横切支撑层 (所有上层都依赖)
   │
   ┌─────────────────────────────────────────────────────────┐
   │ Storage (Doc 09): Postgres / SQLite / Redis / S3        │
   │ Config + Multi-tenancy (Doc 06): 5-layer override       │
   │ Security Model (Doc 10): Auth / IAM / Encrypt / Audit   │
   │ MELT Observability (Doc 08): Metrics / Events / Logs / Traces │
   │ Performance (Doc 11): SLO / Capacity / Bench           │
   │ Operations (Doc 13): Runbook / Incident / Backup       │
   └─────────────────────────────────────────────────────────┘
```

---

## 4. 文档索引

| Doc | 标题 | 核心内容 | 适合谁先读 |
|---|---|---|---|
| **00** | 总览与导航 (本文) | 项目介绍 / 文档关系 / 阅读路径 | 所有人 |
| [01](./01-llm-provider.md) | LLM Provider 抽象 | 9 类后端统一 trait;CLI subprocess 复用;Tool call 三段式;Cache directive | LLM 集成开发者 |
| [02](./02-middleware-pipeline.md) | Middleware Pipeline | 10 层洋葱模型;IAM 前置;双通道 Guard;Cancel 传播 | 业务逻辑开发者 |
| [03](./03-cache-registry.md) | Cache Registry | 三级缓存 (L1/L2/L3);内容寻址;引用计数;租户隔离三道防线 | 性能/成本工程师 |
| [04](./04-agent-runtime.md) | Agent Runtime | Trajectory 树;事件溯源;Saga 补偿;恢复机制;Frontend 契约 | 核心架构师 |
| [05](./05-tools-mcp-skills.md) | Tools / MCP / Skills | 三层概念清晰区分;MCP 集成;Skill 三种实现 | Tool 开发者 |
| [06](./06-config-multitenancy.md) | 配置与多租户 | 5 层覆盖;锁定层;Secret 管理;租户生命周期 | DevOps / 平台工程 |
| [07](./07-deployment-frontend.md) | 部署与 Frontend | 4 种部署形态;CI / TUI / Web Dashboard;Hybrid 控制面 | 产品 + DevOps |
| [08](./08-melt-observability.md) | MELT 可观测性 | 三类数据流辨析;Cardinality 控制;敏感数据强制脱敏 | SRE |
| [09](./09-storage-schema.md) | Storage Schema | Postgres + SQLite + Redis + S3;分区;Migration;租户级清理 | 数据库工程师 |
| [10](./10-security-model.md) | 安全模型 | STRIDE 威胁模型;信任边界;隔离汇总;Prompt Injection 防御 | 安全工程师 |
| [11](./11-performance-capacity.md) | 性能与容量 | SLO 定义;瓶颈分析;Cache ROI;压测方法论 | 性能工程师 + SRE |
| [12](./12-api-specification.md) | API 规范 | Rust / HTTP / gRPC / Python(PyO3) / TS(napi-rs) / WASM | SDK 开发者 |
| [13](./13-operational-runbook.md) | 运维 Runbook | On-call playbook;12 个故障场景;备份恢复;应急沟通 | SRE / On-call |

---

## 5. 阅读路径

不同角色按以下顺序读最高效：

### 5.1 我是核心架构师
```
00 (本文) → 04 (核心 Runtime) → 02 (Middleware) → 01 (Provider)
→ 03 (Cache) → 05 (Tools) → 10 (Security) → 06 (Config)
→ 其余按需
```

### 5.2 我要为 TARS 开发新 Provider 适配
```
00 → 01 (Provider trait 详解) → 02 (理解 Provider 在 Pipeline 的位置)
→ 12 §4-5 (HTTP/gRPC 协议参考) → 完成
```

### 5.3 我要做新的 Frontend (Web / 移动端 / IDE 插件)
```
00 → 04 §12 (TrajectoryEvent 契约) → 12 (API 选择)
→ 07 (Frontend Adapter 模式) → 完成
```

### 5.4 我要从 Python / TypeScript 集成
```
00 → 12 §6 (Python) 或 §7 (TypeScript) → 04 §12 (理解事件流)
→ 12 §10 (Conformance 测试) → 完成
```

### 5.5 我是 SRE / DevOps
```
00 → 13 (Runbook) → 06 (多租户配置) → 09 (存储)
→ 11 (性能/容量) → 08 (可观测) → 07 (部署形态) → 10 (安全)
```

### 5.6 我是安全工程师
```
00 → 10 (安全模型) → 06 §4 (租户隔离) → 03 §10 (Cache 隔离)
→ 02 §4.5 (Prompt Guard) → 13 §5.10 (Isolation Breach 应急) → 08 §11 (脱敏)
```

### 5.7 我是产品 / 决策者
```
00 → 07 (4 种部署形态对比) → 11 §8 (成本结构)
→ 13 §15 (post-mortem 文化) → 完成
```

### 5.8 我刚加入团队,想 1 周内全面了解
```
Day 1: 00 + 04 (核心架构)
Day 2: 02 + 01 (请求路径)
Day 3: 03 + 05 (Cache + Tools)
Day 4: 06 + 10 (配置 + 安全)
Day 5: 07 + 12 (部署 + API)
Day 6: 08 + 09 + 11 (运维三件套)
Day 7: 13 (Runbook) + Q&A
```

---

## 6. 文档依赖关系

每篇文档依赖的其他文档（虚线为弱依赖）:

```
                          ┌────────┐
                          │   00   │
                          └────┬───┘
                               │
     ┌──────────────┬──────────┼──────────┬───────────────┐
     │              │          │          │               │
  ┌──▼──┐        ┌──▼──┐    ┌──▼──┐    ┌──▼──┐         ┌──▼──┐
  │ 01  │◄───────┤ 02  ├────┤ 04  ├────┤ 05  │         │ 12  │
  │ Pro │        │ Mid │    │ Run │    │Tool │         │ API │
  └──┬──┘        └──┬──┘    └──┬──┘    └──┬──┘         └──┬──┘
     │              │          │          │               │
     │           ┌──▼──┐       │       ┌──▼──┐            │
     │           │ 03  │◄──────┘       │     │            │
     │           │Cache│               │     │            │
     │           └─────┘               │     │            │
     │                                 │     │            │
     └─────────────────────────────────┴─────┴────────────┘
                               │
                               ▼
                  ┌──────── 横切关注点 ────────┐
                  │                            │
              ┌───▼───┐  ┌───▼───┐  ┌─────▼──┐  ┌───▼───┐
              │  06   │  │  09   │  │  10    │  │  08   │
              │Config │  │Storage│  │ Sec    │  │ MELT  │
              └───┬───┘  └───┬───┘  └────┬───┘  └───┬───┘
                  │          │           │          │
                  └──────────┼───────────┴──────────┘
                             │
                       ┌─────▼─────┐
                       │  11 + 13   │
                       │  Perf+Ops  │
                       └────────────┘
                                
                       ┌────────────┐
                       │     07     │ ← 消费 04 §12 TrajectoryEvent
                       │ Deploy/UI  │
                       └────────────┘
```

**核心读取顺序**：04 是中央枢纽,先理解它,其余文档作为它的展开。

---

## 7. 术语表

按字母顺序：

| 术语 | 含义 | 出处 |
|---|---|---|
| **Agent** | 可执行单元,接收输入产出输出,可能调 LLM/Tool | Doc 04 §4 |
| **AgentEvent** | 内部事件溯源记录 (≠ TrajectoryEvent) | Doc 04 §3.2 |
| **Audit Log** | 不可篡改的合规记录 (≠ MELT) | Doc 06 §10 |
| **BYOK** | Bring Your Own Key,用户自带 LLM API key | Doc 07 §3.1 |
| **Cache Key** | 内容寻址哈希,含 tenant + IAM + model + content | Doc 03 §3.2 |
| **Capability** | Provider 能力描述符 (支持 tool use / structured output 等) | Doc 01 §5 |
| **CLI Subprocess** | Long-lived `claude`/`gemini` CLI 进程,跨请求复用 | Doc 01 §6.2 |
| **Compensation** | 反向操作,Saga 模式下回滚副作用 | Doc 04 §6 |
| **Content Store** | 大 payload 存储,通过 ContentRef 间接引用 | Doc 04 §3.3 |
| **Critic** | 独立轮次评审 Worker 输出的 Agent | Doc 04 §2.1 |
| **Dynamic Suffix** | Prompt 中每次请求都变的部分,绝不进 cache key | Doc 03 §10.5 |
| **Effective Config** | 5 层合并后的最终配置 | Doc 06 §2 |
| **Event Sourcing** | 事件追加为唯一真相源 | Doc 04 §3.2 |
| **FFI** | Foreign Function Interface,Rust ↔ Python/Node 直接绑定 | Doc 12 §6-7 |
| **Frontend Adapter** | 消费 TrajectoryEvent 流的 UI 层 | Doc 07 §4 |
| **Idempotency Key** | 幂等键,replay/retry 时去重 | Doc 04 §7 + Doc 05 §4.3 |
| **L1/L2/L3 Cache** | 进程内 / Redis / Provider explicit 三级缓存 | Doc 03 §2 |
| **MELT** | Metrics/Events/Logs/Traces 可观测四支柱 | Doc 08 |
| **MCP** | Model Context Protocol,Anthropic 提出的 Tool 协议 | Doc 05 §5 |
| **Middleware** | Tower-style 洋葱层,处理横切关注点 | Doc 02 |
| **ModelHint** | 抽象模型选择 (Tier / Explicit / Ensemble) | Doc 01 §4.1 |
| **Orchestrator** | 不做 reasoning,只拆 task DAG 的 Agent | Doc 04 §2.1 |
| **PII** | Personally Identifiable Information | Doc 08 §11 + Doc 10 §8 |
| **Pipeline** | Middleware 组成的请求处理链 | Doc 02 |
| **Principal** | 调用方身份 (user / service account / subprocess) | Doc 10 §4 |
| **PromptBuilder** | 三段式 prompt 拼装器 | Doc 04 §11 |
| **Provider** | LLM 后端抽象 (API / CLI / 嵌入式) | Doc 01 |
| **RequestContext** | 请求级上下文,含 trace/tenant/principal/cancel/budget | Doc 02 §3.3 |
| **Routing Policy** | Provider 选择策略 (Tier / Cost / Latency / Fallback) | Doc 01 §12 |
| **SLI / SLO** | Service Level Indicator / Objective | Doc 11 §2 |
| **SaaS / Self-Hosted / Local-First / Hybrid** | 4 种部署形态 | Doc 07 §2 |
| **SecretRef** | Secret 引用 (vault path / env var name) | Doc 06 §5 |
| **Session** | 用户会话,与 trajectory 多对一 | Doc 06 §3.3 |
| **Side Effect Kind** | Pure / Isolated / Reversible / Irreversible 四档 | Doc 04 §4.4 |
| **Singleflight** | 并发同 key 请求合并 | Doc 03 §6 |
| **Skill** | 复合能力,可能含多步 LLM + Tool 编排 | Doc 05 §6 |
| **Static Prefix** | Prompt 中月级稳定的部分,L3 cache 主要复用对象 | Doc 03 §10.5 |
| **Tenant** | 硬隔离边界,核心安全单元 | Doc 06 §3 |
| **TaskBudget** | 任务级预算包络 (token/cost/时长/hops/replans) | Doc 04 §8 |
| **Tool** | 原子函数,LLM 通过 tool_use 调用 | Doc 05 §3 |
| **Trajectory** | 一次任务的执行轨迹,可分支可放弃 | Doc 04 §3.1 |
| **TrajectoryEvent** | 暴露给 Frontend 的业务事件 (≠ AgentEvent) | Doc 04 §12 |
| **TUI** | Terminal UI (ratatui-based) | Doc 07 §6 |
| **TTFT** | Time To First Token,LLM 首字延迟 | Doc 11 §2.1 |

---

## 8. 实施状态

> 本节是动态的,实施推进中持续更新。

### 8.1 当前状态 (2026-05)

```
[█░░░░░░░░░░░░░░░░░░░] 5%

完成:
- ✅ 13 篇设计文档 (00-13)
- ✅ 项目骨架 (Cargo workspace)
- ✅ 早期 prototype (interview_app, ube_core, ube_project) - 探索性

进行中:
- ⏳ 核心 trait 定义 (tars-types, tars-runtime)

未开始:
- ⬜ Provider 实现 (任意一家)
- ⬜ Pipeline 框架
- ⬜ Cache Registry
- ⬜ 完整 Agent Runtime
- ⬜ Storage 层
- ⬜ Frontend Adapters
- ⬜ FFI bindings
```

### 8.2 实施 Milestones (建议)

> 这是参考路径,实际可调

**M0: Foundation (3-4 周)**
- tars-types: 共享类型定义
- tars-config: 配置加载 + 5 层合并
- tars-storage: SQLite repository (Personal mode 优先)
- 基本 logging / tracing setup

**M1: Single Provider, Single Path (4-6 周)**
- tars-provider: 单一 OpenAI HTTP 后端
- tars-pipeline: 最小 Middleware (auth + cache lookup + retry)
- tars-cache: L1 内存 + L2 SQLite
- 端到端"Personal 模式"能跑通一次 LLM 调用

**M2: Multi-Provider + Routing (3-4 周)**
- 加 Anthropic / Gemini HTTP
- 加 routing policy + circuit breaker
- 加完整 error 分类

**M3: Agent Runtime Core (6-8 周)**
- tars-runtime: Trajectory + AgentEvent
- 单 Worker 模式跑通
- 加 critic loop

**M4: Tools + MCP (4-6 周)**
- tars-tools: Tool registry
- MCP stdio 子进程管理
- Side effect 分类强制

**M5: CLI + TUI (3-4 周)**
- 基本 `tars run` 命令
- 简单 TUI

**M6: Multi-tenant + Postgres (4-6 周)**
- Postgres schema + migration
- 租户 provisioning
- IAM engine
- 切换到 Team 模式

**M7: Web Dashboard (3-4 周)**
- HTTP API (axum)
- 内嵌 SPA

**M8: FFI Bindings (并行,各 2-4 周)**
- PyO3 binding
- napi-rs binding

**M9: Production Readiness (持续)**
- MELT 完整集成
- 安全 audit
- 性能压测
- Runbook 实操化

总计: 6-9 个月到 v1.0 可对外发布。

### 8.3 不在 v1.0 范围

- WASM binding
- Hybrid 部署模式 (云端控制面)
- 完整 SaaS 多区域部署
- AI 辅助 incident 分析
- gRPC server (HTTP 优先)

留给 v2.0 或按需。

---

## 9. 贡献指南

### 9.1 文档维护

- 任何架构变更先更新对应 doc,再写代码
- 跨 doc 引用必须用相对路径 `./XX-name.md#section-id`
- 反模式清单是宝藏——踩过坑必须加进去
- 待办与开放问题章节是 backlog,review 时清理已完成的

### 9.2 代码贡献

- 遵守对应 doc 的 trait 契约
- 新功能必须有测试 (单元 + conformance)
- 性能关键路径必须有 benchmark
- 任何安全 / 隔离相关变更需要 2 人 review

### 9.3 Doc 演进

- Doc 增减需要团队讨论
- Schema 字段增删走 §11 版本管理流程
- Reading paths (§5) 随团队成长更新

### 9.4 报告问题

- 文档不清晰 → GitHub Issue + label `docs`
- 设计有疑问 → Discussions 板块
- 安全问题 → 私下 email security@tars.dev (见 Doc 10 §15.1)

---

## 10. 历史与版本

| 版本 | 日期 | 变更 |
|---|---|---|
| 0.1 | 2026-05 | 13 篇设计文档完成,实施未开始 |

---

## 11. 致谢与参考

设计中借鉴 / 受启发的项目和论文（按字母序）：

- **Anthropic Claude Code** — CLI 长生命周期模式 + JSONL 双向协议
- **Apache Cassandra** — 多租户分区模型
- **HashiCorp Vault** — Secret namespacing 设计
- **HEARSAY-II / Blackboard Architecture** — 多 Agent 协同的经典模型
- **LangGraph** — Cyclic state machine for agents (虽然我们最终走 event sourcing)
- **OpenAI / Anthropic / Google API docs** — Tool calling / Structured Output / Caching 的具体语义
- **OpenTelemetry** — 全栈可观测标准
- **PostgreSQL pg_partman** — 时间分区自动化
- **Saga Pattern (CIDR 1987)** — 分布式事务补偿
- **Temporal / Restate / Cadence** — Durable workflow inspirations
- **Tower (Rust) / Axum** — Middleware as Layer 模式
- **vLLM / mistral.rs** — Rust LLM 推理生态

---

## 12. 联系

`<TBD>` (项目正式立项后填充)
