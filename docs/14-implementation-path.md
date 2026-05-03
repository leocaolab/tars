# 文档 14 — 实施路径 / Migration Strategy

> 范围：从当前状态 (设计完成,代码极少) 到 v1.0 可发布的具体实施规划——里程碑、依赖、风险、决策点、资源估算。
>
> 上下文：本文档是 Doc 00 §8.2 的展开,把 9 个 milestone 拆成可执行的 deliverable 与 acceptance criteria。
>
> **状态**：本文档随实施推进持续更新,完成的 milestone 标记 ✅ 并保留以备参考。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **可验证的里程碑** | 每个 milestone 有明确 Definition of Done,可被外部团队验收 |
| **小步快跑** | 每个 milestone 2-6 周可完成,避免 6 个月闭门造车 |
| **垂直切片优先** | 早期能跑通端到端 (Provider → Pipeline → Runtime),即使每层很简陋 |
| **风险前置** | 高风险 / 高不确定性的部分早做 (FFI / 子进程管理 / 流式),不要押后 |
| **可中断可恢复** | 每个 milestone 完成后系统是可工作状态,允许长假期暂停 |
| **设计先于实现** | 实施期发现设计问题 → 改 doc 再改码,不允许"代码与文档分歧" |
| **测试覆盖驱动** | 没测试的代码不算完成,即使能跑通 demo |

**反目标**：
- 不追求"一次完美架构"——v1.0 之前接受重构,v1.0 后才稳定 API
- 不追求"功能完备"——v1.0 只覆盖核心场景,长尾功能 v2 处理
- 不追求所有 Provider 第一天就支持——OpenAI compat 优先,其他渐进
- 不为了 milestone 进度跳过测试——技术债比慢一点更可怕

---

## 2. 起点与终点

### 2.1 当前状态 (Day 0)

```
✅ 完成:
- 14 篇设计文档 (Doc 00-13)
- 项目目录已建 (tars/)
- Git 仓库初始化

⚠️  存在但需处理:
- interview_app/ — Python prototype
- interview_app_en/ — English version of prototype
- ube_core/ — 早期 core 代码 (语言?)
- ube_project/ — 早期 project 抽象
- example.py — 演示脚本
- blackboard_5rounds.json — 黑板模式数据样例
- selfplay_5rounds.txt — 自对弈日志
- report_5rounds.md — 实验报告
- README.md — 项目说明 (待更新)

❌ 未开始:
- Cargo workspace
- 任何 Rust crate
- CI/CD pipeline
- 测试基础设施
- 部署脚本
```

### 2.2 v1.0 目标状态 (~6-9 个月后)

```
✅ 必须有:
- Personal mode 端到端工作 (单用户 / SQLite / 本地 web dashboard / OpenAI+Anthropic+Gemini API)
- Team mode 端到端工作 (多租户 / Postgres / Redis / OIDC / IAM)
- 核心 14 个 crate 全部实现
- HTTP API + Python (PyO3) + TypeScript (napi-rs) FFI
- CLI / TUI / Web Dashboard 三种 Frontend
- 完整 MELT 可观测性
- 完整 audit + 计费
- 1 个 reference MCP server 集成可用 (filesystem)
- 至少 3 个 built-in skill (code-review / doc-summary / security-audit)
- 完整测试套件 (unit + integration + conformance)
- 文档与代码一致

可选 (v1.0 不必但推荐):
- WASM client subset
- Hybrid 部署模式
- gRPC server
- 多于 3 个 MCP server
- 高级 Skill (declarative YAML / SubAgent)

明确不在 v1.0:
- SaaS 多区域生产部署 (需要专门基建)
- 企业 SSO / SAML 完整支持 (基本 OIDC 即可)
- AI 辅助 incident 分析
- 实时协作 dashboard
```

---

## 3. 战略选择：旧代码处置

`interview_app` / `ube_core` / `ube_project` 是早期探索,与新设计不同。三种处置方式：

### 3.1 Option A: Greenfield (全新开始)

**做法**：旧代码归档到 `archive/legacy-prototype/`,新代码从零开始。

**优势**：
- 不被旧设计束缚
- Rust 一致性强
- 学习成本明确 (按 doc 学新架构)

**劣势**：
- 损失旧代码的实验数据 / 业务理解
- 用户感知"重写不交付"
- 可能重复犯老错误

### 3.2 Option B: In-place Migration (原地迁移)

**做法**：保留 `ube_core` / `ube_project` 接口,内部逐步用 Rust 实现替换。

**优势**：
- 业务连续性
- 增量交付,每个 milestone 都有可见产出

**劣势**：
- Python ↔ Rust 边界复杂
- 旧设计可能限制新设计
- 性能优化空间小 (FFI overhead)

### 3.3 Option C: Hybrid (推荐)

**做法**：
- **新 Rust 代码** 在 `crates/` 目录,严格按 docs 设计
- **旧 Python 代码** 保留在 `interview_app/` 等,作为：
  - 业务参考 (理解需求)
  - 数据样例 (`blackboard_5rounds.json` 等用作测试 fixture)
  - Reference Skill (`interview` 作为新架构的第一个真实 skill 实现)
- **桥接**：v0.x 期间允许 CLI 层调旧 Python (subprocess);v1.0 时旧代码已被新 Skill 替换,可归档

**推荐选 C**——既不浪费已有探索,又不被束缚。

```
项目根:
├── crates/                     ← 新 Rust 代码 (按 docs 实施)
│   ├── tars-types/
│   ├── tars-runtime/
│   ├── tars-provider/
│   └── ...
├── archive/legacy-prototype/    ← 旧 Python (M3 后归档)
│   ├── interview_app/
│   ├── interview_app_en/
│   ├── ube_core/
│   └── ube_project/
├── examples/                    ← 旧 example.py 迁移 (M5)
├── fixtures/                    ← 测试数据 (旧 json 文件迁移)
│   └── blackboard-5rounds.json
└── docs/                        ← 14 篇设计文档
```

---

## 4. Crate Workspace 结构

按 Doc 12 §3.1 的拆分,`Cargo.toml` workspace：

```toml
# Cargo.toml (root)
[workspace]
resolver = "2"
members = [
    "crates/tars-types",         # M0
    "crates/tars-config",        # M0
    "crates/tars-storage",       # M0
    "crates/tars-melt",          # M0 (基础部分,M5 完整)
    "crates/tars-security",      # M1 (基础),M6 (完整)
    "crates/tars-provider",      # M1
    "crates/tars-cache",         # M1 (L1+L2),M3 (L3)
    "crates/tars-pipeline",      # M2
    "crates/tars-runtime",       # M3
    "crates/tars-tools",         # M4
    "crates/tars-frontend-cli",  # M5
    "crates/tars-frontend-tui",  # M5
    "crates/tars-frontend-web",  # M7
    "crates/tars-server",        # M6 (HTTP + later gRPC)
    "crates/tars-py",            # M8 (PyO3 binding)
    "crates/tars-node",          # M8 (napi-rs binding)
    "crates/tars-cli",           # M5 (主 CLI binary)
    "crates/tars-server-bin",    # M6 (server binary)
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "Apache-2.0"
authors = ["TARS Team"]

[workspace.dependencies]
# 共用依赖版本固定在 workspace 一级,各 crate 引用 workspace = true

# 异步 runtime
tokio = { version = "1.40", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }
async-trait = "0.1"
futures = "0.3"

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# 错误
thiserror = "1"
anyhow = "1"

# 日志/追踪
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }

# HTTP
reqwest = { version = "0.12", features = ["json", "stream"] }
axum = "0.7"
tower = "0.5"

# 数据库
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres", "sqlite", "macros", "migrate"] }

# 加密 / 安全
ring = "0.17"
argon2 = "0.5"
subtle = "2"

# 可观测
opentelemetry = "0.27"
opentelemetry-otlp = "0.27"
tracing-opentelemetry = "0.28"

# 测试
proptest = "1"
criterion = "0.5"
mockall = "0.13"
```

### 4.1 各 crate 的职责边界

| Crate | 依赖 (本项目内) | 第三方关键依赖 |
|---|---|---|
| `tars-types` | (无) | serde / thiserror |
| `tars-config` | tars-types | serde / toml / config |
| `tars-storage` | tars-types | sqlx / r2d2 (sqlite pool) |
| `tars-melt` | tars-types | tracing / opentelemetry |
| `tars-security` | tars-types, tars-config | ring / argon2 / jsonwebtoken |
| `tars-provider` | tars-types, tars-melt, tars-security | reqwest / async-trait |
| `tars-cache` | tars-types, tars-storage, tars-security | moka / redis |
| `tars-pipeline` | tars-types, tars-provider, tars-cache, tars-security, tars-melt | tower |
| `tars-runtime` | tars-types, tars-pipeline, tars-storage, tars-melt | (无新增) |
| `tars-tools` | tars-types, tars-runtime, tars-security | tokio (process) / serde_json |
| `tars-server` | tars-runtime, tars-melt | axum / tonic (later) |
| `tars-frontend-cli` | tars-runtime | clap |
| `tars-frontend-tui` | tars-runtime | ratatui / crossterm |
| `tars-frontend-web` | tars-server | rust-embed |
| `tars-py` | tars-runtime | pyo3 / pyo3-asyncio |
| `tars-node` | tars-runtime | napi / napi-derive |

---

## 5. 关键依赖选型

需要在实施前定调的"build vs buy"决策。每个都列出选项,**粗体**为推荐。

### 5.1 Async runtime
- **tokio** ✅ — 生态标准,无悬念
- async-std — 不推荐,生态萎缩

### 5.2 HTTP 客户端
- **reqwest** ✅ — 主流,reqwest::Client 成熟
- hyper 直接用 — 太底层
- ureq — 同步,不适合

### 5.3 HTTP 服务端
- **axum** ✅ — Tower 生态,符合 Pipeline 设计
- actix-web — 性能优秀但 trait 设计不如 axum 干净
- warp — 维护活跃度低

### 5.4 数据库 ORM/Query
- **sqlx** ✅ — 编译期 SQL 校验 + 多数据库 + async
- diesel — 同步 (有 async 扩展但不成熟)
- sea-orm — 抽象更高但复杂

### 5.5 Postgres 连接池
- **sqlx 自带** ✅ (PgPool)
- deadpool-postgres — 单独用复杂

### 5.6 SQLite
- **rusqlite + r2d2_sqlite** for low-level — 成熟,WAL 配置精确
- sqlx + sqlite feature — 与 Postgres 共用 query 语法,推荐 ✅

### 5.7 Redis
- **redis-rs (with tokio)** ✅ — 主流
- fred — 性能优秀但 API 重

### 5.8 LLM SDK
- **自己写 HTTP client** ✅ (基于 reqwest) — 100% 控制 + 与 docs 一致
- async-openai — 已存在,但与我们的抽象差距大,绕路反而麻烦

### 5.9 Tokenizer
- **tiktoken-rs** for OpenAI 模型 ✅
- **tokenizers (HF Rust binding)** for Anthropic / Gemini / 本地 ✅
- 其他不主流

### 5.10 ONNX Runtime
- **ort** ✅ — 主流 Rust binding
- candle — 纯 Rust 但成熟度低

### 5.11 嵌入式 LLM 推理
- **mistral.rs** ✅ — Rust 原生,active
- candle — 同上,选 mistral.rs 更成熟
- llama-cpp-2 — FFI 到 llama.cpp,不是纯 Rust 但兼容广

### 5.12 TUI
- **ratatui** ✅ — 当前最好
- cursive — 老牌但 immediate mode 不如

### 5.13 Python FFI
- **pyo3 + pyo3-asyncio** ✅ — 推荐
- UniFFI (Mozilla) — **不适合本项目**,原因:
  - 不支持 TypeScript/Node (我们另有 napi-rs 需求,UniFFI 不解决)
  - Stream 表达力弱 (callback-based,Python 端 `async for` 体验差)
  - 序列化开销在流式场景累加显著
  - 复杂 enum (带 data 的 AgentMessage) UDL 表达啰嗦
  - 真正甜区是 iOS/Android 多语言场景,v1.0 不在范围
  - **未来若加移动端 SDK,UniFFI 可与 PyO3 并存**,各自服务不同语言
- rustpython 等 — 不适合此场景

### 5.14 Node.js FFI
- **napi-rs** ✅ — Node.js 主流
- neon — 维护少
- UniFFI — 不支持

### 5.15 Configuration
- **figment** ✅ — 多层合并友好
- config-rs — 也行
- 自己写 — 不必

### 5.16 Secret Manager 客户端
- vault: **vaultrs** ✅
- AWS Secrets Manager: **aws-sdk-secretsmanager**
- GCP Secret Manager: **google-cloud-secretmanager**
- Azure: **azure-sdk-rust**
- 通过统一 trait `SecretResolver` 包装 (Doc 06 §5)

### 5.17 OpenTelemetry
- **opentelemetry-rust + opentelemetry-otlp** ✅ — 官方
- 其他无替代

### 5.18 Cache (L1 进程内)
- **moka** ✅ — concurrent + size-based eviction + TTL
- 自己 LRU — 不必

### 5.19 Schema (JSON Schema 处理)
- **schemars** for derive Schema from Rust types ✅
- **jsonschema** for runtime validation ✅
- canonical JSON: 自己写或 **cjson-rs**

### 5.20 测试
- **proptest** ✅ for property-based
- **mockall** ✅ for mocking traits
- **criterion** ✅ for benchmarks
- **wiremock** for HTTP mocking ✅

---

## 6. M0 — Foundation (3-4 周)

**目标**：搭骨架,定基础设施,确保后续 milestones 有干净起点。

### 6.1 Deliverables

```
crates/tars-types/
  ├─ TenantId / SessionId / TaskId / TraceId 等强类型 ID
  ├─ Principal / Scope / ResourceRef
  ├─ TaskSpec / TaskHandle / TaskResult / TaskBudget
  ├─ ChatRequest / ChatResponse / ChatEvent / Message / ContentBlock
  ├─ ToolDescriptor / ToolOutput / ToolEvent
  ├─ AgentEvent / TrajectoryEvent (但只是 enum,不实现存储)
  ├─ ProviderError / ToolError / 等所有 error types
  └─ #[derive(Schema)] 可生成 JSON Schema (供 Doc 12 自动化)

crates/tars-config/
  ├─ Config / TenantConfig / ProvidersConfig 等顶层 schema
  ├─ figment-based 5 层加载 (built-in / system / user / tenant / request)
  ├─ HotReloadability annotation 处理
  ├─ validate_config() 启动期校验
  └─ ConfigManager (基础,不含热加载)

crates/tars-storage/
  ├─ EventStore trait
  ├─ ContentStore trait
  ├─ KVStore trait (for cache / idempotency)
  ├─ SqliteEventStore impl + migrations
  ├─ FsContentStore impl
  └─ MemoryKVStore impl (临时,M1 升级 SQLite)

crates/tars-melt/
  ├─ TelemetryGuard (启动 + 关闭)
  ├─ tracing_subscriber JSON formatter
  ├─ SecretField<T> 类型 (脱敏强制)
  ├─ 基础 metrics registration helper
  └─ OpenTelemetry SDK 配置 (基础,M5 完整)

repo-level:
  ├─ Cargo.toml workspace 完整
  ├─ rustfmt.toml / clippy.toml
  ├─ .github/workflows/ci.yml (cargo test + clippy + fmt)
  ├─ .github/workflows/security.yml (cargo-audit + cargo-deny)
  ├─ Makefile or justfile (常用命令)
  ├─ Docker base image (供 M6 使用)
  └─ README.md 更新 (项目结构 + 怎么开发)
```

### 6.2 Definition of Done (M0)

- [ ] `cargo test --workspace` 全绿
- [ ] `cargo clippy --workspace -- -D warnings` 无警告
- [ ] `cargo deny check` 通过
- [ ] CI 在 PR 上自动跑
- [ ] 能写一个最小 Rust 程序：加载 config → 写一个 event 到 SQLite → 读回来
- [ ] tracing 输出 JSON 格式
- [ ] 已删除/归档旧 prototype 代码到 `archive/`
- [ ] README 描述如何 setup dev env

### 6.3 风险

- 类型设计反复 → **缓解**：参考 doc 严格执行,允许 v0.x 内调整
- sqlx 编译期校验需要真 DB → **缓解**：CI 启动 SQLite + Postgres test container

---

## 7. M1 — Single Provider, Single Path (4-6 周)

**目标**：一个简化的端到端 LLM 调用路径能跑通,验证核心抽象。

### 7.1 Deliverables

```
crates/tars-provider/
  ├─ LlmProvider trait (Doc 01 §3)
  ├─ ChatRequest / ChatEvent normalize (复用 tars-types)
  ├─ HttpProviderBase (reqwest 共享逻辑 + retry + timeout)
  ├─ OpenAiProvider impl (HTTP only,strict mode + tool use + streaming)
  ├─ Capabilities 描述符
  ├─ Auth resolver (基础: env var + inline)
  └─ Mock provider for testing (replay fixtures)

crates/tars-cache/
  ├─ CacheRegistry trait
  ├─ CacheKey + CacheKeyFactory (Doc 03 §3.2,完整含 IAM scopes 占位)
  ├─ L1: moka in-memory cache
  ├─ L2: SQLite-backed cache (Personal mode)
  ├─ Singleflight (Doc 03 §6,简化版)
  └─ 暂不实现 L3 (M3 加)

crates/tars-pipeline/
  ├─ LlmService trait
  ├─ Middleware trait
  ├─ Pipeline builder
  ├─ TelemetryMiddleware (基础)
  ├─ CacheLookupMiddleware
  ├─ RetryMiddleware (基础)
  └─ 暂不实现 IAM / Budget / Guard (M2-M6)

crates/tars-cli (基础):
  ├─ `tars run --prompt "hello"` 能跑通完整路径
  ├─ 输出流式文本到 stdout
  └─ 显示 token usage + cost (估算)
```

### 7.2 端到端验证脚本

```bash
# 准备
export OPENAI_API_KEY=sk-...
mkdir -p ~/.config/tars
cat > ~/.config/tars/config.toml <<EOF
mode = "personal"

[providers.openai]
type = "openai"
auth = { source = "env", var = "OPENAI_API_KEY" }
default_model = "gpt-4o-mini"

[storage]
backend = "sqlite"
path = "~/.local/share/tars/test.db"

[cache]
hasher_version = 1

[pipeline]
order = ["telemetry", "cache_lookup", "retry"]
EOF

# 跑
tars run --prompt "Write a haiku about Rust"

# 期待输出:
# - 流式吐出 haiku
# - 末尾打印: tokens: 87, cost: $0.0001

# 第二次相同 prompt
tars run --prompt "Write a haiku about Rust"
# 期待:
# - 立即返回 (cache hit)
# - 末尾打印: cached, cost saved: $0.0001
```

### 7.3 Definition of Done (M1)

- [ ] 上述端到端脚本能跑通
- [ ] Cache 命中时不调 OpenAI
- [ ] Cancel (Ctrl+C) 能干净停止
- [ ] 错误 (无效 API key / 无网) 有清晰错误信息
- [ ] Unit test coverage > 70%
- [ ] 至少一个 conformance test 通过 (provider mock 回放)

### 7.4 风险

- 流式 BoxStream 生命周期问题 → **缓解**: Doc 01 §3.1 已经设计了 Arc<Self> 模式,严格遵守
- OpenAI API 变更 → **缓解**: 锁定 API version,subscribe 公告
- SQLite WAL 配置 → **缓解**: Doc 09 §4.2 已有具体 PRAGMA

---

## 8. M2 — Multi-Provider + Routing (3-4 周)

**目标**：加 Anthropic / Gemini,完整 routing/fallback,production-grade error handling。

### 8.1 Deliverables

```
crates/tars-provider/
  + AnthropicProvider impl (HTTP + cache_control 显式)
  + GeminiProvider impl (HTTP + responseSchema)
  + LocalOpenAiCompatProvider (vLLM / llama.cpp server)
  + Tool call format normalization (三家 → 统一 ToolCall struct)
  + StructuredOutput format normalization
  + 错误 classify 完整 (Permanent / Retriable / MaybeRetriable)

crates/tars-pipeline/
  + RoutingMiddleware
  + RoutingPolicy trait + 4 internal impls (Explicit/Tier/Cost/Latency)
  + FallbackChain
  + CircuitBreakerMiddleware (基础: failure_rate based)
```

### 8.2 验证

- [ ] 同一 ChatRequest 走不同 provider 行为一致 (conformance test)
- [ ] Tier-based routing 工作
- [ ] 主 provider 故障时 fallback 正常
- [ ] Circuit breaker 在故障率高时 open

---

## 9. M3 — Agent Runtime Core (6-8 周)

**目标**：完整 Trajectory + 事件溯源 + 单 worker 模式。

### 9.1 Deliverables

```
crates/tars-runtime/
  ├─ Runtime trait (Doc 04 §12)
  ├─ Trajectory + AgentEvent + ContentRef (完整数据模型)
  ├─ Agent trait + AgentMessage protocol
  ├─ Default OrchestratorAgent (调 LLM 出 task DAG)
  ├─ Default WorkerAgent (执行 task)
  ├─ Default CriticAgent
  ├─ Trajectory store (基于 EventStore)
  ├─ ContextStore + ContextCompactor (基础: schema-aware filter)
  ├─ PromptBuilder trait + 默认实现
  ├─ Recovery: replay-from-checkpoint
  └─ Backtrack: 基础 (无副作用补偿,M4 加)

crates/tars-cache/
  + L3 Provider explicit cache (Anthropic + Gemini)
  + L3 reference counting + Janitor

crates/tars-pipeline/
  + BudgetMiddleware (Token bucket via tars-storage MemoryKVStore for now)
  + PromptGuardMiddleware (fast lane: aho-corasick only,slow lane M4)
```

### 9.2 验证

- [ ] 跑一个 multi-step task: orchestrator + 2 workers + critic
- [ ] 中断 + 重启后 task 能恢复 (event replay)
- [ ] 故意让 worker 输出 schema 不合法,critic reject + replan
- [ ] L3 cache 引用计数正确 (acquire 多次,release 后 grace period 才删)

---

## 10. M4 — Tools + MCP + Guard ML (4-6 周)

**目标**：Tool 生态可用,Skill 框架就绪,完整安全 Guard。

### 10.1 Deliverables

```
crates/tars-tools/
  ├─ Tool trait + ToolRegistry
  ├─ Tool 调用 mini-pipeline (IAM/Idempotency/SideEffect/Budget/Audit/Timeout)
  ├─ Built-in tools: fs.read_file / fs.write_file / git.fetch_pr_diff
  ├─ MCP Provider (stdio transport + long-lived session pool)
  ├─ Reference MCP server integration: mcp-filesystem
  ├─ Skill trait + Native Skill executor
  ├─ Built-in Skill: code-review (orchestrator + 2 workers + critic)
  └─ SideEffectKind 强制 (Irreversible 只在 commit phase)

crates/tars-pipeline/
  + PromptGuardMiddleware slow lane: ONNX classifier (DeBERTa)
  + ClassifierProvider trait + OnnxClassifier impl

crates/tars-runtime/
  + Saga compensation (Doc 04 §6)
  + Backtrack with compensations
```

### 10.2 验证

- [ ] `tars run --skill code-review --repo . --pr 42` 端到端工作
- [ ] MCP filesystem server 启动 + 调用成功
- [ ] 试图在 worker phase 调 commit phase 的 tool → 被拒
- [ ] Backtrack 触发 → compensations 反向执行 + audit 记录
- [ ] Prompt injection 测试集 → fast lane + ML lane 都能 catch 主要 case

---

## 11. M5 — CLI + TUI + 完整 MELT (3-4 周)

**目标**：开发者可用的 CLI + TUI,完整的可观测性。

### 11.1 Deliverables

```
crates/tars-cli/
  + 完整命令集: run / chat / dash (placeholder) / task list / task get / task cancel
  + --output json/yaml/markdown 支持
  + 配置管理: config validate / config show
  + Auto-completion (bash/zsh/fish)

crates/tars-frontend-cli/  
  + CiAdapter (CI mode 实现完整)
  + 输出格式: stdout / json / github-comment / junit-xml
  + 退出码语义 (0/1/2/124)

crates/tars-frontend-tui/
  + TuiAdapter (ratatui)
  + Trajectory tree visualization
  + 实时事件流
  + Cancel/Suspend/Resume keys

crates/tars-melt (完整):
  + 所有 Doc 08 §5 metrics 实现
  + Cardinality validator (启动期 + 运行期)
  + LabelValidator
  + Logs 严格用 SecretField
  + Trace head + tail sampling
  + AdaptiveSampler
  + OTel SDK + OTLP exporter 完整集成
```

### 11.2 验证

- [ ] 用户能用 TUI 跑 task 看进度
- [ ] CI mode 能输出 GitHub PR comment
- [ ] OTel Collector (本地 docker-compose) 能收到 metric/trace/log
- [ ] Cardinality 故意违规 → 启动失败
- [ ] Log 试图输出 raw secret → 编译期或单测 fail

---

## 12. M6 — Multi-tenant + Postgres + 安全 (4-6 周)

**目标**：从 Personal 升级到 Team mode,生产级安全。

### 12.1 Deliverables

```
crates/tars-storage/
  + PostgresEventStore impl
  + Postgres migrations (按 Doc 09 §3)
  + S3ContentStore impl (大对象)
  + RedisKVStore impl

crates/tars-config/
  + 完整热加载 (file watcher + DB polling)
  + TenantConfig 完整 + 5 层合并实现
  + ConfigSubscriber pattern

crates/tars-security/
  + AuthResolver impls: oidc / jwt / mtls / oskey
  + IamEngine (in-memory + LDAP/Postgres backends)
  + OutputSanitizer (Doc 10 §8)
  + SecretRef + SecretResolver impls (Vault / env / file)

crates/tars-runtime/
  + Tenant 完整生命周期 (provision / suspend / resume / delete)
  + 审计 (AuditEvent 完整集)
  + Per-tenant subprocess HOME 隔离

crates/tars-pipeline/
  + AuthMiddleware 完整
  + IamMiddleware (Cache 前置强约束)
  + 审计每个层的决策

crates/tars-cache/
  + 三道防线 (Doc 03 §10) 完整
  + Cross-instance invalidation (Redis pub/sub)

crates/tars-server/
  + axum HTTP server
  + REST API (Doc 12 §4 完整 endpoint)
  + SSE 流式
  + OpenAPI auto-generation (utoipa)
  + JWT auth middleware (axum layer)
  + Health/Readiness probes
  + Prometheus /metrics endpoint
```

### 12.2 验证

- [ ] 两个 tenant 共享部署,数据完全隔离
- [ ] Tenant A delete → 所有相关数据真消失,audit 保留
- [ ] OIDC login → 拿 token → 调 API
- [ ] IAM scope 缺失 → 403
- [ ] 配置文件改 → 自动 reload (无需重启)
- [ ] HTTP API 完整跑通 conformance test

---

## 13. M7 — Web Dashboard (3-4 周)

**目标**：浏览器 dashboard 可用。

### 13.1 Deliverables

```
crates/tars-frontend-web/
  + WebAdapter (axum + 内嵌 SPA)
  + REST + SSE + WebSocket
  + Auth: token / OIDC
  + 0.0.0.0 强制 auth

ui/
  + SvelteKit / SolidJS 选型 (推荐 Svelte 体积小)
  + Pages: tasks / task detail / cache stats / cost dashboard / tenant admin
  + Real-time event stream (SSE consumer)
  + Trajectory tree visualization (D3)
  + Build → static files → rust-embed
```

### 13.2 验证

- [ ] `tars dash` 启动 → 浏览器自动打开
- [ ] 看到 task 列表
- [ ] 进 task 详情,实时看 trajectory tree 增长
- [ ] 看到 token / cost 实时更新
- [ ] Admin 能看 tenant 列表 + 切换 tenant

---

## 14. M8 — FFI Bindings (并行,各 2-4 周)

**目标**：Python + Node 用户能 import / require 用 TARS。

### 14.1 Deliverables

```
crates/tars-py/
  + PyO3 binding 完整 (Doc 12 §6.2)
  + maturin build matrix
  + .pyi stubs auto-gen
  + asyncio 集成测试
  + PyPI release pipeline

crates/tars-node/
  + napi-rs binding 完整 (Doc 12 §7.2)
  + d.ts auto-gen
  + npm release pipeline (per-platform .node files)

packages/types/ (TS only)
  + 从 OpenAPI 生成 (openapi-typescript)
  + npm: @tars/types

packages/client-http/ (TS only)
  + 纯 HTTP 客户端 (浏览器 + Node)
  + npm: @tars/client-http
```

### 14.2 验证

- [ ] `pip install tars` + Python sample script 跑通
- [ ] `npm install @tars/runtime` + Node sample 跑通
- [ ] Python conformance test 全过 (Doc 12 §10)
- [ ] TypeScript conformance test 全过

---

## 15. M9 — Production Readiness (持续)

**目标**：v1.0 release 前的所有"防线"建立。

### 15.1 Deliverables

```
- 完整 backup/restore 演练通过 (Doc 13 §10)
- DR 演练 (跨 region 故障)
- 性能压测达 Doc 11 §2 SLO
- 安全 pentest (第三方)
- 完整 audit log workflow (写入 + SIEM mirror + 验证)
- Runbook 实际命令填充 (Doc 13 §5 各 playbook)
- Status page 部署
- PagerDuty 集成
- Customer-facing docs (与内部 design doc 不同,面向 SDK 用户)
- Release notes / changelog
- Migration guide (从 0.x → 1.0,即使内部用)
- Training material for on-call
- v1.0 RC → 内部 dogfood 4 周 → GA
```

### 15.2 Definition of Done (v1.0)

- [ ] 所有 13 篇 doc 的 §"测试策略" 测试 100% pass
- [ ] Conformance test (跨 binding) 100% pass
- [ ] 性能 benchmark 达 Doc 11 §2 SLO
- [ ] 安全 pentest 无 P0/P1 issue
- [ ] Backup/restore RTO 4h / RPO 1h 达成
- [ ] On-call team 完成 runbook 培训
- [ ] Customer docs 完整
- [ ] CHANGELOG 完整
- [ ] 至少 1 个内部 prod 部署稳定运行 4 周

---

## 16. Critical Path

依赖关系图 (粗箭头 = 强依赖,细箭头 = 推荐顺序):

```
M0 ──━━━━━▶ M1 ──━━━━━▶ M2 ──━━━━━▶ M3 ──━━━━━▶ M4 ──━━━━━▶ M5
                                       │             │             │
                                       │             │             ▼
                                       │             │            M7 (Web Dash)
                                       ▼             ▼
                                       └────━━━━━▶ M6 (Multi-tenant)
                                                     │
                                                     ▼
                                                    M8 (FFI bindings - 可并行)
                                                     │
                                                     ▼
                                                    M9 (Production)
```

**关键路径**: M0 → M1 → M2 → M3 → M4 → M6 → M9
- 总长: 5+5+4+7+5+5+8(M9 持续) = ~34 周
- 但 M5 / M7 / M8 可与后期 milestones 并行

**实际估算**: **6-9 个月到 v1.0 GA**,假设 1-2 名全职 Rust 工程师。

---

## 17. 风险登记册

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 核心抽象需要大改 | 中 | 高 | M0 做 vertical slice prototype 验证 |
| LLM provider API 大变 | 中 | 中 | 抽象层设计 (Doc 01) 已为此准备 |
| FFI binding 复杂度低估 | 中 | 中 | M8 提前评估 (M5 后)如果太复杂可推到 v1.1 |
| 性能不达 SLO | 低 | 高 | M5 起持续 benchmark,早发现 |
| 多租户隔离漏洞 | 低 | 极高 | M6 完成后必须 pentest + fuzz |
| Conformance test 跨 binding 不一致 | 中 | 中 | M8 早期建 conformance 框架,新 binding 必须过 |
| 团队 Rust 经验不足 | 中 | 高 | M0 期间培训 + code review 严格 |
| 第三方依赖 (sqlx/axum) 大版本升级 | 低 | 中 | 锁定版本,定期 review |
| 旧 prototype 业务理解流失 | 中 | 中 | M0 期间提取 fixture + 业务文档 |
| Provider API key 成本失控 (开发期) | 中 | 低 | 加 dev mode budget guard / mock provider 优先 |
| 团队成员变动 | 中 | 高 | 文档完整 + Pair programming + bus factor |

---

## 18. 决策点 (Pivot 触发条件)

何时停下来重新评估方向：

### 18.1 M1 验证失败
**症状**: vertical slice 跑不通,核心抽象有根本问题。
**行动**: 暂停 implement,回 doc 修改,重做 M1。**接受 1-2 周延期换长期正确**。

### 18.2 M3 性能不达 SLO
**症状**: 单 trajectory 端到端比裸调 API 慢 50%+。
**行动**: profiling + 分析瓶颈。如是架构问题,讨论是否要简化 (例如减少 middleware 层数)。

### 18.3 FFI binding 极复杂
**症状**: M8 估算需要 > 6 周 (单 binding)。
**行动**: 决策——降级为"v1.0 仅 HTTP client,FFI v1.1 再做"。

### 18.4 单租户 prototype 用户流失
**症状**: dogfood 期间 internal 用户回流到旧 Python 工具。
**行动**: 暂停 feature work,做用户访谈,可能是产品定位问题不是技术问题。

### 18.5 Provider 关系问题
**症状**: 某个 LLM provider 大幅涨价 / 废弃 API / rate limit 收紧。
**行动**: 加速本地推理 (mistral.rs / vLLM) 优先级,可能影响 routing 默认。

---

## 19. 资源规划

### 19.1 团队最小配置

| 角色 | 人数 | 职责 |
|---|---|---|
| Rust 核心工程师 | 2 | 主体实施 (M0-M6) |
| Frontend 工程师 | 1 (M5+ 加入) | TUI + Web Dashboard |
| SRE / DevOps | 1 (M6+ 加入) | 部署 / 监控 / runbook |
| 安全工程师 | 0.5 (M6 + M9) | 安全 review + pentest 协调 |
| Product + 用户调研 | 0.5 | 持续输入需求 |
| **总计** | **~5 FTE** | |

最小可行: 1 Rust + 0.5 Frontend + 0.5 SRE,但工期延长到 12-18 个月。

### 19.2 基础设施成本估算

**Dev 环境** (per 人):
- 笔记本 (M3 Max / Ryzen 7840HS+) — 已有
- LLM API quota: $200/月 (开发 + 测试)
- IDE / tools — 公司提供

**CI** (GitHub Actions / 类似):
- 矩阵构建 (Linux/macOS/Windows × Python 4 versions × Node 3 versions): ~$200/月
- Self-hosted runner (大型 build): ~$300/月

**Staging 环境**:
- 1× medium K8s cluster: ~$500/月
- Postgres + Redis: ~$200/月
- 测试 LLM API quota: $300/月

**总计 v1.0 期间**: ~$1500-2500/月 (不含人力)

### 19.3 时间分配建议

每周建议工作分配 (per 工程师):
- 60% 编码 + 测试
- 15% 设计 review (含 doc 演进)
- 10% Code review (peer)
- 10% 学习 / 探索
- 5% 沟通 / 文档

避免 100% 编码——会迅速积累技术债。

---

## 20. 旧 Prototype 处置详细计划

### 20.1 M0 期间

- [ ] Audit `interview_app` / `ube_core` / `ube_project` 代码
- [ ] 提取业务知识到 doc (作为 Doc 01-13 的补充实例)
- [ ] 提取数据样例到 `fixtures/`
- [ ] 提取 prompt 模板到 `examples/prompts/`

### 20.2 M3 完成后

- [ ] 用新 Skill 框架 (M4) 重写 `interview` 业务作为第一个 reference Skill
- [ ] 验证新实现等价于旧 Python (输出对比)

### 20.3 M5 完成后

- [ ] 旧代码移到 `archive/legacy-prototype/`
- [ ] 添加 README 说明 "这是参考代码,不再维护"
- [ ] 在 git 上打 tag `legacy-end`

### 20.4 v1.0 release 时

- [ ] `archive/` 可选保留或迁移到独立 repo
- [ ] 主 repo 100% Rust + 必要 TS / Python 包

---

## 21. 反模式清单

1. **不要为了凑 milestone 跳过测试**——技术债复利积累。
2. **不要在 M0-M3 期间引入新 feature**——专注核心,新需求记 backlog。
3. **不要在 prod 之前就追求 100% 配置外置化**——dev 阶段 hardcode 一些是 OK 的,M5 再清理。
4. **不要让 doc 与代码分歧持续**——发现不一致立即修 doc 或修码,不允许"先这样,以后再说"。
5. **不要在 M2 之前实现多 provider**——单 provider 验证抽象,过早多 provider 反而难调。
6. **不要在 M6 之前考虑 K8s**——本地 docker-compose 足够 dev/test。
7. **不要让某个 milestone 持续 > 8 周不交付**——要么拆成更小,要么坦白延期。
8. **不要为了"性能"在 M5 之前优化**——M5 才有完整 metric,优化要有数据。
9. **不要在 M8 才考虑 conformance test**——M3 起每个新功能要有跨 binding 思维。
10. **不要让 v1.0 之前 API 完全稳定**——breaking change 在 v0.x 是 OK 的,v1.0 后才 SemVer 严格。
11. **不要让团队全员只看自己的 milestone**——每周 sync 全体看进度,避免 silo。
12. **不要在 PoC 阶段过度抽象**——M0-M2 可以有"知道未来要 trait 化但暂时具体类型"的代码。
13. **不要忽略 customer feedback 即使在 v0.x**——有内部用户就有反馈,价值大。
14. **不要让 on-call 在 v1.0 之前轮值生产**——所有 v0.x 是 best-effort,不发 page。
15. **不要把"能跑"当"完成"**——DoD 包含测试 + 文档 + 可被他人验收。

---

## 22. 与上下游的契约

### 上游 (业务/产品) 承诺

- 提供清晰的 v1.0 必须功能 (本文 §2.2)
- M3 起持续提供 dogfood 反馈
- 不在 v0.x 期间承诺对外用户

### 下游 (基础设施 / 第三方) 依赖

- LLM Provider: 维持 API 稳定 (deprecation 提前 6 个月通知)
- Cargo crates: workspace 锁定版本,审慎升级
- Cloud provider: K8s / managed DB / S3 可用 (M6+)

### 团队内部

- Doc 维护者: 任何架构变更先更新 doc
- 实施者: 严格按 doc 实施,有疑问先讨论 doc
- Reviewer: PR review 时看 doc + 代码一致性

---

## 23. 待办与开放问题

- [ ] 实际团队规模决定后调整 milestones 时间
- [ ] 选择 Web Dashboard 框架 (M7 之前,Svelte / Solid)
- [ ] 决定 v1.0 是否包含 gRPC server (依赖客户需求)
- [ ] CI 选型 (GitHub Actions vs 自建 vs Buildkite)
- [ ] 文档发布方式 (GitHub Pages / Mkdocs / Docusaurus)
- [ ] License 最终决定 (Apache-2.0 / MIT / 双重 / proprietary)
- [ ] Customer support 渠道 (Discord / Slack Connect / 邮件)
- [ ] Release cadence (continuous / monthly / quarterly)
- [ ] Public roadmap 是否公开
- [ ] 商业模式 (open core / 完全开源 + service / 闭源)
- [ ] 注册商标 / 域名 (`tars.ube` / `tarsruntime.dev` / 等)
