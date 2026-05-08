# Doc 06 — Configuration & Multi-Tenancy Management

> Scope: define the layers, sources, priority, and hot-reload mechanism for configuration; the multi-tenant data model and isolation guarantees; secret management; tenant lifecycle; quotas and billing.
>
> Cross-cutting: this doc introduces no new runtime components — it standardizes the unified shape of the "configuration / tenant" dimensions already mentioned across Doc 01-05.

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Configuration as code** | All configuration is expressed in versioned text files; Git is the single source of truth (the DB is just a hot-reload cache) |
| **Hard tenant isolation** | tenant_id is a security boundary — isolates IAM / Cache / Budget / Auth / MCP subprocesses / event log |
| **Layered overrides** | Defaults → System → User → Tenant → Request; deeper layers override shallower ones, but some layers are forbidden from overriding |
| **Secrets never live in files** | All secrets are pulled by reference, resolved at runtime, never persisted in plaintext |
| **Hot-reloadability is explicit** | Not all config can be hot-reloaded; what can and cannot must be explicitly marked in the schema |
| **Validation up front** | Full validation at startup + on hot reload; validation failure rejects startup / rejects apply, no partial loading allowed |
| **Complete tenant lifecycle** | Provision / Suspend / Resume / Delete end-to-end; Delete must cascade-clean |
| **Observable quotas** | Per-tenant token / cost / cache usage queryable in real time, exportable as billing reports |

**Anti-goals**:
- Don't hardcode secrets in config — not even in dev (use a dev profile of the secret manager instead)
- Don't let tenant config override security constraints (IAM order, cache hasher_version, etc.)
- No "dynamic tenant discovery" — every tenant is explicitly registered through the provisioning flow
- Don't let config schema evolution break old tenants — there must be a migration path

---

## 2. Configuration Layers and Priority

```
                        ┌──────────────────┐
                        │ Per-Request      │  ← rarely used, mainly testing
                        │ overrides        │
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ Tenant overrides │  ← Postgres, hot-reloadable
                        │ (DB-backed)      │
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ User config      │  ← ~/.config/tars/*.toml
                        │ (file-backed)    │     (local deploy / dev env)
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ System config    │  ← /etc/tars/*.toml
                        │ (file-backed)    │     (production deploy default)
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ Built-in config  │  ← embedded in binary (ship with code)
                        │ (embedded)       │
                        └────────┬─────────┘
                                 │ overrides
                        ┌──────────────────┐
                        │ Compiled         │  ← const in Default impl
                        │ defaults         │
                        └──────────────────┘
```

### 2.1 Priority Rules

- **Deeper overrides shallower** — Per-Request > Tenant > User > System > Built-in > Compiled
- **Arrays / Maps merge rather than replace** (unless explicitly marked `replace = true`)
- **Presence > default** — a config item written out, even with an empty value, counts as "explicitly set"
- **All merging happens at startup / hot reload** — at runtime you get an already-collapsed effective config, with no runtime branching

### 2.2 Layers Forbidden from Override

Certain layers must be locked at the system level; tenants/users cannot override them:

| Config item | Locked layer | Rationale |
|---|---|---|
| Pipeline layer ordering constraints (Doc 02 §7) | System | Security constraint, IAM must precede Cache |
| Cache hasher_version (Doc 03 §11) | System | Changing it invalidates cache for all tenants |
| Provider list itself | System | Tenants can only choose to enable, not introduce new provider instances |
| Audit log toggle | System | Compliance requirement, tenants are not allowed to disable |
| Tool `side_effect` classification (Doc 05 §3.1) | System | Security constraint, Irreversible cannot be downgraded by a tenant to Reversible |
| MCP server binary allowlist (Doc 05 §5.5) | System | Prevents arbitrary code execution |

```rust
pub struct ConfigLayer {
    pub source: ConfigSource,
    pub locked_keys: Vec<String>,          // key paths that downstream cannot override
}

// Startup validation: if Tenant config tries to override a locked key, fail immediately
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

## 3. Configuration Data Model

### 3.1 Top-level schema

```rust
pub struct Config {
    pub version: ConfigVersion,            // for migration
    pub providers: ProvidersConfig,        // Doc 01
    pub pipeline: PipelineConfig,          // Doc 02
    pub cache: CacheConfig,                // Doc 03
    pub agents: AgentsConfig,              // Doc 04
    pub tools: ToolsConfig,                // Doc 05 (incl. mcp_servers, skills)
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
    
    /// Tenant-level overrides (deep-merged into corresponding global section)
    pub overrides: TenantOverrides,
    
    /// Quota hard limits
    pub quotas: TenantQuotas,
    
    /// Subset of Providers visible to this tenant
    pub allowed_providers: Vec<ProviderId>,
    
    /// Subset of Tools / Skills visible to this tenant
    pub allowed_tools: Vec<ToolId>,
    pub allowed_skills: Vec<SkillId>,
    
    /// Visible MCP servers (will spawn isolated subprocesses)
    pub allowed_mcp_servers: Vec<McpServerId>,
    
    /// Isolation configuration
    pub isolation: TenantIsolation,
}

pub enum TenantStatus {
    Active,
    Suspended { since: SystemTime, reason: String },
    PendingDeletion { scheduled_for: SystemTime },
    Deleted { deleted_at: SystemTime, audit_ref: AuditRef },
}

pub struct TenantIsolation {
    /// HOME directory for CLI / MCP subprocesses (Doc 01 §6.2 + Doc 05 §5.3)
    pub subprocess_home: PathBuf,
    
    /// Cache key namespace prefix (Doc 03 §3.2 hard constraint)
    pub cache_namespace: String,
    
    /// Logical partition for the event log
    pub event_log_partition: String,
    
    /// Tenant-scoped secret namespace
    pub secret_namespace: String,
}
```

### 3.2 TenantOverrides shape

```rust
pub struct TenantOverrides {
    pub middleware_budget: Option<BudgetOverrides>,
    pub middleware_prompt_guard: Option<PromptGuardOverrides>,
    pub cache: Option<CacheOverrides>,
    pub agent_blueprints: Vec<AgentBlueprint>,        // tenant-defined Agents
    pub routing_policy: Option<RoutingPolicyName>,
    pub default_models: Option<HashMap<ModelTier, ProviderId>>,
}
```

Merge rules (deep merge):
- `Option<T>` field: Some(value) overrides, None inherits from parent layer
- `Vec<T>` field: **append** (not replace, unless explicit replace)
- `HashMap<K, V>` field: merge by key, deeper layer overrides on key collision

### 3.3 Workspace and Session

Below Tenant there are two more concepts — but these are not config layers, they are runtime entities:

```rust
pub struct Workspace {
    pub id: WorkspaceId,
    pub tenant: TenantId,
    pub display_name: String,
    pub principal_owners: Vec<Principal>,
    pub iam_scopes: Vec<Scope>,           // scopes provided by this workspace
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

| Dimension | Tenant | Workspace | Session |
|---|---|---|---|
| Duration | Long-term (company / team level) | Mid-term (project / repo level) | Short-term (single workflow) |
| Isolation strength | Hard (security boundary) | Logical (IAM differentiated) | Soft (cache-sharing scenarios) |
| Order of magnitude | 10²-10³ | 10³-10⁴/tenant | 10⁵-10⁶/day |
| Config overrides | ✅ | ❌ (express differences via IAM scope) | ❌ |

---

## 4. Tenant Isolation Guarantees (Summary)

The isolation points discussed across earlier docs, consolidated along the tenant dimension:

### 4.1 Data isolation

| Data | Isolation mechanism | Doc reference |
|---|---|---|
| Cache key | TENANT + IAM_SCOPES go into the SHA-256 prefix | Doc 03 §3.2 |
| L3 Provider cache handle | tenant_namespace field enforced, cross-tenant reject | Doc 03 §10.2 |
| Provider-side prefix cache | tenant_marker injected into system prompt | Doc 03 §10.3 |
| Trajectory event log | event_log_partition logical partition | §3.1 |
| Content Store | tenant dimension prefixed onto hash | (Doc 04 §3.3 default behavior) |
| Budget Store | tenant_id is the first-level prefix of the Redis key | Doc 02 §4.3 |
| Idempotency Cache | tenant_id + trajectory_id is part of the key | Doc 05 §4.3 |

### 4.2 Process / resource isolation

| Resource | Isolation mechanism | Doc reference |
|---|---|---|
| CLI subprocess (Claude / Gemini) | per_tenant_home, independent OAuth state | Doc 01 §6.2 |
| MCP server subprocess | per_tenant_home + independent session pool | Doc 05 §5.3 |
| Embedded models (mistral.rs / ONNX) | not isolated (stateless inference), shared instance | Doc 01 §6.3 |

### 4.3 Network / auth isolation

| Credential | Isolation mechanism | Doc reference |
|---|---|---|
| Provider API key | per-tenant secret reference | §5 + Doc 01 §7 |
| OAuth token | secret_namespace isolation | §5 |
| MCP server auth | independent subprocess HOME | Doc 05 §5.3 |

### 4.4 Quota isolation

| Resource | Limit mechanism | Doc reference |
|---|---|---|
| Token consumption rate | per-tenant TPM/RPM, Redis token bucket | Doc 02 §4.3 + §9 (this doc) |
| Cost cap | per-tenant daily/monthly USD hard cap | same as above |
| L3 cache storage | per-tenant storage_quota_bytes | Doc 03 §11 |
| Trajectory concurrency | per-tenant max_concurrent_tasks | §9 |
| MCP subprocess count | per-tenant max_subprocess_count | §9 |

---

## 5. Secret Management

### 5.1 Never goes into config files

```toml
# ❌ Wrong: plaintext secret
[providers.openai]
api_key = "sk-proj-xxxxxxxxxxxxxxxxxxxxxx"

# ❌ Wrong: encrypted but stored next to its decryption key
[providers.openai]
api_key_encrypted = "AES256:abc..."
api_key_decrypt_key_path = "/etc/tars/master.key"  # on the same host

# ✅ Correct: reference an external secret manager
[providers.openai]
api_key = { source = "vault", path = "secret/data/tenants/${tenant_id}/openai/api_key" }

# ✅ Correct: reference an environment variable (suitable for dev)
[providers.openai]  
api_key = { source = "env", var = "OPENAI_API_KEY" }
```

### 5.2 SecretRef type

```rust
pub struct SecretRef {
    pub source: SecretSource,
    pub identifier: String,              // path / var name / KMS key id
    pub cache_ttl: Duration,             // cache duration after resolution, default 5min
}

pub enum SecretSource {
    Env,                                  // env var
    File,                                 // file path, suits K8s secret mount
    Vault,                                // HashiCorp Vault
    GcpSecretManager,
    AwsSecretsManager,
    AzureKeyVault,
    Inline,                               // dev only, warn at startup
}

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, refr: &SecretRef, ctx: &SecretContext) 
        -> Result<SecretValue, SecretError>;
    
    /// Proactive notification on secret invalidation (used for OAuth token refresh)
    fn invalidate(&self, refr: &SecretRef);
}
```

### 5.3 Per-tenant secret namespace

Each tenant's secrets live in an independent namespace to avoid cross-use:

```toml
[providers.openai]
api_key = { source = "vault", path = "secret/data/tenants/${tenant_id}/openai/api_key" }
```

`${tenant_id}` is a template variable, substituted by the SecretResolver with the tenant_id from the request context. So:
- The config file itself is tenant-agnostic, sharing a single template
- The actual secrets are physically isolated under different paths in the secret manager
- Cross-tenant secret access necessarily fails (path doesn't exist)

### 5.4 Secret caching and refresh

Secret resolution has a cost (tens of ms network round trip) and must be cached:

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

Refresh strategy:
- **Passive**: re-fetch on the next resolve after the cache TTL expires
- **Active**: OAuth refresh — on receiving 401, call `invalidate` + re-resolve immediately
- **Warm-up**: on tenant startup, pre-fetch its commonly used secrets to avoid first-request latency

**Never persisted**: secret cache lives only in memory; on process restart all entries are lost. Never written to disk / DB / Redis.

---

## 6. Configuration Hot Reload

### 6.1 Hot-reload classification

```rust
pub enum HotReloadability {
    /// Fully hot-reloadable, no runtime impact
    Hot,
    
    /// Hot-reloadable, but requires draining in-flight requests (e.g. changing routing policy)
    HotWithDrain,
    
    /// Requires restarting subprocesses (CLI / MCP server)
    SubprocessRestart,
    
    /// Requires full Runtime restart
    FullRestart,
    
    /// Never changeable (would break data integrity)
    Immutable,
}
```

Each config schema field is annotated with its hot-reload capability (via attribute):

```rust
#[derive(Config)]
pub struct CacheConfig {
    #[reload(Immutable)]                   // changing invalidates all cache
    pub hasher_version: u32,
    
    #[reload(Hot)]                         // takes effect immediately
    pub l1_capacity: u64,
    
    #[reload(HotWithDrain)]                // wait for current lookup to complete
    pub l2_url: String,
    
    #[reload(SubprocessRestart)]           // change restarts mcp server
    pub mcp_server_args: Vec<String>,
}
```

### 6.2 Hot-reload flow

```rust
pub struct ConfigManager {
    current: Arc<ArcSwap<EffectiveConfig>>,
    watchers: Vec<Arc<dyn ConfigWatcher>>,
    subscribers: broadcast::Sender<ConfigChangeEvent>,
}

impl ConfigManager {
    /// Trigger reload (sources: file change notification / DB change notification / explicit API)
    pub async fn reload(&self) -> Result<ReloadReport, ConfigError> {
        // 1. Read new config
        let new_raw = self.collect_all_layers().await?;
        let new_effective = self.merge_layers(new_raw)?;
        
        // 2. Validate
        self.validate(&new_effective)?;
        
        // 3. Diff against old config, classify changes
        let diff = self.diff(&self.current.load(), &new_effective);
        
        // 4. Check reloadability of each change
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
        
        // 5. Apply — bucket by reloadability
        let drain_tasks = diff.changes.iter()
            .filter(|c| c.reloadability() == HotReloadability::HotWithDrain)
            .map(|c| self.drain_for(c))
            .collect::<Vec<_>>();
        futures::future::join_all(drain_tasks).await;
        
        // 6. swap
        self.current.store(Arc::new(new_effective.clone()));
        
        // 7. Notify subprocess restart
        for change in &diff.changes {
            if change.reloadability() == HotReloadability::SubprocessRestart {
                self.restart_subprocess_for(change).await?;
            }
        }
        
        // 8. Broadcast
        self.subscribers.send(ConfigChangeEvent { diff }).ok();
        
        Ok(ReloadReport { applied: diff.changes.len(), warnings: vec![] })
    }
}
```

### 6.3 Sources of reload

```toml
[config_manager]
sources = ["file_watcher", "db_polling", "explicit_api"]

[config_manager.file_watcher]
paths = ["/etc/tars/", "/etc/tars/tenants/"]
debounce_ms = 500                         # coalesce file flap window

[config_manager.db_polling]
interval_secs = 30                        # tenant DB change polling
table = "tenant_configs"

[config_manager.explicit_api]
listen = "127.0.0.1:9001"                 # admin API, triggers immediate reload
```

---

## 7. Configuration Validation

### 7.1 Startup-time validation

```rust
pub fn validate_config(config: &Config) -> Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();
    
    // Schema completeness
    errors.extend(validate_schema(config));
    
    // Pipeline layer ordering constraints (Doc 02 §7)
    errors.extend(validate_pipeline_order(&config.pipeline));
    
    // Provider config: auth resolvable / models exist / capabilities consistent
    errors.extend(validate_providers(&config.providers));
    
    // Tenant reference integrity: each tenant.allowed_providers exists in providers
    errors.extend(validate_tenant_references(&config.tenants, config));
    
    // Secret references reachable (do a ping test, but don't actually fetch)
    errors.extend(validate_secret_references(&config));
    
    // Tool / MCP config: binary path exists / scope reference exists
    errors.extend(validate_tools(&config.tools));
    
    // Locked-layer override check (§2.2)
    errors.extend(validate_layer_locks(config));
    
    // PromptBuilder stability (Doc 04 §11)
    errors.extend(validate_prompt_builder_stability(&config.agents));
    
    // Cross-section consistency: model tier referenced by routing policy is reachable in providers
    errors.extend(validate_cross_section(config));
    
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
```

Startup validation **must pass entirely before startup is allowed** — half-startup is an anti-pattern (some features work, others don't, leading to inexplicable runtime errors).

### 7.2 Runtime validation

Validation triggered by hot reload is stricter: in addition to all startup checks, it must check reloadability constraints (§6.2).

### 7.3 Handling validation failures

```rust
pub enum ConfigError {
    /// Fatal: startup fails / reload fails
    Fatal(String),
    
    /// Warning: config is usable but discouraged (e.g. inline secret)
    Warning(String),
    
    /// Known incompatibility: a field deprecated when an old schema is bumped to a new version
    Deprecated { field: String, removed_in_version: ConfigVersion },
}
```

Startup-time Fatal → process exit(1) + full error list written to stderr (not just the first error).
Startup-time Warning → starts normally, all warnings listed in the startup banner.
Deprecated → starts normally, recorded to a migration TODO file (`/var/lib/tars/migration_todo.json`).

---

## 8. Tenant Lifecycle

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
    // 1. Allocate TenantId
    let tenant_id = TenantId::generate();
    
    // 2. Create isolation resources
    let isolation = TenantIsolation {
        subprocess_home: PathBuf::from(format!("/var/lib/tars/tenants/{}/home", tenant_id)),
        cache_namespace: format!("ns:{}", tenant_id),
        event_log_partition: format!("evt_{}", tenant_id),
        secret_namespace: format!("tenants/{}", tenant_id),
    };
    
    // 3. Physical initialization
    fs::create_dir_all(&isolation.subprocess_home)?;
    db.execute(&format!("CREATE TABLE IF NOT EXISTS {}_events (...)", 
        isolation.event_log_partition)).await?;
    secret_manager.create_namespace(&isolation.secret_namespace).await?;
    
    // 4. Write TenantConfig
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
    
    // 5. Trigger ConfigManager reload
    config_manager.reload().await?;
    
    // 6. Audit
    audit_log.write(AuditEvent::TenantProvisioned { 
        tenant: tenant_id, 
        by: current_principal() 
    }).await?;
    
    Ok(config)
}
```

### 8.2 Suspend / Resume

Suspend doesn't delete data, only blocks new requests:

```rust
pub async fn suspend_tenant(tenant: &TenantId, reason: String) -> Result<(), SuspendError> {
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::Suspended { 
        since: SystemTime::now(), 
        reason: reason.clone(),
    };
    db.update_tenant_config(&config).await?;
    
    // 1. Immediately reject new requests for this tenant (Pipeline IAM layer checks status)
    config_manager.reload().await?;
    
    // 2. Gracefully drain in-flight requests (per deadline)
    runtime.drain_tenant(tenant, Duration::from_secs(60)).await;
    
    // 3. Proactively purge L3 cache (avoid continuing to accumulate storage charges)
    cache_janitor.purge_tenant(tenant).await?;
    
    // 4. Kill this tenant's MCP / CLI subprocesses
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

Delete is irreversible, **two-phase commit**:

```rust
pub async fn schedule_deletion(
    tenant: &TenantId, 
    delay: Duration,
) -> Result<DeletionHandle, DeleteError> {
    // Phase 1: mark PendingDeletion, defer the real delete by N days (default 30)
    // During this window the data still exists, abort_deletion can revert it
    
    let mut config = db.load_tenant(tenant).await?;
    config.status = TenantStatus::PendingDeletion {
        scheduled_for: SystemTime::now() + delay,
    };
    db.update_tenant_config(&config).await?;
    
    // The tenant enters suspended state (no longer usable)
    suspend_tenant(tenant, "pending_deletion".into()).await?;
    
    // Register a scheduled task to fire at scheduled_for
    scheduler.schedule_at(SystemTime::now() + delay, 
        Box::new(move || actually_delete(tenant.clone()))).await?;
    
    Ok(DeletionHandle { tenant: tenant.clone(), scheduled_for: ... })
}

async fn actually_delete(tenant: TenantId) -> Result<(), DeleteError> {
    // Phase 2: cascade delete
    
    // 1. Abort any trajectories that may still be running
    runtime.abort_tenant(&tenant).await?;
    
    // 2. Delete event log (drop by partition)
    db.execute(&format!("DROP TABLE {}_events", 
        config.isolation.event_log_partition)).await?;
    
    // 3. Delete ContentStore objects (by tenant prefix)
    content_store.purge_tenant(&tenant).await?;
    
    // 4. Delete cache (L2 Redis: prefix scan + delete; L3: list + delete)
    cache_registry.invalidate_tenant(&tenant).await?;
    
    // 5. Delete budget store history
    budget_store.purge_tenant(&tenant).await?;
    
    // 6. Delete subprocess HOME directory
    fs::remove_dir_all(&config.isolation.subprocess_home)?;
    
    // 7. Delete secret namespace
    secret_manager.delete_namespace(&config.isolation.secret_namespace).await?;
    
    // 8. Delete tenant config (last step)
    db.delete_tenant_config(&tenant).await?;
    
    // 9. Write tamper-proof audit record
    audit_log.write(AuditEvent::TenantDeleted { 
        tenant: tenant.clone(),
        deleted_at: SystemTime::now(),
        completed_steps: vec!["events", "content", "cache", "budget", "fs", "secrets", "config"],
    }).await?;
    
    Ok(())
}
```

**Key invariants**:
- If any step between phase 1 and phase 2 fails, the entire deletion is aborted + alerted
- The 9 steps in phase 2 must execute in order; on failure stop there (don't keep deleting blindly)
- Each step must emit an "X objects deleted" metric for audit verification
- audit records are **never deleted**, even after the tenant itself is deleted

---

## 9. Quotas and Billing

### 9.1 Quota model

```rust
pub struct TenantQuotas {
    /// Rate limits (hard caps, exceeding triggers 429)
    pub max_rpm: u32,                      // requests per minute
    pub max_tpm: u64,                      // input+output tokens per minute
    pub max_concurrent_tasks: u32,         // concurrently running trajectories
    pub max_subprocess_count: u32,         // total CLI + MCP subprocess cap
    
    /// Capacity limits
    pub max_l3_storage_bytes: u64,
    pub max_event_log_size_bytes: u64,
    
    /// Cost caps
    pub daily_cost_usd_soft: f64,          // triggers alert
    pub daily_cost_usd_hard: f64,          // triggers circuit break
    pub monthly_cost_usd_hard: f64,
    
    /// Tool/Skill call frequency caps
    pub max_tool_calls_per_day: HashMap<ToolId, u64>,
}
```

### 9.2 Billing data flow

```
Each LLM call / Tool call completes
       │
       ▼
Telemetry (Doc 02 §4.1) extracts usage + computes cost
       │
       ▼
BudgetStore::commit (Redis atomic decrement)
       │
       ▼
Async dual-write:
  ├─→ Billing log (PostgreSQL `billing_events` table) - per-event auditable
  └─→ Aggregation service - real-time aggregation to hour/day/month dimensions
       │
       ▼
Triggers:
  - exceeds soft threshold → alert (Slack / email)
  - exceeds hard threshold → circuit break (BudgetMiddleware rejects)
  - month-end close → export CSV/JSON to billing system
```

### 9.3 Billing report export

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
    StripeWebhook,                         // push directly to Stripe metered billing
    InternalKafka { topic: String },
}
```

Report contents:
- Aggregate token / cost / call count by tenant
- Breakdown by model / tool
- Time series by day
- Separately list cache savings (Doc 03 §12 `cache.l3.cost_saved_usd`)

---

## 10. Audit and Compliance

### 10.1 Tamper-proof audit log

```rust
pub enum AuditEvent {
    // Tenant lifecycle
    TenantProvisioned { tenant: TenantId, by: Principal },
    TenantSuspended { tenant: TenantId, reason: String },
    TenantResumed { tenant: TenantId },
    TenantDeleted { tenant: TenantId, deleted_at: SystemTime, completed_steps: Vec<String> },
    
    // Configuration changes
    ConfigReloaded { changes: Vec<ConfigChange>, by: Principal },
    ConfigReloadRejected { reason: String, by: Principal },
    
    // Security events
    IamDenied { principal: Principal, resource: ResourceRef, reason: String },
    SecurityAlert { kind: String, details: serde_json::Value },
    CompensationFailed { trajectory: TrajectoryId, compensation: CompensationId, error: String },
    
    // Data access
    SecretAccessed { ref: SecretRef, by: Principal },
    
    // Billing events
    BudgetSoftLimitExceeded { tenant: TenantId, period: String, amount: f64 },
    BudgetHardLimitExceeded { tenant: TenantId, period: String, amount: f64 },
}

#[async_trait]
pub trait AuditLog: Send + Sync {
    async fn write(&self, event: AuditEvent) -> Result<AuditRef, AuditError>;
}
```

**Implementation requirements**:
- Write to append-only storage (Postgres + immutable column / WORM S3 / blockchain)
- Sign every event (HMAC with rotated key)
- Asynchronous dual-write to an external SIEM (Splunk / Datadog / ELK)
- Even after the tenant is deleted, audit records are retained for 7 years (compliance requirement)

### 10.2 GDPR compliance

- **Right to portability**: `export_tenant_data` API exports all that tenant's events / cache keys (excluding LLM response content, since it is derivative) / billing
- **Right to be forgotten**: §8.3's 30-day delay + cascade delete
- **Data localization**: Provider config can specify region; tenant config selects providers in the corresponding region (e.g. EU tenants can only use Anthropic / Gemini in EU regions)

```toml
[providers.claude_eu]
type = "anthropic"
base_url = "https://api.anthropic.com"     # Anthropic has no explicit EU endpoint, but routing via VPC works
region = "eu-west-1"
data_residency = "EU"

[tenants.eu_customer_acme]
allowed_providers = ["claude_eu", "gemini_eu"]
data_residency_required = "EU"             # enforces only providers tagged EU may be used
```

---

## 11. Configuration Shape Summary

The full schema spans all preceding docs; here is a minimal working example:

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
hasher_version = 1                         # locked item

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

# === Doc 06 (this doc) ===
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

# === Tenants ===
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

## 12. Testing Strategy

### 12.1 Configuration schema tests

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
    config.pipeline.order = vec!["cache_lookup".into(), "iam".into()];  // IAM after cache!
    
    let errors = validate_config(&config).unwrap_err();
    assert!(errors.iter().any(|e| matches!(e, ConfigError::Fatal(s) if s.contains("iam"))));
}

#[test]
fn locked_key_override_rejected() {
    let mut config = test_config();
    config.tenants.insert("evil".into(), TenantConfig {
        overrides: TenantOverrides {
            cache: Some(CacheOverrides {
                hasher_version: Some(99),    // attempt to override a locked item
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

### 12.2 Tenant isolation end-to-end tests

```rust
#[tokio::test]
async fn tenant_a_cache_does_not_leak_to_tenant_b() {
    let runtime = test_runtime_with_two_tenants("a", "b").await;
    
    // Tenant A makes a call with the same prompt, populating cache
    let req = test_request_with_tenant("a");
    runtime.execute(req.clone()).await.unwrap();
    
    // Tenant B makes a call with the same prompt
    let req_b = ChatRequest { tenant_id: "b".into(), ..req };
    let stats_before = mock_provider.invocation_count();
    runtime.execute(req_b).await.unwrap();
    let stats_after = mock_provider.invocation_count();
    
    // B must actually hit the provider (cannot hit A's cache)
    assert_eq!(stats_after - stats_before, 1);
}

#[tokio::test]
async fn deleted_tenant_data_completely_purged() {
    let runtime = test_runtime();
    let tenant = provision_test_tenant(&runtime).await;
    
    // Create some data
    create_trajectories(&runtime, &tenant, 10).await;
    create_cache_entries(&runtime, &tenant, 50).await;
    
    schedule_deletion(&tenant, Duration::ZERO).await.unwrap();
    actually_delete(&tenant).await.unwrap();
    
    // Verify all resources are gone
    assert!(db.tenant_exists(&tenant).await.unwrap() == false);
    assert!(content_store.tenant_object_count(&tenant).await.unwrap() == 0);
    assert!(cache_registry.tenant_key_count(&tenant).await.unwrap() == 0);
    assert!(!fs::exists(&format!("/var/lib/tars/tenants/{}", tenant)));
    
    // But the audit log must remain
    let audit_records = audit_log.query_for_tenant(&tenant).await.unwrap();
    assert!(audit_records.iter().any(|r| matches!(r.event, AuditEvent::TenantDeleted { .. })));
}
```

### 12.3 Hot-reload tests

```rust
#[tokio::test]
async fn budget_change_hot_reloads() {
    let manager = test_config_manager();
    
    // Initial budget 100
    assert_eq!(manager.current().middleware.budget.default_daily_cost_usd, 100.0);
    
    // Modify the config file
    update_config_file(|c| { c.middleware.budget.default_daily_cost_usd = 200.0 });
    manager.reload().await.unwrap();
    
    // Takes effect immediately
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

### 12.4 Secret tests

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
    
    // Different tenants get different secrets
    assert_ne!(key_a, key_b);
}

#[tokio::test]
async fn cross_tenant_secret_access_rejected() {
    // Tenant A's code attempts to read tenant B's secret
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

## 13. Anti-pattern Checklist

1. **Don't write plaintext secrets in config files** — always use SecretRef.
2. **Don't let tenant config override locked security constraints** — cache hasher_version, pipeline ordering, tool side_effect classification, etc.
3. **Don't silently accept partially loaded config** — validation failure = startup failure / reload failure, no half-broken running.
4. **Don't assume all config is hot-reloadable** — annotate reloadability explicitly on the schema.
5. **Don't skip isolation resource initialization on Tenant Provision** — if subprocess_home doesn't exist, the first MCP call crashes.
6. **Don't miss any data store on Tenant Delete** — cascade delete must cover events / cache / content / budget / fs / secret — all 7 categories.
7. **Don't put audit logs in the same store as business data** — must have an independent write path so audit can still write when the business DB is down.
8. **Don't let Tenant Suspend immediately affect in-flight requests** — Drain window defaults to 60s, allow already-started work to complete.
9. **Don't persist values in the secret cache** — memory only, lost on process restart.
10. **Don't let the Configuration type hold mutable global state** — atomic swap via ArcSwap, lock-free read path.
11. **Don't delay notification when quotas trigger circuit-break** — alerts must be real-time; finding out after the fact means the money is already lost.
12. **Don't allow deletion events to be overwritten or edited** — audit log is append-only, even admins can't modify.
13. **Don't mix system / tenant config** — tenant overrides are the explicit `overrides: TenantOverrides`, not direct mutation of the global section.
14. **Don't let tenant_id strings be freely generated** — must go through `TenantId::generate()`; user-supplied IDs are forbidden (collision-prone / injection attacks).
15. **Don't silently ignore deprecated fields** — record them in migration_todo, surface to ops.

---

## 14. Contracts with Upstream and Downstream

### Upstream (Application / Frontend Adapter) commitments

- What you get from `ConfigManager::current()` is an already merged / validated EffectiveConfig
- All tenant switching is conveyed through RequestContext.tenant_id, never via global variables
- Don't directly access a tenant's isolation paths (subprocess_home / cache_namespace); go through the corresponding trait interfaces (SubprocessManager / CacheRegistry)

### Downstream contracts (each Doc 01-05 component)

- Receive EffectiveConfig as a constructor argument; don't read files yourself
- Implement the `ConfigSubscriber` trait to listen to changes:
  ```rust
  #[async_trait]
  pub trait ConfigSubscriber: Send + Sync {
      fn interested_in(&self) -> Vec<ConfigKeyPattern>;
      async fn on_change(&self, change: &ConfigChange) -> Result<(), SubscriberError>;
  }
  ```
- Don't cache resolved secrets longer than SecretRef.cache_ttl
- On Tenant Suspend / Delete, clean up owned resources (subprocess kill / cache purge / etc.)

### Cross-node contracts

In multi-node deployments:
- Each node has an independent `ConfigManager`, synchronized via file watcher / DB polling
- Config changes don't require simultaneous activation across all nodes, but must be eventually consistent
- Nodes don't communicate config changes directly — the filesystem / DB is the intermediary
- audit_log must be replicated to centralized storage (local + Splunk); a node going down loses nothing

---

## 15. TODOs and Open Questions

- [ ] Which DSL for the configuration schema: TOML + serde / Cue / Pkl / Dhall
- [ ] Multi-region deployment config sync strategy (push vs pull / DB region routing)
- [ ] Per-tenant encryption (encryption at rest with tenant-specific keys, for financial compliance)
- [ ] How to technically guarantee audit log immutability (HMAC vs WORM vs blockchain)
- [ ] Tenant ID naming rules and readability (UUID vs short_id vs human-readable)
- [ ] Schema and API for the per-tenant quota visualization dashboard
- [ ] Configuration migration tooling (automatic v1 → v2 conversion + dry-run)
- [ ] Automation of Secret rotation (calendar-based vs event-driven)
- [ ] Enforcement of multi-region tenant data residency (startup-time vs runtime validation)
- [ ] Whether Workspace needs an independent quota / IAM sub-layer (this doc currently says no — tenant is sufficient)
