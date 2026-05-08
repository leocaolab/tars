# 文档 07 — 部署形态与 Frontend Adapter

> 范围：定义 Runtime 的四种典型部署形态（Personal Local-First / Team Self-Hosted / SaaS / Hybrid），以及消费 Doc 04 §12 `TrajectoryEvent` 流的三类 Frontend Adapter（CI / TUI / Web Dashboard）。
>
> 上游：Doc 04 Runtime 暴露的 `Runtime::subscribe(task) -> BoxStream<TrajectoryEvent>`。
>
> 横切：本文档不引入新的 Runtime 抽象，只规范"同一个 Runtime Core 如何在不同部署 / UI 形态下复用"。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **Runtime Core 形态无关** | 同一份 Runtime 代码同时支持 4 种部署 + 3 种 Frontend，区别只在配置 + 可选编译特性 |
| **Frontend 通过 Event 解耦** | Frontend 只消费 `TrajectoryEvent` 流（Doc 04 §12），不直接访问 Runtime 内部状态 |
| **Local-First 是默认** | 个人开发者的代码绝不出本机；BYOK 把账单和数据所有权全部交给用户 |
| **零代理可选** | Local-First 模式下 Runtime 直连 Provider API，不需要任何中心服务器 |
| **Web Dashboard ≠ SaaS** | 浏览器界面可以是 `localhost:port` 的本地服务，不等于云端部署 |
| **CI 与交互可共存** | 同一个 task 在 CI 跑出退出码报错，开发者本地能用 TUI 复现并 debug |
| **Frontend 可热替换** | 不停 Runtime 的前提下，TUI 和 Web Dashboard 可同时连同一个 task 流 |

**反目标**：
- 不强制用户上云——产品定位是 dev tool，不是 SaaS 平台
- 不让 Frontend 持有业务逻辑——它只是 reducer + 渲染
- 不为了"功能完备"而堆砌 UI——CLI 优先，TUI / Web 是渐进增强
- 不在 Frontend 层做 IAM / Budget / Routing——这些是 Pipeline / Runtime 的事

---

## 2. 部署形态总览

```
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│  ① Personal Local-First + BYOK     ② Team Self-Hosted              │
│  ┌──────────────────┐              ┌─────────────────────────┐     │
│  │ User Laptop      │              │ Internal K8s / VM       │     │
│  │ ┌──────────────┐ │              │ ┌─────────────────────┐ │     │
│  │ │ Runtime      │ │              │ │ Runtime (HA)        │ │     │
│  │ │ + SQLite     │ │              │ │ + Postgres + Redis  │ │     │
│  │ │ + axum:8080  │ │              │ │ + OTel Collector    │ │     │
│  │ └──────┬───────┘ │              │ └──────────┬──────────┘ │     │
│  └─────────┼────────┘              └────────────┼────────────┘     │
│            │ direct API                         │ direct API        │
│            ▼                                    ▼                   │
│   OpenAI/Anthropic/Gemini API           OpenAI/Anthropic/Gemini    │
│   (User's own API keys)                 (Org-shared keys + vault)  │
│                                                                     │
│  ③ SaaS Multi-tenant               ④ Hybrid Control Plane          │
│  ┌──────────────────┐              ┌──────────────┐                 │
│  │ Cloud (vendor)   │              │ Cloud UI     │                 │
│  │ ┌──────────────┐ │              │ (read-only)  │                 │
│  │ │ Runtime + DB │ │◄────────────┤              │                 │
│  │ │ + Auth + UI  │ │              └──────┬───────┘                 │
│  │ └──────┬───────┘ │                     │ events only             │
│  └────────┼─────────┘              ┌──────▼───────┐                 │
│           │                        │ User Laptop  │                 │
│           │                        │ ┌──────────┐ │                 │
│           ▼                        │ │ Runtime  │ │                 │
│   LLM API + Vendor billing         │ │ + SQLite │ │                 │
│                                    │ └────┬─────┘ │                 │
│                                    └──────┼───────┘                 │
│                                           ▼                         │
│                                   LLM API (user BYOK)               │
└─────────────────────────────────────────────────────────────────────┘
```

### 2.1 四种形态的对照

| 维度 | ① Personal Local-First | ② Team Self-Hosted | ③ SaaS | ④ Hybrid |
|---|---|---|---|---|
| 数据所在地 | 用户笔记本 | 客户内网 | 厂商云 | 用户笔记本 |
| 代码内容是否离开本机 | ❌ | ✅ (仅团队内) | ✅ | ❌ |
| LLM 账单 | 用户自付 (BYOK) | 团队统一 | 厂商按量计费 | 用户自付 (BYOK) |
| 多租户 | 单租户 (即用户本人) | 多团队 | 多客户 | 单用户 + 团队 dashboard |
| Storage | SQLite | Postgres + Redis | Postgres + Redis | SQLite + 同步层 |
| HA / 扩展 | N/A | 多副本 | 多副本 + 多区域 | N/A |
| 运维负担 | 0 (用户安装) | 中 (客户 IT) | 高 (厂商 SRE) | 0 (用户) + 中 (厂商提供 dashboard) |
| Frontend | TUI / 本地 Web / CI | Web / API / CI | Web / API | TUI / 本地 Web + 远程只读 dashboard |
| 适用场景 | 个人开发、独立顾问 | 中大型团队、合规敏感 | SMB / startup 共享团队 | 平衡隐私 + 协作可见性 |

### 2.2 同一 Codebase 编译出多形态

不同形态通过 Cargo features 选择性编译：

```toml
[features]
default = ["sqlite", "tui", "local-web"]

# Storage backends
sqlite = ["dep:rusqlite", "dep:r2d2_sqlite"]
postgres = ["dep:tokio-postgres", "dep:deadpool-postgres"]

# Frontend adapters
ci = ["dep:clap"]
tui = ["dep:ratatui", "dep:crossterm"]
local-web = ["dep:axum", "dep:tower-http"]
remote-dashboard-protocol = ["dep:axum", "dep:tonic"]   # 用于 Hybrid 模式

# 形态预设
personal = ["sqlite", "tui", "local-web", "ci"]
team = ["postgres", "local-web", "ci"]
saas = ["postgres", "local-web"]
hybrid = ["sqlite", "tui", "local-web", "remote-dashboard-protocol"]
```

构建产物：
- `tars` (默认 personal feature 集，单个二进制文件 ~30MB)
- `tars-server` (team / saas feature 集)
- `tars-cli-only` (仅 CI 模式，最小二进制 ~15MB)

---

## 3. 部署形态详解

### 3.1 ① Personal Local-First + BYOK

**目标用户**：个人开发者、独立顾问、对代码隐私极度敏感的程序员。

**核心约束**：
- 用户的代码**绝不离开本机**
- 唯一与外部通信的只是 LLM Provider API（且用户自己的 API key）
- 没有用户登录系统——本机 OS 用户即"租户"
- 配置存 `~/.config/tars/`，数据存 `~/.local/share/tars/`

**架构**：

```
┌─────────────────────────────────────────┐
│  User's Laptop                          │
│  ┌─────────────────────────────────────┐│
│  │  tars binary (~30MB)                ││
│  │  ┌──────────────────────────────┐   ││
│  │  │ Runtime Core                 │   ││
│  │  │ + SQLite (events/cache/cfg)  │   ││
│  │  │ + Embedded ONNX (guard)      │   ││
│  │  │ + Optional mistral.rs (本地) │   ││
│  │  └──────────────────────────────┘   ││
│  │  ┌──────────────────────────────┐   ││
│  │  │ Frontends (按需启用)         │   ││
│  │  │  - CLI (always)              │   ││
│  │  │  - TUI (`tars chat`)         │   ││
│  │  │  - Local Web (`tars dash`)   │   ││
│  │  └──────────────────────────────┘   ││
│  └─────────────────────────────────────┘│
└──────────────────┬──────────────────────┘
                   │ direct HTTPS
                   ▼
            LLM Provider API
            (user's own keys)
```

**特殊设计点**：
- **存储用 SQLite 而非 Postgres**：单文件、零运维、性能足够（事件日志 100 events/s 完全胜任）
- **Cache L1 + L2 都在内存 / SQLite**，没有 Redis（个人模式不需要跨进程共享）
- **L3 explicit cache 仍可用**——通过用户的 BYOK 调用 Provider 创建
- **配置无租户概念**——`tenant_id` hardcode 为 `local`
- **Auth 用本地 OS 用户**——不需要 password / OAuth，依赖 OS 文件权限保护 `~/.config/tars/secrets/`
- **Pipeline 简化**：IAM / Budget / Telemetry 的复杂功能默认关闭，只保留必要的 PromptGuard / CircuitBreaker / Retry

```toml
# ~/.config/tars/config.toml (Personal mode)
mode = "personal"
storage = "sqlite"
data_dir = "~/.local/share/tars"

[providers.openai]
type = "openai"
auth = { source = "file", path = "~/.config/tars/secrets/openai.key" }

[providers.claude_cli]
type = "claude_code_cli"
binary = "claude"
auth = { source = "delegate" }

[pipeline]
order = ["telemetry", "prompt_guard", "cache_lookup", 
         "routing", "circuit_breaker", "retry"]
# 注意:个人模式不需要 IAM / Budget,但保留 PromptGuard 和 Cache

[cache]
backend = "sqlite"
hasher_version = 1

[frontends]
default_command = "chat"        # `tars` 不带参时进 TUI
web_port = 8080
web_auto_open_browser = true
```

### 3.2 ② Team Self-Hosted

**目标用户**：中大型团队、对代码隐私 / 合规敏感、希望团队共享 cache 节省成本。

**核心约束**：
- 部署在客户内网（K8s / VM / bare metal）
- 多租户（按团队 / 项目划分）
- 团队统一 LLM 账单（共享 API key 或企业账户）
- 集成客户的 IAM 系统（LDAP / OIDC / SAML）

**架构**：

```
┌────────────────────────────────────────────────────────┐
│  Customer Internal Network                             │
│  ┌──────────────┐  ┌──────────────┐  ┌─────────────┐  │
│  │ tars-server  │  │ tars-server  │  │ tars-server │  │
│  │ (HA replica) │  │ (HA replica) │  │ (HA replica)│  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬──────┘  │
│         │                 │                  │         │
│         └─────────────────┼──────────────────┘         │
│                           │                            │
│         ┌─────────────────┼──────────────────┐         │
│         ▼                 ▼                  ▼         │
│  ┌─────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ Postgres    │  │ Redis Cluster│  │ OTel + Vault │  │
│  │ (events/cfg)│  │ (cache/budget│  │              │  │
│  └─────────────┘  └──────────────┘  └──────────────┘  │
│                                                        │
│  ┌──────────────┐                                      │
│  │ vLLM / Ollama│  (本地推理,可选)                      │
│  │ + Local GPU  │                                      │
│  └──────────────┘                                      │
└──────────────────────────────────┬─────────────────────┘
                                   │
                      ┌────────────┼─────────────┐
                      ▼            ▼             ▼
                 LLM Provider API (HTTPS, Egress)
```

**关键决策**：
- **Postgres + Redis**——多副本场景必须共享存储
- **HA 部署**：多个 tars-server 实例，无状态，通过 Postgres / Redis 同步
- **Provider 凭证走 Vault**——管理员配置一次，所有租户的 Provider 调用走 Vault 拉取
- **可选本地推理**——如果团队有 GPU 节点，配置 vLLM / Ollama 作为 `ModelTier::Local` 的实现，敏感数据走本地
- **集成企业 IAM**：通过 OIDC / SAML 拉用户身份，映射到 TARS 的 Principal + Tenant

```toml
# /etc/tars/config.toml (Team mode)
mode = "team"
storage = "postgres"

[storage.postgres]
url = { source = "vault", path = "secret/data/tars/postgres" }
pool_size = 20

[storage.redis]
url = { source = "vault", path = "secret/data/tars/redis" }

[auth]
mechanism = "oidc"
issuer = "https://sso.acme.com"
client_id = "tars"
client_secret = { source = "vault", path = "secret/data/tars/oidc" }

[iam]
backend = "ldap"                            # 用 LDAP group 决策权限
ldap_url = "ldap://ldap.acme.com"
group_to_scope_mapping_file = "/etc/tars/iam_mapping.toml"

[providers.claude_api]
type = "anthropic"
auth = { source = "vault", path = "secret/data/tars/anthropic" }

[providers.local_qwen]
type = "openai_compat"
base_url = "http://vllm-node:8000/v1"
auth = { source = "none" }

# 多租户配置 (按团队)
[tenants.team_security]
display_name = "Security Team"
allowed_providers = ["claude_api", "local_qwen"]

[tenants.team_security.quotas]
daily_cost_usd_hard = 1000
max_tpm = 1000000
```

### 3.3 ③ SaaS Multi-tenant

**目标用户**：SMB / startup 不想维护基建、愿意把代码上传给厂商 SaaS。

**核心约束**：
- 厂商运维所有基建
- 厂商按量计费（marked-up LLM cost + per-seat fee）
- 厂商承担合规责任（SOC2 / ISO27001 / GDPR / HIPAA）
- 多租户硬隔离是绝对要求（Doc 06 §4 所有隔离机制）

**与 Team 模式的差别**：
- 数据存在厂商云，需要更严格的加密 (rest 加密 + per-tenant key)
- 需要计费集成（Stripe / RevenueCat 等）
- 需要厂商提供的认证系统（不能依赖客户 SSO）
- 必须有 region 选择（GDPR 数据本地化）

```toml
# 厂商部署配置
mode = "saas"
storage = "postgres"

[auth]
mechanism = "internal"                      # 厂商自有用户系统
session_backend = "redis"
mfa_required = true

[billing]
provider = "stripe"
api_key = { source = "vault", path = "secret/data/billing/stripe" }
metered_billing = true                      # 按 token 计量

[deployment]
region = "us-east-1"
data_residency_strict = true                # tenant.data_residency_required 必须匹配 region

[encryption]
rest_encryption = true
per_tenant_key = true
kms_provider = "aws"
```

### 3.4 ④ Hybrid Control Plane

**目标用户**：希望保留个人 Local-First 隐私，但又想要团队协作的 dashboard / 历史汇总。

**核心约束**：
- 数据 plane（事件 / cache / 代码）在用户本机
- 控制 plane（账号 / 配置同步 / 审计概览）在云端
- 云端**不存储任何代码片段或 LLM 响应内容**——只存元数据（"某用户某天审了多少 PR / 省了多少钱"）

**架构**：

```
┌──────────────────────────┐
│  Cloud Control Plane     │
│  ┌────────────────────┐  │
│  │ User Auth          │  │
│  │ Config Templates   │  │
│  │ Aggregated Metrics │  │
│  │ (no payload)       │  │
│  └────────┬───────────┘  │
└───────────┼──────────────┘
            │ HTTPS
            │  - Push: config templates
            │  - Pull: anonymized metrics
            │  - Never: code / LLM content
            ▼
┌──────────────────────────┐
│  User Laptop             │
│  ┌────────────────────┐  │
│  │ tars (personal)    │  │
│  │ + SyncAgent        │  │
│  │ + Local Dashboard  │  │
│  └────────────────────┘  │
└──────────────────────────┘
```

**敏感性分级**：

| 数据 | 留在本地 | 可同步到云 |
|---|---|---|
| 代码内容 | ✅ | ❌ |
| LLM 响应原文 | ✅ | ❌ |
| Prompt 内容 | ✅ | ❌ |
| 配置 (无 secret) | ✅ | ✅ |
| 用户身份 | ✅ | ✅ |
| 任务计数 / 时长 / 成本 | ✅ | ✅ |
| 错误类型分布 | ✅ | ✅ (匿名化) |
| Cache 命中率 | ✅ | ✅ |

云端 dashboard 只能看到"用户 A 这周用了 X 次 Code Review，省了 $Y，平均 latency Z"——没有任何业务内容。

---

## 4. Frontend Adapter 抽象

### 4.1 核心 trait

```rust
#[async_trait]
pub trait FrontendAdapter: Send + Sync {
    fn id(&self) -> &str;
    
    /// 启动 Adapter,绑定到 Runtime
    async fn run(
        self: Arc<Self>,
        runtime: Arc<dyn Runtime>,
        shutdown: CancellationToken,
    ) -> Result<(), AdapterError>;
}
```

每个 Frontend Adapter 内部都会：
1. 监听某种用户输入（CLI args / TUI keys / HTTP requests）
2. 调用 `runtime.submit(spec, principal)` 提交任务
3. 通过 `runtime.subscribe(task_id)` 拿到 `TrajectoryEvent` 流
4. 把事件投影为 UI 状态（reducer 模式）
5. 渲染 UI

**Frontend 之间互不感知**——Runtime 可以同时被 TUI + Web 订阅，两个 Frontend 看到同一个事件流。

### 4.2 Event Reducer

```rust
pub trait EventReducer: Send + Sync {
    type State: Default + Clone + Send;
    
    fn reduce(&self, state: Self::State, event: TrajectoryEvent) -> Self::State;
}
```

三个 Frontend 各有不同的 State 类型：

| Frontend | State 形态 | reduce 行为 |
|---|---|---|
| CI | `Result<Report, FailureSummary>` | 累积 artifact，最后产出报告 |
| TUI | `TuiViewModel { trajectory_tree, current_focus, log_window }` | 增量更新视图 |
| Web | `Vec<UiPatch>` (JSON Patch RFC 6902) | 增量推送给前端 |

---

## 5. CI Mode Adapter

### 5.1 调用形态

```bash
# 在 GitHub Actions / GitLab CI / Jenkins
tars run code-review \
  --repo . \
  --pr ${PR_NUMBER} \
  --output github-comment \
  --fail-on critical
echo "Exit code: $?"   # 0 = pass, 非0 = fail
```

### 5.2 实现

```rust
pub struct CiAdapter {
    config: CiConfig,
}

pub struct CiConfig {
    pub task_template: String,                    // skill id 或 agent blueprint
    pub output_format: CiOutput,
    pub fail_on: FailureThreshold,
    pub max_wait: Duration,
}

pub enum CiOutput {
    Stdout,
    JsonFile(PathBuf),
    GithubComment { repo: String, pr: u32 },
    GitlabComment { project: String, mr: u32 },
    JunitXml(PathBuf),
}

#[async_trait]
impl FrontendAdapter for CiAdapter {
    async fn run(
        self: Arc<Self>,
        runtime: Arc<dyn Runtime>,
        shutdown: CancellationToken,
    ) -> Result<(), AdapterError> {
        let spec = self.build_task_spec()?;
        let task = runtime.submit(spec, self.ci_principal()).await?;
        
        let mut events = runtime.subscribe(task.task_id);
        let mut state = CiState::default();
        let reducer = CiReducer::new();
        
        // 流式收事件,但只在终结时输出 (CI 不需要中途输出)
        loop {
            tokio::select! {
                Some(event) = events.next() => {
                    state = reducer.reduce(state, event);
                    
                    // 致命错误立即退出 (省 CI 时间)
                    if state.has_unrecoverable_error() {
                        break;
                    }
                }
                _ = shutdown.cancelled() => {
                    runtime.cancel(task.task_id).await?;
                    break;
                }
                _ = tokio::time::sleep(self.config.max_wait) => {
                    runtime.cancel(task.task_id).await?;
                    return Err(AdapterError::Timeout);
                }
            }
            
            if state.is_terminal() { break; }
        }
        
        // 输出
        match &self.config.output_format {
            CiOutput::Stdout => self.print_to_stdout(&state),
            CiOutput::JsonFile(path) => self.write_json(path, &state).await?,
            CiOutput::GithubComment { repo, pr } => {
                self.post_github_comment(repo, *pr, &state).await?
            },
            CiOutput::GitlabComment { project, mr } => {
                self.post_gitlab_comment(project, *mr, &state).await?
            },
            CiOutput::JunitXml(path) => self.write_junit(path, &state).await?,
        }
        
        // 退出码
        let exit_code = state.compute_exit_code(&self.config.fail_on);
        std::process::exit(exit_code);
    }
}
```

**关键决策**：
- **CI 不需要中间输出**——只在终结时打印结果（避免 CI 日志被刷屏）
- **超时强制 cancel**——CI 不能挂着不动
- **退出码语义**：0 = 全过 / 1 = 业务失败（rule 触发）/ 2 = 系统错误（runtime panic）/ 124 = 超时
- **post comment 是不可逆动作**——必须在 commit phase（Doc 04 §4.4）
- **支持多种输出**：Stdout 用于本地 debug；GitHub/GitLab Comment 用于 PR；JUnit XML 用于 Jenkins

### 5.3 输出格式示例

GitHub Comment (Markdown)：

```markdown
## TARS Code Review 🤖

**Verdict**: ⚠️ 2 Critical, 5 Warning

### Critical Issues
1. **`src/auth.rs:42`** - SQL injection in login query
2. **`src/api/users.rs:101`** - PII logged in plaintext

### Warnings
[折叠列表]

---
*Reviewed in 47s • Used 12,847 tokens • Cost $0.18 • Cache hit rate 73%*
*Generated by [TARS](https://...) · [View full report](https://localhost:8080/tasks/abc123)*
```

JUnit XML（用于 Jenkins / Azure DevOps）：

```xml
<testsuite name="tars.code-review" tests="7" failures="2">
  <testcase classname="security" name="src/auth.rs:42">
    <failure type="critical">SQL injection in login query</failure>
  </testcase>
  ...
</testsuite>
```

---

## 6. TUI Mode Adapter

### 6.1 调用形态

```bash
tars chat                    # 进入交互式 TUI
tars chat --resume abc123    # 恢复一个之前的 task
tars chat --skill code-review --repo . --pr 42  # 直接跑 skill 但用 TUI 看进度
```

### 6.2 布局

```
┌─ TARS ─────────────────────────────────────────────────────────┐
│  Task: code-review#abc123                  [F1 Help] [F10 Quit]│
├──────────────────────────┬─────────────────────────────────────┤
│  Trajectory Tree         │  Active Step                        │
│                          │                                     │
│  ▼ Root: code-review     │  Agent: SecurityWorker              │
│    ├─ ✓ Planner          │  Model: claude-opus-4-7             │
│    ├─ ▶ SecurityWorker  │  Status: streaming...               │
│    │   └─ ⏳ scanning    │                                     │
│    ├─ ⏳ PerfWorker      │  Output (live):                     │
│    └─ ⏸ Critic           │  ┌───────────────────────────────┐  │
│                          │  │ Found potential SQL injection │  │
│                          │  │ in src/auth.rs line 42:       │  │
│                          │  │ ```rust                       │  │
│                          │  │ let q = format!("SELECT...... │  │
│                          │  └───────────────────────────────┘  │
├──────────────────────────┴─────────────────────────────────────┤
│  Tokens: 8,432 / 50,000   Cost: $0.12   Cache: 64% hit         │
│  > _                                              [Enter] Send  │
└────────────────────────────────────────────────────────────────┘
```

### 6.3 实现 (基于 ratatui)

```rust
pub struct TuiAdapter {
    config: TuiConfig,
}

#[async_trait]
impl FrontendAdapter for TuiAdapter {
    async fn run(
        self: Arc<Self>,
        runtime: Arc<dyn Runtime>,
        shutdown: CancellationToken,
    ) -> Result<(), AdapterError> {
        let mut terminal = ratatui::init()?;
        let app = Arc::new(Mutex::new(TuiApp::new(runtime)));
        
        // Three concurrent loops:
        // 1. Input loop (crossterm key events)
        // 2. Event loop (Runtime TrajectoryEvent stream)
        // 3. Render loop (60 FPS)
        
        let input_task = tokio::spawn(input_loop(app.clone(), shutdown.clone()));
        let event_task = tokio::spawn(event_loop(app.clone(), shutdown.clone()));
        let render_task = tokio::spawn(render_loop(app.clone(), terminal, shutdown.clone()));
        
        let _ = tokio::try_join!(input_task, event_task, render_task);
        ratatui::restore();
        Ok(())
    }
}

async fn event_loop(
    app: Arc<Mutex<TuiApp>>,
    shutdown: CancellationToken,
) -> Result<(), AdapterError> {
    loop {
        let active_tasks: Vec<_> = app.lock().await.active_tasks();
        for task in active_tasks {
            let mut events = app.lock().await.runtime.subscribe(task);
            while let Some(event) = tokio::select! {
                e = events.next() => e,
                _ = shutdown.cancelled() => None,
            } {
                let mut app = app.lock().await;
                app.apply_event(event);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
```

### 6.4 Cancel 协同

按 `Ctrl+C` / `q` / `Esc` 时 TUI 必须把 cancel 透传到 Runtime：

```rust
impl TuiApp {
    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if let Some(task) = self.current_task() {
                    let runtime = self.runtime.clone();
                    tokio::spawn(async move {
                        runtime.cancel(task).await.ok();
                    });
                }
                self.shutdown.cancel();
            }
            // ... other keys
        }
    }
}
```

Runtime 收到 cancel → 通过 Doc 02 §5 的 cancel 链传递到 Provider → CLI subprocess interrupt 信号（Doc 01 §6.2.1）→ 干净停止。整条链路通透。

### 6.5 多 task 并行

TUI 支持同时跑多个 task（横向 tab 切换）：

- `Tab`：切换 active task
- `Ctrl+N`：新 task
- `Ctrl+W`：关闭当前 tab（同时 cancel）

每个 task 一个独立的 `subscribe` 流，并行 reduce 到各自的 ViewModel。

---

## 7. Web Dashboard Adapter

### 7.1 调用形态

```bash
tars dash                                 # 启动本地 web,自动开浏览器
tars dash --port 9090 --no-open           # 指定端口,不自动开
tars dash --bind 0.0.0.0 --auth-token X   # 团队共享场景
```

默认绑定 `127.0.0.1:8080`，仅本机可访问。如果 `--bind 0.0.0.0` 必须配 `--auth-token`，否则启动失败（防止配置错误暴露）。

### 7.2 架构

```
Browser
   │  HTTP / WebSocket
   ▼
┌──────────────────┐
│ axum Server      │
│  ┌────────────┐  │
│  │ REST API   │  │  task submit / query / cancel
│  ├────────────┤  │
│  │ SSE / WS   │  │  TrajectoryEvent stream → JSON Patch
│  ├────────────┤  │
│  │ Static SPA │  │  embedded in binary (rust-embed)
│  └────────────┘  │
└────────┬─────────┘
         │ direct call
         ▼
   Runtime Core
```

### 7.3 实现

```rust
pub struct WebAdapter {
    config: WebConfig,
}

pub struct WebConfig {
    pub bind: SocketAddr,
    pub auth: WebAuth,
    pub auto_open_browser: bool,
}

pub enum WebAuth {
    /// 仅 localhost 绑定时允许
    None,
    /// 共享 token (CLI 启动时生成,打印一次)
    Token(SecretString),
    /// OIDC (团队部署)
    Oidc { issuer: Url, client_id: String },
}

#[async_trait]
impl FrontendAdapter for WebAdapter {
    async fn run(
        self: Arc<Self>,
        runtime: Arc<dyn Runtime>,
        shutdown: CancellationToken,
    ) -> Result<(), AdapterError> {
        // 安全校验:0.0.0.0 必须配 auth
        if self.config.bind.ip().is_unspecified() 
            && matches!(self.config.auth, WebAuth::None) 
        {
            return Err(AdapterError::ConfigError(
                "Binding to 0.0.0.0 requires auth token or OIDC".into()
            ));
        }
        
        let app = Router::new()
            .route("/api/tasks", post(submit_task).get(list_tasks))
            .route("/api/tasks/:id", get(get_task).delete(cancel_task))
            .route("/api/tasks/:id/events", get(stream_events))   // SSE
            .nest_service("/", ServeEmbed::<Spa>::new())          // 内嵌 SPA
            .layer(AuthLayer::from(self.config.auth.clone()))
            .with_state(AppState { runtime: runtime.clone() });
        
        if self.config.auto_open_browser {
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                opener::open(format!("http://{}", self.config.bind)).ok();
            });
        }
        
        axum::serve(TcpListener::bind(self.config.bind).await?, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await?;
        
        Ok(())
    }
}

async fn stream_events(
    State(state): State<AppState>,
    Path(task_id): Path<TaskId>,
) -> impl IntoResponse {
    let stream = state.runtime.subscribe(task_id)
        .map(|event| {
            // 转 JSON Patch 增量推送
            let patch = event_to_json_patch(&event);
            Ok::<_, Infallible>(Event::default().json_data(&patch).unwrap())
        });
    
    Sse::new(stream).keep_alive(KeepAlive::default())
}
```

### 7.4 SPA 选型

- **框架**：SvelteKit / SolidJS / 原生 Web Components（轻量优先，避免 React 体积）
- **打包**：vite build → static files → `rust-embed!` 内嵌进二进制
- **状态管理**：JSON Patch 增量更新（每个 event → 一组 patch）
- **可视化**：trajectory tree 用 D3 / Cytoscape；流式输出用 markdown-it
- **不依赖外部 CDN**——所有 JS / CSS / font 内嵌

### 7.5 Hybrid Cloud Dashboard

Hybrid 模式下，云端的 Dashboard 不是消费本地 Runtime 的 TrajectoryEvent，而是消费 SyncAgent 推送的**匿名化 metrics**：

```rust
pub struct CloudSyncAgent {
    cloud_endpoint: Url,
    auth_token: SecretRef,
    runtime: Arc<dyn Runtime>,
}

impl CloudSyncAgent {
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = tick.tick() => self.push_aggregated_metrics().await,
                _ = shutdown.cancelled() => return,
            }
        }
    }
    
    async fn push_aggregated_metrics(&self) {
        let metrics = self.runtime.aggregate_metrics(
            AggregationWindow::LastHour,
            AggregationLevel::TenantOnly,        // 不含任何 payload / content
        ).await.unwrap();
        
        let payload = AnonymizedMetricsPayload {
            user_id: self.user_id.clone(),
            window: metrics.window,
            task_count: metrics.task_count,
            total_tokens: metrics.total_tokens,
            total_cost_usd: metrics.total_cost_usd,
            cache_hit_rate: metrics.cache_hit_rate,
            error_distribution: metrics.error_kinds,   // 只有错误类型,无 message
            // 绝不上传:任何 prompt / response / code 内容
        };
        
        let _ = http_post(&self.cloud_endpoint, &payload).await;
    }
}
```

云端 Dashboard 看到的就是这些匿名 metrics + 用户身份，渲染成"团队这周节省 $X / 高频用户列表 / 错误率趋势"——**完全不需要业务内容**。

---

## 8. 三种 Adapter 共享的 Reducer 模式

虽然三个 Adapter 的 State 形态不同，**reducer 的核心逻辑高度一致**——都在追踪 trajectory tree 状态。提取共享部分：

```rust
pub struct CommonTrajectoryReducer;

#[derive(Default, Clone)]
pub struct TrajectoryViewModel {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub trajectory_nodes: Vec<TrajectoryNode>,
    pub active_agents: HashMap<AgentId, AgentRunningState>,
    pub artifacts: Vec<ArtifactRef>,
    pub usage: Usage,
    pub cost: f64,
    pub cache_stats: CacheStats,
    pub errors: Vec<ErrorEntry>,
}

impl CommonTrajectoryReducer {
    pub fn reduce(
        &self, 
        mut state: TrajectoryViewModel, 
        event: TrajectoryEvent,
    ) -> TrajectoryViewModel {
        match event {
            TrajectoryEvent::TaskStarted { task, spec } => {
                state.task_id = task;
                state.status = TaskStatus::Running;
            }
            TrajectoryEvent::AgentInvoked { agent, .. } => {
                state.active_agents.insert(agent, AgentRunningState::Started);
            }
            TrajectoryEvent::AgentCompleted { agent, output_summary, usage } => {
                state.active_agents.remove(&agent);
                state.usage = state.usage.merge(usage);
                // ... 
            }
            TrajectoryEvent::PartialArtifact { artifact } => {
                state.artifacts.push(artifact.into());
            }
            TrajectoryEvent::Completed { final_artifact, total_usage, total_cost } => {
                state.status = TaskStatus::Completed;
                state.cost = total_cost;
            }
            // ... 其他事件
        }
        state
    }
}
```

每个 Adapter 在 CommonTrajectoryReducer 之上叠加自己的视图层 reducer：

- **CI**：把 ViewModel 转 Markdown / JUnit XML
- **TUI**：把 ViewModel 渲染成 ratatui Widget tree
- **Web**：把 ViewModel diff 成 JSON Patch 推给前端

---

## 9. Packaging 与分发

### 9.1 分发形态

| 形态 | 包格式 | 目标用户 |
|---|---|---|
| Personal | `tars` 单文件二进制 | 个人开发者 |
| Personal | `.deb` / `.rpm` / Homebrew | 自动 PATH 集成 |
| Personal | `cargo install tars` | Rust 用户 |
| Team | Docker image `tars/server:1.x` | K8s / docker compose |
| Team | Helm chart | K8s 部署 |
| Team | Terraform module | IaC |
| SaaS | (内部) Helm chart | 厂商 SRE |

### 9.2 二进制构建优化

```toml
# Cargo.toml - release profile
[profile.release]
opt-level = 3
lto = "fat"                                # 全局优化
codegen-units = 1                          # 牺牲编译速度换二进制大小
strip = "symbols"                          # 剥离调试符号
panic = "abort"                            # 不生成 unwind 表

[profile.release-small]
inherits = "release"
opt-level = "z"                            # 优先小体积
```

`cargo build --profile release` 后 `tars` ~30MB（含内嵌 SPA + ONNX classifier）。`release-small` ~22MB（损失 5-10% 性能）。

### 9.3 平台覆盖

GitHub Actions matrix 构建：

| Target | 平台 |
|---|---|
| `x86_64-unknown-linux-gnu` | 主流 Linux |
| `x86_64-unknown-linux-musl` | Alpine / 静态链接 |
| `aarch64-unknown-linux-gnu` | ARM Linux (Graviton) |
| `x86_64-apple-darwin` | Intel Mac |
| `aarch64-apple-darwin` | Apple Silicon |
| `x86_64-pc-windows-msvc` | Windows |

签名 + notarize（macOS）。

---

## 10. Auto-Update 机制

### 10.1 Personal 模式

```bash
tars update           # 显式触发
tars update --check   # 仅检查新版本,不安装
```

策略：
- 检查 GitHub Release API
- 下载新二进制到 temp 目录
- 校验签名（sigstore / minisign）
- 原子 rename 替换当前二进制
- 重启进程
- **不自动更新**——必须用户显式确认（避免破坏正在跑的 task）

### 10.2 Team 模式

不做自动更新——团队 IT 通过自己的 deploy pipeline 管控版本。提供：
- Docker image 多 tag (`:1.2.3` / `:1.2` / `:latest`)
- Helm chart appVersion 同步
- Migration tool: `tars-server migrate --from 1.2 --to 1.3`（schema 变更时）

### 10.3 兼容性保证

- **Config schema**：major version 内向后兼容；major bump 提供 migration tool
- **API**：HTTP REST 通过 `/api/v1` 路径版本化，`/api/v2` 新增不破坏 v1
- **Storage**：Postgres / SQLite migration 都用 `refinery` / `sqlx::migrate!`，启动时自动 apply

---

## 11. 隐私与安全 (per mode)

| 关注点 | Personal | Team | SaaS | Hybrid |
|---|---|---|---|---|
| 代码内容存放 | 本机 SQLite | 团队 Postgres | 厂商 Postgres (加密) | 本机 SQLite |
| LLM 响应存放 | 本机 SQLite | 团队 Postgres | 厂商 Postgres (加密) | 本机 SQLite |
| API key 存放 | OS keychain / file (600) | Vault | Vault + KMS | OS keychain |
| 网络出站 | LLM API only | LLM API + OTel | LLM API + 内部 | LLM API + 匿名 metrics |
| 用户身份 | OS user | OIDC / LDAP | 厂商 IdP | OS user (本地) + Cloud IdP (面板) |
| 审计日志 | SQLite (本地保留) | Postgres + SIEM | Postgres + SIEM + WORM | SQLite + Cloud (匿名) |
| 数据残留 (退出后) | SQLite 文件保留 | DB 持久化 | 7 年保留 (合规) | SQLite + 云端 metrics 90 天 |

**Personal 模式的额外保护**：
- secret 文件权限强制 0600
- SQLite 文件权限强制 0600
- subprocess (CLI / MCP) 继承严格 umask
- 默认禁用任何外联 telemetry（用户必须显式 opt-in）

---

## 12. 配置示例汇总

```toml
# 完整的 personal 模式配置
mode = "personal"
data_dir = "~/.local/share/tars"
log_level = "info"

[providers.anthropic]
type = "anthropic"
auth = { source = "file", path = "~/.config/tars/secrets/anthropic" }

[providers.openai]
type = "openai"
auth = { source = "env", var = "OPENAI_API_KEY" }

[providers.claude_cli]
type = "claude_code_cli"
binary = "claude"
auth = { source = "delegate" }

[storage]
backend = "sqlite"
path = "${data_dir}/tars.db"

[cache]
backend = "sqlite"
hasher_version = 1

[pipeline]
order = ["telemetry", "prompt_guard", "cache_lookup", 
         "routing", "circuit_breaker", "retry"]

[middleware.prompt_guard]
slow_lane = "embedded_onnx"
slow_lane_model = "${data_dir}/models/deberta-injection-int8.onnx"

[frontends.tui]
enabled = true
default_skill = "code-review"

[frontends.local_web]
enabled = true
bind = "127.0.0.1:8080"
auto_open_browser = true

[frontends.ci]
enabled = true
default_output = "stdout"

# 完全不联系任何外部 telemetry
[observability]
otel_enabled = false
crash_reporting = false
usage_analytics = false
```

---

## 13. 测试策略

### 13.1 Adapter 单元测试

```rust
#[tokio::test]
async fn ci_adapter_returns_correct_exit_code_on_critical() {
    let runtime = MockRuntime::with_events(vec![
        TrajectoryEvent::Completed {
            final_artifact: Artifact::review_with_critical_findings(2),
            ..
        }
    ]);
    
    let adapter = CiAdapter::new(CiConfig {
        fail_on: FailureThreshold::Critical,
        output_format: CiOutput::Stdout,
        ..
    });
    
    // CiAdapter 内部会 process::exit,这里用 catch_exit 钩子
    let exit_code = catch_exit(|| async {
        Arc::new(adapter).run(runtime, CancellationToken::new()).await.unwrap();
    }).await;
    
    assert_eq!(exit_code, 1);
}
```

### 13.2 多 Adapter 并发测试

```rust
#[tokio::test]
async fn tui_and_web_can_subscribe_same_task() {
    let runtime = test_runtime();
    let task = runtime.submit(spec(), principal()).await.unwrap();
    
    let tui_events: Vec<_> = runtime.subscribe(task).take(5).collect().await;
    let web_events: Vec<_> = runtime.subscribe(task).take(5).collect().await;
    
    // 两个订阅都应收到完整事件 (Runtime 使用 broadcast)
    assert_eq!(tui_events.len(), web_events.len());
}
```

### 13.3 Web Adapter 安全测试

```rust
#[tokio::test]
async fn web_adapter_rejects_0000_without_auth() {
    let config = WebConfig {
        bind: "0.0.0.0:8080".parse().unwrap(),
        auth: WebAuth::None,
        ..
    };
    let adapter = WebAdapter::new(config);
    
    let result = Arc::new(adapter).run(test_runtime(), CancellationToken::new()).await;
    assert!(matches!(result, Err(AdapterError::ConfigError(_))));
}
```

### 13.4 Hybrid SyncAgent 隐私测试

```rust
#[tokio::test]
async fn sync_agent_never_sends_payload_content() {
    let captured_payloads = Arc::new(Mutex::new(Vec::new()));
    let mock_cloud = MockHttpServer::capturing(captured_payloads.clone()).await;
    
    let agent = CloudSyncAgent::new(mock_cloud.url(), test_runtime_with_real_data());
    Arc::new(agent).push_aggregated_metrics().await;
    
    let payloads = captured_payloads.lock().await;
    for payload in payloads.iter() {
        // 验证 payload 不含任何 prompt / response / code
        assert!(!payload.contains_str("source code"));
        assert!(!payload.contains_str("prompt"));
        assert!(!payload.contains_str("response"));
    }
}
```

---

## 14. 反模式清单

1. **不要让 Frontend Adapter 直接读 AgentEvent**——只通过 TrajectoryEvent (Doc 04 §12)，保护内部 schema。
2. **不要在 Frontend 里做业务逻辑**——所有决策在 Runtime / Pipeline / Agent，Adapter 只是 reducer + 渲染。
3. **不要让 Web Dashboard 默认绑定 0.0.0.0**——必须配 token / OIDC，配置错时启动失败。
4. **不要把 SaaS 与 Local-First 混入同一份配置文件**——通过 `mode = ...` 顶层切换，避免误配置导致泄漏。
5. **不要在 Hybrid 同步层泄漏 payload**——SyncAgent 只能读 aggregated metrics 接口，不允许 raw event 流。
6. **不要让 CI 模式输出中间进度**——保持终端日志干净，只在终结时 print。
7. **不要在 TUI 里 block 主线程**——所有 IO 走 tokio::spawn + channel。
8. **不要让 Auto-Update 不经用户确认**——可能正在跑长任务。
9. **不要把 SPA 资源外链 CDN**——离线环境会挂，且引入第三方追踪。
10. **不要假设所有 Frontend 共享同一时序**——TUI 和 Web 看到的事件顺序可能略有差异（broadcast 调度），UI 必须能处理乱序。
11. **不要在 Personal 模式默认开启外联 telemetry**——必须显式 opt-in，并在配置文件里 banner 提示。
12. **不要让 Adapter 持有 Runtime 内部数据结构**——只通过 trait 接口（Runtime / EventReducer）。
13. **不要让 Hybrid 模式的本地 Dashboard 和云端 Dashboard 混用 auth**——本地用 OS user，云端用 OIDC，互不复用。
14. **不要在 Frontend 错误处理里吞 Runtime 的错误**——Frontend 看到的错误必须暴露给用户，不能"装作没事"。
15. **不要为了"功能完整"在 CI 模式做交互**——CI 永远 non-interactive，需要交互的步骤改用 TUI。

---

## 15. 与上下游的契约

### 上游 (用户) 入口

- **CLI**：`tars [subcommand] [args]`，每个 subcommand 对应一个 Adapter（`run` → CI / `chat` → TUI / `dash` → Web）
- **HTTP API** (Web Adapter 提供)：REST + SSE，文档在 OpenAPI spec
- **gRPC** (可选，Hybrid 同步层用)：protobuf 定义在 `proto/sync.proto`

### 下游 (Runtime) 契约

- 通过 `Runtime::submit / subscribe / cancel / suspend / resume` 调用
- 不直接访问 AgentEvent / Trajectory / 内部数据结构
- TrajectoryEvent 流的消费速度可能不同——Runtime 用 `broadcast::channel`，慢消费者可能 lag，必须有降级处理（drop 老事件 + 提示用户）

### 跨 Frontend 的事件一致性

- 同一 task 被多个 Adapter subscribe 时，Runtime 保证每个订阅都能收到完整事件流（不丢失）
- 但**不保证跨订阅的严格时序**——broadcast 调度可能让 Adapter A 比 B 早 / 晚一个事件
- 终结事件（Completed / Failed）一定是流的最后一个，所有 Adapter 都必须最终收到

---

## 16. 待办与开放问题

- [ ] TUI 里 trajectory tree 大于屏幕高度的滚动 / collapse 策略
- [ ] Web Dashboard 的 SPA 框架最终选型（Svelte vs Solid vs vanilla）
- [ ] Hybrid 模式云端控制面的实际形态（独立产品 vs 嵌入主产品）
- [ ] CI 模式的输出格式是否要支持 SARIF（GitHub Code Scanning 标准）
- [ ] Auto-update 的回滚机制（新版本崩溃时如何降回上一版）
- [ ] BYOK 用户的 API key 校验（启动时 ping provider 还是延迟到首次调用）
- [ ] Frontend Adapter 之间共享 session 的可能（同一 session_id 在 TUI 启动 + 关 TUI 后 Web 接管）
- [ ] 移动端 / 远程访问 (SSH 转发 TUI vs 远程 Web)
- [ ] CI 模式的并发度控制（同一 PR 多次触发是否复用之前的结果）
- [ ] Personal 模式的多机同步 (用户在 laptop 跑了一半,在 desktop 想接续)
