# 文档 06 — 配置与多租户管理

> 范围：定义配置的层级、来源、优先级、热加载机制；多租户数据模型与隔离保证；Secret 管理；租户生命周期；配额与计费。
>
> 横切：本文档不引入新的运行时组件，只规范前面 Doc 01-05 已经提到的"配置 / 租户"维度的统一形态。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **配置即代码** | 全部配置用版本化的文本文件表达,Git 是唯一事实源 (DB 只是热加载缓存) |
| **租户硬隔离** | tenant_id 是安全边界——隔离 IAM / Cache / Budget / Auth / MCP 子进程 / 事件日志 |
| **分层覆盖** | 默认值 → 系统 → 用户 → 租户 → 请求,深层可覆盖浅层,但有些层禁止覆盖 |
| **Secret 永不入文件** | 所有 secret 通过引用拉取,运行时解析,绝不持久化明文 |
| **可热加载者明确标注** | 不是所有配置都能热加载;能与不能必须在 schema 上显式标记 |
| **校验前置** | 启动期 + 热加载时全量校验;校验失败拒绝启动/拒绝应用,不允许部分加载 |
| **租户生命周期完整** | Provision / Suspend / Resume / Delete 全流程,Delete 必须级联清理 |
| **配额可观测** | 每个租户的 token / cost / cache 用量可实时查询,可导出计费报表 |

**反目标**：
- 不在配置里硬编码 secret——开发期都不允许（即使是 dev 环境，用 secret manager 的 dev profile）
- 不让租户配置覆盖安全约束（IAM 顺序、cache hasher_version 等）
- 不做"动态租户发现"——所有租户在 provisioning 流程中显式注册
- 不让配置 schema 演进破坏老租户——必须有 migration 路径

---

## 2. 配置层级与优先级

```
                        ┌──────────────────┐
                        │ Per-Request      │  ← 极少用,主要测试
                        │ overrides        │
                        └────────┬─────────┘
                                 │ 覆盖
                        ┌──────────────────┐
                        │ Tenant overrides │  ← Postgres,可热加载
                        │ (DB-backed)      │
                        └────────┬─────────┘
                                 │ 覆盖
                        ┌──────────────────┐
                        │ User config      │  ← ~/.config/tars/*.toml
                        │ (file-backed)    │     (本地部署 / dev 环境)
                        └────────┬─────────┘
                                 │ 覆盖
                        ┌──────────────────┐
                        │ System config    │  ← /etc/tars/*.toml
                        │ (file-backed)    │     (生产部署默认)
                        └────────┬─────────┘
                                 │ 覆盖
                        ┌──────────────────┐
                        │ Built-in config  │  ← 二进制内置 (ship with code)
                        │ (embedded)       │
                        └────────┬─────────┘
                                 │ 覆盖
                        ┌──────────────────┐
                        │ Compiled         │  ← Default impl 的 const
                        │ defaults         │
                        └──────────────────┘
```

### 2.1 优先级规则

- **深层覆盖浅层**——Per-Request > Tenant > User > System > Built-in > Compiled
- **数组 / Map 是合并而非替换**（除非显式标记 `replace = true`）
- **存在性 > 默认值**——配置项写出来即使是空值,也算"显式设置"
- **所有合并在启动/热加载时完成**——运行时拿到的是已经 collapse 好的 effective config,无运行时分支判断

### 2.2 禁止覆盖的层

某些层必须在系统级锁定,租户/用户不能覆盖:

| 配置项 | 锁定层 | 理由 |
|---|---|---|
| Pipeline 层序约束 (Doc 02 §7) | System | 安全约束,IAM 必须先于 Cache |
| Cache hasher_version (Doc 03 §11) | System | 改了会导致全租户 cache 失效 |
| Provider 列表本身 | System | 租户只能选择启用,不能引入新 provider 实例 |
| 审计日志开关 | System | 合规要求,不允许租户关闭 |
| Tool 的 `side_effect` 分类 (Doc 05 §3.1) | System | 安全约束,Irreversible 不允许租户改成 Reversible |
| MCP server 的 binary 白名单 (Doc 05 §5.5) | System | 防止任意代码执行 |

```rust
pub struct ConfigLayer {
    pub source: ConfigSource,
    pub locked_keys: Vec<String>,          // 不允许下游覆盖的 key 路径
}

// 启动期校验:如果 Tenant config 试图覆盖被锁定的 key,直接报错
fn validate_layer_overrides(...) -> Result<(), ConfigError> {
    for (key, _value) in tenant_overrides.flatten() {
        if system_layer.locked_keys.contains(&key) {
            return Err(ConfigError::AttemptedLockedOverride { key });
        }
    }
    Ok(())
}
```

---

## 3. 配置数据模型

### 3.1 顶层 schema

```rust
pub struct Config {
    pub version: ConfigVersion,            // 用于 migration
    pub providers: ProvidersConfig,        // Doc 01
    pub pipeline: PipelineConfig,          // Doc 02
    pub cache: CacheConfig,                // Doc 03
    pub agents: AgentsConfig,              // Doc 04
    pub tools: ToolsConfig,                // Doc 05 (含 mcp_servers, skills)
    pub tenants: HashMap<TenantId, TenantConfig>,
    pub secrets: SecretsConfig,
    pub observability: ObservabilityConfig,
    pub deployment: DeploymentConfig,
}

pub struct TenantConfig {
    pub id: TenantId,
    pub display_name: String,
    pub status: TenantStatus,             // Active / Suspended / PendingDeletion
    pub created_at: SystemTime,
    pub provisioned_by: Principal,
    
    /// 租户级覆盖 (深合并到全局对应 section)
    pub overrides: TenantOverrides,
    
    /// 配额硬上限
    pub quotas: TenantQuotas,
    
    /// 该租户的可见 Provider 子集
    pub allowed_providers: Vec<ProviderId>,
    
    /// 该租户的可见 Tool / Skill 子集
    pub allowed_tools: Vec<ToolId>,
    pub allowed_skills: Vec<SkillId>,
    
    /// 可见 MCP server (会启动隔离的子进程)
    pub allowed_mcp_servers: Vec<McpServerId>,
    
    /// 隔离配置
    pub isolation: TenantIsolation,
}

pub enum TenantStatus {
    Active,
    Suspended { since: SystemTime, reason: String },
    PendingDeletion { scheduled_for: SystemTime },
    Deleted { deleted_at: SystemTime, audit_ref: AuditRef },
}

pub struct TenantIsolation {
    /// CLI / MCP 子进程的 HOME 目录 (Doc 01 §6.2 + Doc 05 §5.3)
    pub subprocess_home: PathBuf,
    
    /// Cache key namespace prefix (Doc 03 §3.2 强约束)
    pub cache_namespace: String,
    
    /// 事件日志的逻辑分区
    pub event_log_partition: String,
    
    /// Tenant-scoped 的 secret namespace
    pub secret_namespace: String,
}
```

### 3.2 TenantOverrides 形态

```rust
pub struct TenantOverrides {
    pub middleware_budget: Option<BudgetOverrides>,
    pub middleware_prompt_guard: Option<PromptGuardOverrides>,
    pub cache: Option<CacheOverrides>,
    pub agent_blueprints: Vec<AgentBlueprint>,        // 租户自定义的 Agent
    pub routing_policy: Option<RoutingPolicyName>,
    pub default_models: Option<HashMap<ModelTier, ProviderId>>,
}
```

合并规则 (深合并):
- `Option<T>` 字段:Some(value) 覆盖,None 继承父层
- `Vec<T>` 字段:**追加** (不是替换,除非 explicit replace)
- `HashMap<K, V>` 字段:按 key 合并,同 key 时深层覆盖

### 3.3 Workspace 与 Session

Tenant 之下还有两层概念,但不是配置层级,而是运行时实体:

```rust
pub struct Workspace {
    pub id: WorkspaceId,
    pub tenant: TenantId,
    pub display_name: String,
    pub principal_owners: Vec<Principal>,
    pub iam_scopes: Vec<Scope>,           // 该 workspace 提供的 scope
    pub created_at: SystemTime,
}

pub struct Session {
    pub id: SessionId,
    pub workspace: WorkspaceId,
    pub principal: Principal,
    pub started_at: SystemTime,
    pub last_activity_at: AtomicSystemTime,
    pub ephemeral_state: SessionState,    // Cache handle ref / agent state
}
```

| 维度 | Tenant | Workspace | Session |
|---|---|---|---|
| 持续时间 | 长期 (公司/团队级) | 中期 (项目/repo级) | 短期 (单次工作流) |
| 隔离强度 | 硬隔离 (安全边界) | 逻辑隔离 (IAM 区分) | 软隔离 (cache 共享场景) |
| 数量级 | 10²-10³ | 10³-10⁴/tenant | 10⁵-10⁶/day |
| 配置覆盖 | ✅ | ❌ (通过 IAM scope 表达差异) | ❌ |

---

## 4. 租户隔离保证 (汇总)

前面文档分散讨论的隔离点,在租户维度统一汇总:

### 4.1 数据隔离

| 数据 | 隔离机制 | 文档位置 |
|---|---|---|
| Cache key | TENANT + IAM_SCOPES 进入 SHA-256 前缀 | Doc 03 §3.2 |
| L3 Provider cache handle | tenant_namespace 字段强制,跨租户 reject | Doc 03 §10.2 |
| Provider-side prefix cache | system prompt 注入 tenant_marker | Doc 03 §10.3 |
| Trajectory 事件日志 | event_log_partition 逻辑分区 | §3.1 |
| Content Store | hash 前缀加 tenant 维度 | (Doc 04 §3.3 默认行为) |
| Budget Store | tenant_id 是 Redis key 一级前缀 | Doc 02 §4.3 |
| Idempotency Cache | tenant_id + trajectory_id 是 key 一部分 | Doc 05 §4.3 |

### 4.2 进程/资源隔离

| 资源 | 隔离机制 | 文档位置 |
|---|---|---|
| CLI subprocess (Claude / Gemini) | per_tenant_home,独立 OAuth state | Doc 01 §6.2 |
| MCP server subprocess | per_tenant_home + 独立 session pool | Doc 05 §5.3 |
| 嵌入式模型 (mistral.rs / ONNX) | 不隔离 (无状态推理),共享实例 | Doc 01 §6.3 |

### 4.3 网络/认证隔离

| 凭证 | 隔离机制 | 文档位置 |
|---|---|---|
| Provider API key | per-tenant secret 引用 | §5 + Doc 01 §7 |
| OAuth token | secret_namespace 隔离 | §5 |
| MCP server auth | 子进程独立 HOME | Doc 05 §5.3 |

### 4.4 配额隔离

| 资源 | 限制方式 | 文档位置 |
|---|---|---|
| Token 消耗速率 | 租户级 TPM/RPM,Redis 令牌桶 | Doc 02 §4.3 + §9 (本文) |
| Cost 上限 | 租户级 daily/monthly 美金硬上限 | 同上 |
| L3 cache 存储 | 租户级 storage_quota_bytes | Doc 03 §11 |
| Trajectory 并发数 | 租户级 max_concurrent_tasks | §9 |
| MCP 子进程数 | 租户级 max_subprocess_count | §9 |

---

## 5. Secret 管理

### 5.1 绝不入配置文件

```toml
# ❌ 错误:明文 secret
[providers.openai]
api_key = "sk-proj-xxxxxxxxxxxxxxxxxxxxxx"

# ❌ 错误:加密但与解密 key 同地存放
[providers.openai]
api_key_encrypted = "AES256:abc..."
api_key_decrypt_key_path = "/etc/tars/master.key"  # 在同一 host

# ✅ 正确:引用外部 secret manager
[providers.openai]
api_key = { source = "vault", path = "secret/data/tenants/${tenant_id}/openai/api_key" }

# ✅ 正确:引用环境变量 (适合 dev)
[providers.openai]  
api_key = { source = "env", var = "OPENAI_API_KEY" }
```

### 5.2 SecretRef 类型

```rust
pub struct SecretRef {
    pub source: SecretSource,
    pub identifier: String,              // 路径 / var name / KMS key id
    pub cache_ttl: Duration,             // 解析后的缓存时间,默认 5min
}

pub enum SecretSource {
    Env,                                  // env var
    File,                                 // 文件路径,适合 K8s secret mount
    Vault,                                // HashiCorp Vault
    GcpSecretManager,
    AwsSecretsManager,
    AzureKeyVault,
    Inline,                               // 仅 dev,启动时警告
}

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, refr: &SecretRef, ctx: &SecretContext) 
        -> Result<SecretValue, SecretError>;
    
    /// secret 失效时主动通知 (用于 OAuth token 刷新)
    fn invalidate(&self, refr: &SecretRef);
}
```

### 5.3 租户 secret namespace

每个租户的 secret 走独立 namespace,避免误用:

```toml
[providers.openai]
api_key = { source = "vault", path = "secret/data/tenants/${tenant_id}/openai/api_key" }
```

`${tenant_id}` 是模板变量,在 SecretResolver 解析时替换为请求上下文里的 tenant_id。这样:
- 配置文件本身不区分租户,共用模板
- 实际 secret 物理上隔离在 secret manager 的不同路径
- 跨租户访问 secret 必然失败 (path 不存在)

### 5.4 Secret 缓存与刷新

Secret 解析有成本 (网络往返几十 ms),必须缓存:

```rust
pub struct CachedSecretResolver {
    inner: Arc<dyn SecretResolver>,
    cache: Arc<DashMap<SecretCacheKey, CachedSecret>>,
}

struct CachedSecret {
    value: SecretValue,
    resolved_at: Instant,
    expires_at: Instant,
}
```

刷新策略:
- **被动**:cache TTL 过期后下次 resolve 时重新拉取
- **主动**:OAuth refresh - 收到 401 后调 `invalidate` + 立即重新 resolve
- **预热**:租户启动时对其常用 secret 做一次预拉取,避免首次请求延迟

**永不持久化**:Secret 缓存仅在内存,进程重启全部失效。绝不写入磁盘 / DB / Redis。

---

## 6. 配置热加载

### 6.1 热加载分类

```rust
pub enum HotReloadability {
    /// 完全可热加载,无运行时影响
    Hot,
    
    /// 可热加载,但需要排干 in-flight 请求 (例如改 routing policy)
    HotWithDrain,
    
    /// 需要重启子进程 (CLI / MCP server)
    SubprocessRestart,
    
    /// 需要完整重启 Runtime
    FullRestart,
    
    /// 永远不能改 (改了会破坏数据完整性)
    Immutable,
}
```

每个配置 schema 字段都标注其热加载能力 (通过 attribute):

```rust
#[derive(Config)]
pub struct CacheConfig {
    #[reload(Immutable)]                   // 改了全部 cache 失效
    pub hasher_version: u32,
    
    #[reload(Hot)]                         // 改了立即生效
    pub l1_capacity: u64,
    
    #[reload(HotWithDrain)]                // 需要等当前 lookup 完成
    pub l2_url: String,
    
    #[reload(SubprocessRestart)]           // 改了重启 mcp server
    pub mcp_server_args: Vec<String>,
}
```

### 6.2 热加载流程

```rust
pub struct ConfigManager {
    current: Arc<ArcSwap<EffectiveConfig>>,
    watchers: Vec<Arc<dyn ConfigWatcher>>,
    subscribers: broadcast::Sender<ConfigChangeEvent>,
}

impl ConfigManager {
    /// 触发 reload (来源:文件变更通知 / DB 变更通知 / 显式 API)
    pub async fn reload(&self) -> Result<ReloadReport, ConfigError> {
        // 1. 读取新配置
        let new_raw = self.collect_all_layers().await?;
        let new_effective = self.merge_layers(new_raw)?;
        
        // 2. 校验
        self.validate(&new_effective)?;
        
        // 3. Diff 旧配置,分类变更
        let diff = self.diff(&self.current.load(), &new_effective);
        
        // 4. 检查每个变更的 reloadability
        for change in &diff.changes {
            match change.reloadability() {
                HotReloadability::Immutable => {
                    return Err(ConfigError::AttemptedImmutableChange(change.key.clone()));
                }
                HotReloadability::FullRestart => {
                    return Err(ConfigError::RequiresFullRestart(change.key.clone()));
                }
                _ => {}
            }
        }
        
        // 5. 应用 - 按 reloadability 分桶
        let drain_tasks = diff.changes.iter()
            .filter(|c| c.reloadability() == HotReloadability::HotWithDrain)
            .map(|c| self.drain_for(c))
            .collect::<Vec<_>>();
        futures::future::join_all(drain_tasks).await;
        
        // 6. swap
        self.current.store(Arc::new(new_effective.clone()));
        
        // 7. 通知 subprocess restart
        for change in &diff.changes {
            if change.reloadability() == HotReloadability::SubprocessRestart {
                self.restart_subprocess_for(change).await?;
            }
        }
        
        // 8. 广播
        self.subscribers.send(ConfigChangeEvent { diff }).ok();
        
        Ok(ReloadReport { applied: diff.changes.len(), warnings: vec![] })
    }
}
```

### 6.3 reload 的来源

```toml
[config_manager]
sources = ["file_watcher", "db_polling", "explicit_api"]

[config_manager.file_watcher]
paths = ["/etc/tars/", "/etc/tars/tenants/"]
debounce_ms = 500                         # 文件抖动期合并

[config_manager.db_polling]
interval_secs = 30                        # tenant DB 变更轮询
table = "tenant_configs"

[config_manager.explicit_api]
listen = "127.0.0.1:9001"                 # admin API,触发立即 reload
```

---

## 7. 配置校验

### 7.1 启动期校验

```rust
pub fn validate_config(config: &Config) -> Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();
    
    // Schema 完整性
    errors.extend(validate_schema(config));
    
    // Pipeline 层序约束 (Doc 02 §7)
    errors.extend(validate_pipeline_order(&config.pipeline));
    
    // Provider 配置:auth 可解析 / 模型存在 / capability 一致
    errors.extend(validate_providers(&config.providers));
    
    // 租户引用完整性:每个 tenant.allowed_providers 存在于 providers
    errors.extend(validate_tenant_references(&config.tenants, config));
    
    // Secret 引用可达 (做 ping 测试,但不实际拉取)
    errors.extend(validate_secret_references(&config));
    
    // Tool / MCP 配置:binary 路径存在 / scope 引用存在
    errors.extend(validate_tools(&config.tools));
    
    // 锁定层覆盖检查 (§2.2)
    errors.extend(validate_layer_locks(config));
    
    // PromptBuilder 稳定性 (Doc 04 §11)
    errors.extend(validate_prompt_builder_stability(&config.agents));
    
    // 跨段一致性:routing policy 引用的 model tier 在 provider 中可达
    errors.extend(validate_cross_section(config));
    
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
```

启动期校验**全部通过才允许启动**——半启动是反模式 (部分功能能用,部分不能,导致莫名其妙的运行时报错)。

### 7.2 运行期校验

热加载触发的校验更严格:除了启动期所有检查,还要检查 reloadability 约束 (§6.2)。

### 7.3 校验失败的处置

```rust
pub enum ConfigError {
    /// 致命:启动失败 / reload 失败
    Fatal(String),
    
    /// 警告:配置可用但不推荐 (例如 inline secret)
    Warning(String),
    
    /// 已知不兼容:旧 schema 升到新 version 后某字段被废弃
    Deprecated { field: String, removed_in_version: ConfigVersion },
}
```

启动期 Fatal → 进程 exit(1) + 完整错误清单写 stderr (不只第一个错误)。
启动期 Warning → 启动正常,启动 banner 里列出所有 warning。
Deprecated → 启动正常,记录到迁移待办文件 (`/var/lib/tars/migration_todo.json`)。

---

## 8. 租户生命周期

### 8.1 Provision

```rust
pub struct ProvisionRequest {
    pub display_name: String,
    pub initial_quotas: TenantQuotas,
    pub initial_owners: Vec<Principal>,
    pub allowed_providers: Vec<ProviderId>,
    pub allowed_tools: Vec<ToolId>,
}

pub async fn provision_tenant(req: ProvisionRequest) -> Result<TenantConfig, ProvisionError> {
    // 1. 分配 TenantId
    let tenant_id = TenantId::generate();
    
    // 2. 创建隔离资源
    let isolation = TenantIsolation {
        subprocess_home: PathBuf::from(format!("/var/lib/tars/tenants/{}/home", tenant_id)),
        cache_namespace: format!("ns:{}", tenant_id),
        event_log_partition: format!("evt_{}", tenant_id),
        secret_namespace: format!("tenants/{}", tenant_id),
    };
    
    // 3. 物理初始化
    fs::create_dir_all(&isolation.subprocess_home)?;
    db.execute(&format!("CREATE TABLE IF NOT EXISTS {}_events (...)", 
        isolation.event_log_partition)).await?;
    secret_manager.create_namespace(&isolation.secret_namespace).await?;
    
    // 4. 写入 TenantConfig
    let config = TenantConfig {
        id: tenant_id.clone(),
        display_name: req.display_name,
        status: TenantStatus::Active,
        created_at: SystemTime::now(),
        provisioned_by: current_principal(),
        overrides: Default::default(),
        quotas: req.initial_quotas,
        allowed_providers: req.allowed_providers,
        allowed_tools: req.allowed_tools,
        allowed_skills: vec![],
        allowed_mcp_servers: vec![],
        isolation,
    };
    
    db.insert_tenant_config(&config).await?;
    
    // 5. 触发 ConfigManager reload
    config_manager.reload().await?;
    
    // 6. 审计
    audit_log.write(AuditEvent::TenantProvisioned { 
        tenant: tenant_id, 
        by: current_principal() 
    }).await?;
    
    Ok(config)
}
```

### 8.2 Suspend / Resume

Suspend 不删数据,只阻止新请求:

```rust
pub async fn suspend_tenant(tenant: &TenantId, reason: String) -> Result<(), SuspendError> {
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::Suspended { 
        since: SystemTime::now(), 
        reason: reason.clone(),
    };
    db.update_tenant_config(&config).await?;
    
    // 1. 立即拒绝该租户的新请求 (Pipeline IAM 层检查 status)
    config_manager.reload().await?;
    
    // 2. 优雅排干 in-flight 请求 (按 deadline)
    runtime.drain_tenant(tenant, Duration::from_secs(60)).await;
    
    // 3. 主动清理 L3 cache (避免存储费继续累积)
    cache_janitor.purge_tenant(tenant).await?;
    
    // 4. 杀掉该租户的 MCP / CLI 子进程
    subprocess_manager.kill_tenant_processes(tenant).await;
    
    audit_log.write(AuditEvent::TenantSuspended { tenant: tenant.clone(), reason }).await?;
    Ok(())
}

pub async fn resume_tenant(tenant: &TenantId) -> Result<(), ResumeError> {
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::Active;
    db.update_tenant_config(&config).await?;
    config_manager.reload().await?;
    audit_log.write(AuditEvent::TenantResumed { tenant: tenant.clone() }).await?;
    Ok(())
}
```

### 8.3 Delete (GDPR-style)

Delete 是不可逆操作,**两阶段提交**:

```rust
pub async fn schedule_deletion(
    tenant: &TenantId, 
    delay: Duration,
) -> Result<DeletionHandle, DeleteError> {
    // 阶段 1:标记 PendingDeletion,延迟 N 天 (默认 30 天) 真正删除
    // 这期间数据仍然存在,可以 abort_deletion 撤销
    
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::PendingDeletion {
        scheduled_for: SystemTime::now() + delay,
    };
    db.update_tenant_config(&config).await?;
    
    // 该租户进入 suspend 状态(不能再用)
    suspend_tenant(tenant, "pending_deletion".into()).await?;
    
    // 注册定时任务在 scheduled_for 到期时执行
    scheduler.schedule_at(SystemTime::now() + delay, 
        Box::new(move || actually_delete(tenant.clone()))).await?;
    
    Ok(DeletionHandle { tenant: tenant.clone(), scheduled_for: ... })
}

async fn actually_delete(tenant: TenantId) -> Result<(), DeleteError> {
    // 阶段 2:级联删除
    
    // 1. 终止所有可能还在跑的 trajectory
    runtime.abort_tenant(&tenant).await?;
    
    // 2. 删事件日志 (按 partition drop)
    db.execute(&format!("DROP TABLE {}_events", 
        config.isolation.event_log_partition)).await?;
    
    // 3. 删 ContentStore 内容 (按 tenant 前缀)
    content_store.purge_tenant(&tenant).await?;
    
    // 4. 删 cache (L2 Redis 按 prefix scan + delete; L3 按 list+delete)
    cache_registry.invalidate_tenant(&tenant).await?;
    
    // 5. 删 budget store 历史
    budget_store.purge_tenant(&tenant).await?;
    
    // 6. 删 subprocess HOME 目录
    fs::remove_dir_all(&config.isolation.subprocess_home)?;
    
    // 7. 删 secret namespace
    secret_manager.delete_namespace(&config.isolation.secret_namespace).await?;
    
    // 8. 删 tenant config (最后一步)
    db.delete_tenant_config(&tenant).await?;
    
    // 9. 写不可篡改的 audit record
    audit_log.write(AuditEvent::TenantDeleted { 
        tenant: tenant.clone(),
        deleted_at: SystemTime::now(),
        completed_steps: vec!["events", "content", "cache", "budget", "fs", "secrets", "config"],
    }).await?;
    
    Ok(())
}
```

**关键不变量**:
- 阶段 1 → 阶段 2 之间任何一步失败,整个删除中止 + 告警
- 阶段 2 的 9 个 step 必须按顺序执行,前面失败就停在那里 (不要乱删)
- 每一步都要有"已删 X 个对象"的 metric,便于审计验证
- audit record **永不删除**,即使 tenant 本身被删

---

## 9. 配额与计费

### 9.1 Quota 模型

```rust
pub struct TenantQuotas {
    /// 速率限制 (硬上限,超额触发 429)
    pub max_rpm: u32,                      // 每分钟请求数
    pub max_tpm: u64,                      // 每分钟 input+output token
    pub max_concurrent_tasks: u32,         // 同时运行的 trajectory 数
    pub max_subprocess_count: u32,         // CLI + MCP 子进程总数上限
    
    /// 容量限制
    pub max_l3_storage_bytes: u64,
    pub max_event_log_size_bytes: u64,
    
    /// 成本上限
    pub daily_cost_usd_soft: f64,          // 触发告警
    pub daily_cost_usd_hard: f64,          // 触发熔断
    pub monthly_cost_usd_hard: f64,
    
    /// Tool/Skill 调用频次上限
    pub max_tool_calls_per_day: HashMap<ToolId, u64>,
}
```

### 9.2 计费数据流

```
每个 LLM 调用 / Tool 调用结束
       │
       ▼
Telemetry (Doc 02 §4.1) 提取 usage + 计算 cost
       │
       ▼
BudgetStore::commit (Redis 原子扣减)
       │
       ▼
异步双写:
  ├─→ 计费日志 (PostgreSQL `billing_events` 表) - 单事件可审计
  └─→ Aggregation 服务 - 实时聚合到小时/日/月维度
       │
       ▼
触发器:
  - 超 soft 阈值 → 告警 (Slack / email)
  - 超 hard 阈值 → 熔断 (BudgetMiddleware 拒绝)
  - 月底关账 → 导出 CSV/JSON 给计费系统
```

### 9.3 计费报表导出

```rust
#[async_trait]
pub trait BillingExporter: Send + Sync {
    async fn export(
        &self,
        period: BillingPeriod,
        format: ExportFormat,
    ) -> Result<ExportArtifact, BillingError>;
}

pub struct BillingPeriod {
    pub start: SystemTime,
    pub end: SystemTime,
    pub tenant_filter: Option<TenantId>,
}

pub enum ExportFormat {
    Csv,
    Json,
    StripeWebhook,                         // 直接推到 Stripe metered billing
    InternalKafka { topic: String },
}
```

报表内容:
- 按 tenant 维度汇总 token / cost / 调用次数
- 按 model / tool 维度细分
- 按 day 维度时间序列
- 单独列出 cache 节省金额 (Doc 03 §12 `cache.l3.cost_saved_usd`)

---

## 10. 审计与合规

### 10.1 不可篡改的审计日志

```rust
pub enum AuditEvent {
    // 租户生命周期
    TenantProvisioned { tenant: TenantId, by: Principal },
    TenantSuspended { tenant: TenantId, reason: String },
    TenantResumed { tenant: TenantId },
    TenantDeleted { tenant: TenantId, deleted_at: SystemTime, completed_steps: Vec<String> },
    
    // 配置变更
    ConfigReloaded { changes: Vec<ConfigChange>, by: Principal },
    ConfigReloadRejected { reason: String, by: Principal },
    
    // 安全事件
    IamDenied { principal: Principal, resource: ResourceRef, reason: String },
    SecurityAlert { kind: String, details: serde_json::Value },
    CompensationFailed { trajectory: TrajectoryId, compensation: CompensationId, error: String },
    
    // 数据访问
    SecretAccessed { ref: SecretRef, by: Principal },
    
    // 计费事件
    BudgetSoftLimitExceeded { tenant: TenantId, period: String, amount: f64 },
    BudgetHardLimitExceeded { tenant: TenantId, period: String, amount: f64 },
}

#[async_trait]
pub trait AuditLog: Send + Sync {
    async fn write(&self, event: AuditEvent) -> Result<AuditRef, AuditError>;
}
```

**实现要求**:
- 写入 append-only 存储 (Postgres + immutable column / WORM S3 / 区块链)
- 每条事件签名 (HMAC with rotated key)
- 异步双写到外部 SIEM (Splunk / Datadog / ELK)
- 即使 tenant 被删,审计记录仍保留 7 年 (合规要求)

### 10.2 GDPR 合规

- **数据可携权**:`export_tenant_data` API 导出所有该租户的事件 / cache key (不含 LLM 响应内容,因为是衍生品) / billing
- **被遗忘权**:§8.3 的 30 天延迟 + 级联删除
- **数据本地化**:Provider 配置可指定 region,租户配置选用对应 region 的 provider (例如欧盟租户只能用 EU region 的 Anthropic / Gemini)

```toml
[providers.claude_eu]
type = "anthropic"
base_url = "https://api.anthropic.com"     # Anthropic 没有显式 EU endpoint,但可通过 VPC 路由
region = "eu-west-1"
data_residency = "EU"

[tenants.eu_customer_acme]
allowed_providers = ["claude_eu", "gemini_eu"]
data_residency_required = "EU"             # 强制只能用 EU 标记的 provider
```

---

## 11. 配置形态汇总

完整 schema 跨越前面所有文档,这里给一个最小可工作的示例:

```toml
# config.toml
version = "1.0"

# === Doc 01 ===
[providers.claude_api]
type = "anthropic"
auth = { source = "vault", path = "secret/data/tenants/${tenant_id}/anthropic" }
default_model = "claude-opus-4-7"

[providers.local_qwen]
type = "openai_compat"
base_url = "http://ryzen-node-1:8000/v1"
auth = { source = "none" }

# === Doc 02 ===
[pipeline]
order = ["telemetry", "auth", "iam", "budget", "cache_lookup", 
         "prompt_guard", "schema_validation", "routing", 
         "circuit_breaker", "retry"]

[pipeline.constraints]
"iam" = { must_be_before = ["cache_lookup"] }
"telemetry" = { must_be_outermost = true }

[middleware.budget]
backend = "redis"
default_tpm = 100000
default_daily_cost_usd = 50

# === Doc 03 ===
[cache]
hasher_version = 1                         # 锁定项

[cache.l1]
max_capacity = 10000

[cache.l2]
url = { source = "env", var = "REDIS_URL" }

[cache.l3]
storage_quota_bytes = 10737418240

# === Doc 04 ===
[agents]
prompt_builder_stability_check = true

[[agents.blueprints]]
id = "code_reviewer"
orchestrator_tier = "default"
worker_tier = "reasoning"
critic_tier = "default"

# === Doc 05 ===
[mcp_servers.filesystem]
type = "stdio"
binary = "/usr/local/bin/mcp-filesystem"
mode = "long_lived"
auth = { source = "delegate", per_tenant_home = true }

# === Doc 06 (本文) ===
[secrets]
default_resolver = "vault"

[secrets.vault]
url = { source = "env", var = "VAULT_ADDR" }
token = { source = "env", var = "VAULT_TOKEN" }

[observability]
otel_endpoint = { source = "env", var = "OTEL_EXPORTER_OTLP_ENDPOINT" }
audit_log_backend = "postgres"
audit_log_replication = ["splunk"]

[deployment]
node_id = { source = "env", var = "NODE_ID" }
discovery = "static"                       # vs k8s / consul
peers = ["node-1:7000", "node-2:7000"]

# === 租户 ===
[tenants.acme_corp]
display_name = "ACME Corporation"
status = "active"
allowed_providers = ["claude_api", "local_qwen"]
allowed_tools = ["fs.read_file", "git.fetch_pr_diff"]
allowed_mcp_servers = ["filesystem"]

[tenants.acme_corp.quotas]
max_tpm = 500000
daily_cost_usd_hard = 500

[tenants.acme_corp.overrides.cache]
l3_storage_quota_bytes = 53687091200       # 50 GB

[tenants.acme_corp.isolation]
subprocess_home = "/var/lib/tars/tenants/acme_corp/home"
cache_namespace = "ns:acme_corp"
event_log_partition = "evt_acme_corp"
secret_namespace = "tenants/acme_corp"
```

---

## 12. 测试策略

### 12.1 配置 schema 测试

```rust
#[test]
fn schema_round_trip() {
    let toml_str = include_str!("../examples/full_config.toml");
    let parsed: Config = toml::from_str(toml_str).unwrap();
    let re_serialized = toml::to_string(&parsed).unwrap();
    let re_parsed: Config = toml::from_str(&re_serialized).unwrap();
    assert_eq!(parsed, re_parsed);
}

#[test]
fn pipeline_constraint_violation_rejected() {
    let mut config = test_config();
    config.pipeline.order = vec!["cache_lookup".into(), "iam".into()];  // IAM 在 cache 之后!
    
    let errors = validate_config(&config).unwrap_err();
    assert!(errors.iter().any(|e| matches!(e, ConfigError::Fatal(s) if s.contains("iam"))));
}

#[test]
fn locked_key_override_rejected() {
    let mut config = test_config();
    config.tenants.insert("evil".into(), TenantConfig {
        overrides: TenantOverrides {
            cache: Some(CacheOverrides {
                hasher_version: Some(99),    // 试图覆盖锁定项
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    });
    
    let errors = validate_config(&config).unwrap_err();
    assert!(errors.iter().any(|e| matches!(e, ConfigError::Fatal(s) 
        if s.contains("locked"))));
}
```

### 12.2 租户隔离端到端测试

```rust
#[tokio::test]
async fn tenant_a_cache_does_not_leak_to_tenant_b() {
    let runtime = test_runtime_with_two_tenants("a", "b").await;
    
    // 租户 A 用同样的 prompt 调用,触发 cache 写入
    let req = test_request_with_tenant("a");
    runtime.execute(req.clone()).await.unwrap();
    
    // 租户 B 用同样的 prompt 调用
    let req_b = ChatRequest { tenant_id: "b".into(), ..req };
    let stats_before = mock_provider.invocation_count();
    runtime.execute(req_b).await.unwrap();
    let stats_after = mock_provider.invocation_count();
    
    // B 必须实际调 provider (不能命中 A 的缓存)
    assert_eq!(stats_after - stats_before, 1);
}

#[tokio::test]
async fn deleted_tenant_data_completely_purged() {
    let runtime = test_runtime();
    let tenant = provision_test_tenant(&runtime).await;
    
    // 制造一些数据
    create_trajectories(&runtime, &tenant, 10).await;
    create_cache_entries(&runtime, &tenant, 50).await;
    
    schedule_deletion(&tenant, Duration::ZERO).await.unwrap();
    actually_delete(&tenant).await.unwrap();
    
    // 验证全部资源消失
    assert!(db.tenant_exists(&tenant).await.unwrap() == false);
    assert!(content_store.tenant_object_count(&tenant).await.unwrap() == 0);
    assert!(cache_registry.tenant_key_count(&tenant).await.unwrap() == 0);
    assert!(!fs::exists(&format!("/var/lib/tars/tenants/{}", tenant)));
    
    // 但审计 log 必须保留
    let audit_records = audit_log.query_for_tenant(&tenant).await.unwrap();
    assert!(audit_records.iter().any(|r| matches!(r.event, AuditEvent::TenantDeleted { .. })));
}
```

### 12.3 热加载测试

```rust
#[tokio::test]
async fn budget_change_hot_reloads() {
    let manager = test_config_manager();
    
    // 初始 budget 100
    assert_eq!(manager.current().middleware.budget.default_daily_cost_usd, 100.0);
    
    // 修改配置文件
    update_config_file(|c| { c.middleware.budget.default_daily_cost_usd = 200.0 });
    manager.reload().await.unwrap();
    
    // 立即生效
    assert_eq!(manager.current().middleware.budget.default_daily_cost_usd, 200.0);
}

#[tokio::test]
async fn immutable_change_rejected() {
    let manager = test_config_manager();
    
    update_config_file(|c| { c.cache.hasher_version = 999 });
    let result = manager.reload().await;
    
    assert!(matches!(result, Err(ConfigError::AttemptedImmutableChange(s)) 
        if s.contains("hasher_version")));
}
```

### 12.4 Secret 测试

```rust
#[tokio::test]
async fn secret_template_resolves_per_tenant() {
    let resolver = test_secret_resolver();
    
    let refr = SecretRef {
        source: SecretSource::Vault,
        identifier: "secret/data/tenants/${tenant_id}/openai/api_key".into(),
        ..Default::default()
    };
    
    let key_a = resolver.resolve(&refr, &ctx_for_tenant("a")).await.unwrap();
    let key_b = resolver.resolve(&refr, &ctx_for_tenant("b")).await.unwrap();
    
    // 不同租户拿到不同 secret
    assert_ne!(key_a, key_b);
}

#[tokio::test]
async fn cross_tenant_secret_access_rejected() {
    // 租户 A 的代码试图读租户 B 的 secret
    let resolver = test_secret_resolver();
    let refr_b = SecretRef {
        source: SecretSource::Vault,
        identifier: "secret/data/tenants/b/openai/api_key".into(),
        ..Default::default()
    };
    
    let result = resolver.resolve(&refr_b, &ctx_for_tenant("a")).await;
    assert!(matches!(result, Err(SecretError::AccessDenied)));
}
```

---

## 13. 反模式清单

1. **不要在配置文件里写明文 secret**——永远用 SecretRef。
2. **不要让租户配置覆盖锁定的安全约束**——cache hasher_version、pipeline 层序、tool side_effect 类型等。
3. **不要静默接受部分加载的配置**——校验失败 = 启动失败 / reload 失败,不允许半残运行。
4. **不要假设所有配置都能热加载**——schema 上明确标注 reloadability。
5. **不要在 Tenant Provision 时跳过隔离资源初始化**——subprocess_home 不存在的话第一次 MCP 调用就崩。
6. **不要在 Tenant Delete 时漏掉某个数据存储**——级联删除要覆盖事件 / cache / content / budget / fs / secret 全部 7 类。
7. **不要把审计日志放在与业务数据同一存储**——必须有独立写入路径,业务 DB 挂了审计仍能写。
8. **不要让 Tenant Suspend 立即影响 in-flight 请求**——Drain 期默认 60s,允许已开始的工作完成。
9. **不要在 secret cache 里持久化值**——内存 only,进程重启全失效。
10. **不要让 Configuration 类持有可变全局状态**——通过 ArcSwap 原子替换,读路径无锁。
11. **不要在配额触发熔断时延迟通知**——告警必须实时,事后才知道亏钱已经晚了。
12. **不要让删除事件可被覆盖或编辑**——audit log append-only,即使 admin 也不能改。
13. **不要混用 system / tenant 配置**——tenant override 是显式的 `overrides: TenantOverrides`,不是直接修改全局 section。
14. **不要让 tenant_id 字符串自由生成**——必须通过 `TenantId::generate()`,禁止用户提供 ID (容易冲突 / 注入攻击)。
15. **不要让 deprecated 字段静默被忽略**——记录到 migration_todo,promo 给运维。

---

## 14. 与上下游的契约

### 上游 (Application / Frontend Adapter) 承诺

- 通过 `ConfigManager::current()` 拿到的是已经合并 / 校验过的 EffectiveConfig
- 所有租户切换通过 RequestContext.tenant_id 传递,不通过全局变量
- 不直接访问租户的 isolation 路径 (subprocess_home / cache_namespace),通过对应的 trait 接口 (SubprocessManager / CacheRegistry)

### 下游契约 (各 Doc 01-05 组件)

- 接收 EffectiveConfig 作为构造参数,不自己读文件
- 实现 `ConfigSubscriber` trait 监听变更:
  ```rust
  #[async_trait]
  pub trait ConfigSubscriber: Send + Sync {
      fn interested_in(&self) -> Vec<ConfigKeyPattern>;
      async fn on_change(&self, change: &ConfigChange) -> Result<(), SubscriberError>;
  }
  ```
- 不缓存 secret 解析结果超过 SecretRef.cache_ttl
- Tenant Suspend / Delete 时清理所属资源 (subprocess kill / cache purge / etc.)

### 跨节点契约

多节点部署时:
- 每个节点独立 `ConfigManager`,通过 file watcher / DB polling 同步
- 配置变更不要求全节点同时生效,但必须最终一致 (eventually consistent)
- 节点之间不直接通信配置变更——文件系统 / DB 是中介
- audit_log 必须复制到中心化存储 (本地 + Splunk),节点挂掉不丢

---

## 15. 待办与开放问题

- [ ] Configuration schema 用什么 DSL: TOML + serde / Cue / Pkl / Dhall
- [ ] 多区域部署的 config 同步策略 (push vs pull / DB region routing)
- [ ] Per-tenant 加密 (rest 加密用 tenant-specific key, 满足金融合规)
- [ ] 审计 log 的不可篡改性如何技术性保证 (HMAC vs WORM vs blockchain)
- [ ] Tenant ID 的命名规则与可读性 (UUID vs short_id vs human-readable)
- [ ] 租户 quota 可视化 dashboard 的 schema 与 API
- [ ] Configuration migration 工具链 (v1 → v2 的自动转换 + dry-run)
- [ ] Secret rotation 的自动化 (calendar-based vs event-driven)
- [ ] Multi-region 租户的 data residency 强制执行 (启动期 vs 运行期校验)
- [ ] Workspace 是否需要独立的 quota / IAM 子层级 (本文档目前认为不需要,通过 tenant 已足够)
