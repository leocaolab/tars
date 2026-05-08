# 文档 09 — 数据持久化与 Storage Schema

> 范围：定义所有持久化数据的存储介质、表结构、索引、分区、迁移、备份策略。
>
> 上下文：贯穿前面 Doc 01-08——本文档不引入新业务概念，只把散落的存储约束统一规范。
>
> 适配两种部署形态：Personal (SQLite) 与 Team/SaaS/Hybrid (Postgres + Redis + S3)。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **同 schema 跨存储** | SQLite 与 Postgres 表结构尽量同型,业务代码同一份 |
| **Append-only 优先** | 事件 / 审计 / 计费三大表 INSERT-only,不允许 UPDATE / DELETE 业务路径 |
| **租户级分区** | 大表按 tenant_id 物理分区,删除租户时 DROP PARTITION 秒级完成 |
| **冷热分层** | 热数据 (近 30 天) 在 OLTP,冷数据归档到 S3,查询透明 |
| **零停机迁移** | Schema migration 必须支持滚动升级,新老版本能并存 |
| **Backup 可恢复** | 任何时间点的状态都能通过备份恢复,RPO < 1h,RTO < 4h |
| **存储与业务解耦** | 所有 schema 通过 Repository trait 暴露,业务代码不写 SQL |

**反目标**：
- 不在业务路径上做跨表 transaction（性能瓶颈，错误恢复复杂）
- 不让业务依赖某个特定 SQL 方言（Postgres 特有 feature 通过 Repository 屏蔽）
- 不为了"性能"反规范化导致同一数据多处更新
- 不在 SQLite 里做并发热写（Personal 模式预期单进程，多进程会冲突）

---

## 2. 存储介质矩阵

按部署形态分配：

| 数据类型 | Personal | Team / SaaS | Hybrid |
|---|---|---|---|
| 事件日志 | SQLite (WAL mode) | Postgres (partitioned) | SQLite (本地) |
| 审计日志 | SQLite (单独 file) | Postgres + S3 mirror + SIEM | SQLite + 不上传 |
| 计费事件 | SQLite | Postgres | SQLite (本地汇总→上传匿名 metric) |
| 租户配置 | TOML 文件 (无 DB) | Postgres | TOML + 云端 sync |
| L1 Cache | 进程内内存 | 进程内内存 | 进程内内存 |
| L2 Cache | SQLite blob | Redis Cluster | SQLite blob |
| L3 Cache 索引 | SQLite | Postgres | SQLite |
| Budget 状态 | SQLite (单进程) | Redis (原子扣减) | SQLite |
| Idempotency Cache | SQLite | Redis | SQLite |
| Session 状态 | SQLite | Redis | SQLite |
| 大 Content (ContentRef) | 本地 FS | S3 | 本地 FS |
| Secret 引用 (无值) | TOML | Postgres | TOML |
| Secret 值 | OS keychain / file | Vault / KMS | OS keychain |

### 2.1 选型理由速览

- **SQLite + WAL**：Personal 模式单进程，无运维负担，性能足够（<1k events/s 完全够用）
- **Postgres**：Team/SaaS 多副本必备，事务能力强，分区成熟
- **Redis**：原子扣减 + pub/sub + 高 QPS——Budget 和 Cache L2 的最佳形态
- **S3**：大对象 + 冷归档 + 跨区复制
- **OS keychain / Vault**：secret 值绝不与业务 DB 同地存放

---

## 3. Postgres Schema (Team / SaaS)

### 3.1 总览

```
public schema (核心业务):
  tenants
  workspaces
  sessions
  iam_scope_assignments
  
  trajectories                       (按 tenant_id 分区)
  agent_events                       (按 tenant_id 分区,内部按月再分)
  content_refs
  
  l3_cache_handles
  l3_handle_sessions                 (引用关系)
  
  idempotency_cache                  (按 tenant_id 分区)
  
  pending_compensations
  compensation_failures              (告警源)
  
audit schema (合规):
  audit_events                       (按时间分区,append-only)
  audit_signatures                   (HMAC,与 event 1:1)
  
billing schema (计费):
  billing_events                     (按月分区)
  billing_aggregations_hourly
  billing_aggregations_daily
  billing_aggregations_monthly
  
config schema (配置):
  tenant_configs                     (JSONB)
  config_change_log                  (append-only,谁改了什么)
  secret_refs                        (引用,不是值)
  
ops schema (运维元数据):
  schema_migrations                  (refinery / sqlx::migrate!)
  cardinality_observations           (Doc 08 §5.5)
  reconciliation_runs                (Doc 03 §7.3)
```

### 3.2 关键表 DDL

#### 3.2.1 tenants

```sql
CREATE TABLE tenants (
    id              UUID PRIMARY KEY,
    display_name    TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('active', 'suspended', 'pending_deletion', 'deleted')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    suspended_at    TIMESTAMPTZ,
    suspend_reason  TEXT,
    deletion_scheduled_for  TIMESTAMPTZ,
    deleted_at      TIMESTAMPTZ,
    isolation_subprocess_home  TEXT NOT NULL,
    isolation_cache_namespace  TEXT NOT NULL UNIQUE,
    isolation_event_partition  TEXT NOT NULL UNIQUE,
    isolation_secret_namespace TEXT NOT NULL UNIQUE
);

CREATE INDEX idx_tenants_status ON tenants(status) WHERE status != 'deleted';
CREATE INDEX idx_tenants_pending_deletion ON tenants(deletion_scheduled_for) 
    WHERE status = 'pending_deletion';
```

#### 3.2.2 trajectories (按 tenant 分区)

```sql
CREATE TABLE trajectories (
    id                  UUID NOT NULL,
    tenant_id           UUID NOT NULL,
    root_task_id        UUID NOT NULL,
    parent_id           UUID,
    branch_reason       JSONB NOT NULL,
    status              TEXT NOT NULL CHECK (status IN ('active', 'suspended', 'completed', 'dead')),
    head_offset         BIGINT NOT NULL DEFAULT 0,
    pending_compensations JSONB NOT NULL DEFAULT '[]',
    budget_remaining    JSONB NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, id)
) PARTITION BY HASH (tenant_id);

-- 创建分区 (典型部署 64 个 hash partition)
DO $$
BEGIN
    FOR i IN 0..63 LOOP
        EXECUTE format(
            'CREATE TABLE trajectories_p%s PARTITION OF trajectories
             FOR VALUES WITH (modulus 64, remainder %s)',
            i, i
        );
    END LOOP;
END $$;

-- 索引
CREATE INDEX idx_trajectories_active 
    ON trajectories (tenant_id, status, updated_at) 
    WHERE status IN ('active', 'suspended');
CREATE INDEX idx_trajectories_root_task 
    ON trajectories (tenant_id, root_task_id);
CREATE INDEX idx_trajectories_parent 
    ON trajectories (tenant_id, parent_id) 
    WHERE parent_id IS NOT NULL;
```

#### 3.2.3 agent_events (按 tenant + 时间分区)

最大表，预期 10⁶-10⁹ 行。双层分区：tenant_id (HASH) → created_at (RANGE 按月)：

```sql
CREATE TABLE agent_events (
    id              BIGSERIAL,
    tenant_id       UUID NOT NULL,
    trajectory_id   UUID NOT NULL,
    step_seq        INT,                  -- nullable: 非 step 事件没有
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    content_ref     BYTEA,                -- 大 payload 通过 content_refs 间接引用
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, created_at, id)
) PARTITION BY HASH (tenant_id);

DO $$
BEGIN
    FOR i IN 0..63 LOOP
        EXECUTE format(
            'CREATE TABLE agent_events_p%s PARTITION OF agent_events
             FOR VALUES WITH (modulus 64, remainder %s)
             PARTITION BY RANGE (created_at)',
            i, i
        );
    END LOOP;
END $$;

-- 每个 hash partition 内部再按月分区,通过 pg_partman 自动维护:
-- SELECT partman.create_parent('public.agent_events_p0', 'created_at', 'native', 'monthly');

CREATE INDEX idx_agent_events_trajectory 
    ON agent_events (tenant_id, trajectory_id, created_at);
CREATE INDEX idx_agent_events_type 
    ON agent_events (tenant_id, event_type, created_at);

-- BRIN 索引适合 append-only 时间序列,体积比 B-tree 小 100x
CREATE INDEX idx_agent_events_brin 
    ON agent_events USING BRIN (created_at);
```

**冷归档**：超过 30 天的 month partition 通过 `pg_dump` → S3，然后 `DROP PARTITION`。归档后查询走 §7 的冷热混合路径。

#### 3.2.4 content_refs

```sql
CREATE TABLE content_refs (
    hash            BYTEA PRIMARY KEY,        -- SHA-256, 32 bytes
    tenant_id       UUID NOT NULL,
    size_bytes      BIGINT NOT NULL,
    mime_type       TEXT,
    backend         TEXT NOT NULL CHECK (backend IN ('inline', 's3', 'fs')),
    inline_data     BYTEA,                    -- 仅 backend = 'inline' 时
    s3_key          TEXT,                     -- 仅 backend = 's3' 时
    fs_path         TEXT,                     -- 仅 backend = 'fs' 时
    refcount        INT NOT NULL DEFAULT 0,   -- 引用计数,GC 用
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_content_refs_tenant ON content_refs(tenant_id);
CREATE INDEX idx_content_refs_zero_refcount ON content_refs(refcount) WHERE refcount = 0;
CREATE INDEX idx_content_refs_unaccessed ON content_refs(last_accessed_at) 
    WHERE refcount = 0;

-- 自动选择 backend 的策略 (在应用层):
-- size <= 16KB    → inline
-- size <= 1MB     → fs (本地)
-- size > 1MB      → s3
```

#### 3.2.5 l3_cache_handles + 引用计数

```sql
CREATE TABLE l3_cache_handles (
    id              UUID PRIMARY KEY,
    tenant_id       UUID NOT NULL,
    cache_key_hash  BYTEA NOT NULL,           -- Doc 03 §3 的 fingerprint
    provider        TEXT NOT NULL,
    external_id     TEXT NOT NULL,            -- Provider 侧的 cache id
    size_estimate_bytes BIGINT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at      TIMESTAMPTZ NOT NULL,
    last_used_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    usage_count     BIGINT NOT NULL DEFAULT 0,
    pending_eviction_since TIMESTAMPTZ,
    UNIQUE (tenant_id, cache_key_hash, provider)
);

CREATE INDEX idx_l3_handles_eviction_candidates 
    ON l3_cache_handles (pending_eviction_since) 
    WHERE pending_eviction_since IS NOT NULL;
CREATE INDEX idx_l3_handles_expired 
    ON l3_cache_handles (expires_at) 
    WHERE expires_at < NOW();

-- 引用关系表 (Doc 03 §7.2)
CREATE TABLE l3_handle_sessions (
    handle_id       UUID NOT NULL REFERENCES l3_cache_handles(id) ON DELETE CASCADE,
    session_id      UUID NOT NULL,
    referenced_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (handle_id, session_id)
);

CREATE INDEX idx_l3_handle_sessions_session ON l3_handle_sessions(session_id);
```

#### 3.2.6 audit_events (按时间分区)

```sql
CREATE TABLE audit.audit_events (
    id              BIGSERIAL,
    tenant_id       UUID,                     -- nullable: 系统级事件 (config reload)
    principal_id    TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    severity        TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    payload         JSONB NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (occurred_at, id)
) PARTITION BY RANGE (occurred_at);

-- 月分区,7 年保留
SELECT partman.create_parent(
    'audit.audit_events', 
    'occurred_at', 
    'native', 
    'monthly',
    p_premake := 12,
    p_retention := '7 years',
    p_retention_keep_table := false  -- 真实删除 (合规允许后)
);

CREATE INDEX idx_audit_tenant ON audit.audit_events(tenant_id, occurred_at) 
    WHERE tenant_id IS NOT NULL;
CREATE INDEX idx_audit_type ON audit.audit_events(event_type, occurred_at);
CREATE INDEX idx_audit_critical ON audit.audit_events(occurred_at) 
    WHERE severity = 'critical';

-- HMAC 签名表 (与 event 1:1, 防篡改)
CREATE TABLE audit.audit_signatures (
    event_id        BIGINT NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL,
    hmac_sha256     BYTEA NOT NULL,
    signing_key_id  TEXT NOT NULL,
    PRIMARY KEY (occurred_at, event_id),
    FOREIGN KEY (occurred_at, event_id) REFERENCES audit.audit_events(occurred_at, id)
);
```

**关键不变量**：
- `audit_events` 没有 UPDATE / DELETE 权限——通过 Postgres role 强制
- 即使 superuser 也只能通过特殊审批流程删除（合规要求）
- `audit_signatures` 在 event 写入后立即填充，HMAC key 定期轮换

```sql
-- 强制 append-only
REVOKE UPDATE, DELETE, TRUNCATE ON audit.audit_events FROM application_role;
GRANT INSERT, SELECT ON audit.audit_events TO application_role;
```

#### 3.2.7 billing_events

```sql
CREATE TABLE billing.billing_events (
    id              BIGSERIAL,
    tenant_id       UUID NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    event_kind      TEXT NOT NULL CHECK (event_kind IN 
                    ('llm_call', 'tool_call', 'l3_cache_storage', 'l3_cache_hit_savings')),
    provider        TEXT,
    model           TEXT,
    tool_id         TEXT,
    tokens_input    BIGINT,
    tokens_output   BIGINT,
    tokens_cached   BIGINT,
    cost_usd        NUMERIC(12, 6) NOT NULL,
    duration_ms     INT,
    trace_id        TEXT,
    PRIMARY KEY (occurred_at, id)
) PARTITION BY RANGE (occurred_at);

SELECT partman.create_parent(
    'billing.billing_events', 
    'occurred_at', 
    'native', 
    'monthly',
    p_retention := '3 years'
);

CREATE INDEX idx_billing_tenant_time ON billing.billing_events(tenant_id, occurred_at);
CREATE INDEX idx_billing_kind ON billing.billing_events(event_kind, occurred_at);

-- 预聚合 (定时 job 写入,加速 dashboard 查询)
CREATE TABLE billing.aggregations_daily (
    tenant_id       UUID NOT NULL,
    bucket_date     DATE NOT NULL,
    event_kind      TEXT NOT NULL,
    provider        TEXT,
    model           TEXT,
    request_count   BIGINT NOT NULL,
    tokens_input    BIGINT NOT NULL,
    tokens_output   BIGINT NOT NULL,
    cost_usd        NUMERIC(12, 6) NOT NULL,
    PRIMARY KEY (tenant_id, bucket_date, event_kind, COALESCE(provider, ''), COALESCE(model, ''))
);

CREATE INDEX idx_billing_daily_tenant ON billing.aggregations_daily(tenant_id, bucket_date);
```

#### 3.2.8 tenant_configs

```sql
CREATE TABLE config.tenant_configs (
    tenant_id       UUID PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    overrides       JSONB NOT NULL DEFAULT '{}',
    quotas          JSONB NOT NULL,
    allowed_providers TEXT[] NOT NULL DEFAULT '{}',
    allowed_tools     TEXT[] NOT NULL DEFAULT '{}',
    allowed_skills    TEXT[] NOT NULL DEFAULT '{}',
    allowed_mcp_servers TEXT[] NOT NULL DEFAULT '{}',
    schema_version  INT NOT NULL DEFAULT 1,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by      TEXT NOT NULL
);

-- 配置变更历史 (append-only)
CREATE TABLE config.config_change_log (
    id              BIGSERIAL PRIMARY KEY,
    tenant_id       UUID,                     -- NULL 表示全局配置变更
    changed_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    changed_by      TEXT NOT NULL,
    change_kind     TEXT NOT NULL,
    diff            JSONB NOT NULL,           -- 变更前后的 patch
    reason          TEXT
);

CREATE INDEX idx_config_log_tenant ON config.config_change_log(tenant_id, changed_at);
```

---

## 4. SQLite Schema (Personal)

SQLite 模式的 schema 与 Postgres 大致同型，但有几点关键差异：

### 4.1 模式差异

| 维度 | Postgres | SQLite (Personal) |
|---|---|---|
| 数据类型 | UUID / TIMESTAMPTZ / JSONB / BYTEA | TEXT (UUID 字符串) / INTEGER (unix epoch) / TEXT (JSON 字符串) / BLOB |
| 分区 | HASH + RANGE | 无 (单租户量小,不需要) |
| 并发 | MVCC | WAL mode + busy_timeout |
| 外键 | 自动启用 | 必须 `PRAGMA foreign_keys = ON` |
| Schema 隔离 | schema namespace | 单 file 平铺 |

### 4.2 SQLite 启动配置

```rust
async fn init_sqlite(path: &Path) -> Result<Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_init(|conn| {
            // 关键 PRAGMA
            conn.execute_batch("
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;       -- WAL 下足够安全
                PRAGMA foreign_keys = ON;
                PRAGMA busy_timeout = 5000;        -- 5s 等锁
                PRAGMA cache_size = -32000;        -- 32MB 内存缓存
                PRAGMA temp_store = MEMORY;
                PRAGMA mmap_size = 268435456;      -- 256MB mmap
            ")?;
            Ok(())
        });
    
    let pool = Pool::builder()
        .max_size(8)                              // Personal 模式低并发
        .build(manager)?;
    
    Ok(pool)
}
```

### 4.3 SQLite agent_events (示例,与 Postgres 对应)

```sql
CREATE TABLE agent_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    trajectory_id   TEXT NOT NULL,
    step_seq        INTEGER,
    event_type      TEXT NOT NULL,
    payload         TEXT NOT NULL,            -- JSON as TEXT
    content_ref     BLOB,
    created_at      INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))
);

CREATE INDEX idx_agent_events_trajectory ON agent_events(trajectory_id, created_at);
CREATE INDEX idx_agent_events_type ON agent_events(event_type, created_at);
```

注意：SQLite 没有 tenant_id 字段（Personal 永远 single-tenant，hardcode 为 `local`）。

### 4.4 SQLite 与 Postgres 共享代码

通过 `sqlx` 的 `Database` 抽象，业务代码用 trait 屏蔽：

```rust
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, event: AgentEvent) -> Result<EventOffset, StoreError>;
    async fn fetch(&self, traj: TrajectoryId, since: EventOffset) -> Result<Vec<AgentEvent>, StoreError>;
}

pub struct SqliteEventStore { pool: Pool<SqliteConnectionManager> }
pub struct PostgresEventStore { pool: PgPool }

#[async_trait] impl EventStore for SqliteEventStore { ... }
#[async_trait] impl EventStore for PostgresEventStore { ... }
```

启动时根据 `mode` 配置选择实现：

```rust
let event_store: Arc<dyn EventStore> = match config.storage.backend {
    StorageBackend::Sqlite => Arc::new(SqliteEventStore::new(...).await?),
    StorageBackend::Postgres => Arc::new(PostgresEventStore::new(...).await?),
};
```

---

## 5. Redis 数据结构 (Team / SaaS)

### 5.1 Key 命名规范

```
{purpose}:{tenant}:{rest}

cache:l2:{tenant}:{key_hash}              → JSON blob (cached LLM response)
cache:l2:meta:{tenant}:{key_hash}         → meta hash (model, cost_saved, etc.)

budget:tpm:{tenant}                        → token bucket (Lua atomic decr)
budget:rpm:{tenant}                        → request bucket
budget:cost:daily:{tenant}:{date}          → counter (USD)

idem:{tenant}:{trajectory_id}:{step}       → tool result cache

session:{session_id}                       → hash (workspace, principal, last_seen)
session:tenant_active:{tenant}             → set of active session_ids

inflight:singleflight:{cache_key_hash}     → broadcaster channel proxy
                                             (短生命周期, 由 leader 持有)

invalidation_channel                       → pub/sub channel (cross-instance)
```

### 5.2 Budget 原子扣减 (Lua)

不能用 GET + 业务判断 + SET 的非原子模式（race condition）：

```lua
-- KEYS[1] = budget key (e.g., "budget:cost:daily:acme:2026-05-02")
-- ARGV[1] = amount to deduct
-- ARGV[2] = hard limit
-- 返回: { remaining, granted (1/0) }

local current = redis.call('GET', KEYS[1])
current = tonumber(current) or 0
local amount = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])

if current + amount > limit then
    return { current, 0 }
end

local new_value = redis.call('INCRBYFLOAT', KEYS[1], amount)

-- 第一次创建时设置 TTL (24h, 跨日自动重置)
if current == 0 then
    redis.call('EXPIRE', KEYS[1], 86400)
end

return { new_value, 1 }
```

应用层调用：

```rust
async fn try_deduct(&self, key: &str, amount: f64, limit: f64) 
    -> Result<DeductResult, BudgetError> 
{
    let result: Vec<f64> = self.script
        .key(key)
        .arg(amount)
        .arg(limit)
        .invoke_async(&mut self.conn).await?;
    
    Ok(DeductResult {
        remaining: result[0],
        granted: result[1] == 1.0,
    })
}
```

### 5.3 失效广播 (pub/sub)

```rust
// 每个实例启动时订阅
async fn subscribe_invalidation(redis: &RedisClient, l1: Arc<L1Cache>) {
    let mut sub = redis.subscribe("cache:invalidation").await.unwrap();
    while let Some(msg) = sub.next().await {
        if let Ok(event) = serde_json::from_str::<InvalidationEvent>(&msg.payload) {
            l1.apply_invalidation(&event).await;
        }
    }
}

// 写入端
async fn invalidate(&self, key: &CacheKey) {
    let event = InvalidationEvent::Key(key.clone());
    let payload = serde_json::to_string(&event).unwrap();
    self.redis.publish("cache:invalidation", payload).await.ok();
    
    // 本地立即应用
    self.local_l1.remove(key).await;
    self.l2.delete(key).await.ok();
}
```

### 5.4 Redis 配置建议

```toml
[storage.redis]
url = { source = "vault", path = "secret/data/tars/redis" }
pool_size = 50
connection_timeout_ms = 1000
command_timeout_ms = 500          # 短超时,失败时 fallback 不阻塞业务

# 持久化策略
# AOF everysec: 最多丢 1s 数据,适合 budget/cache (允许少量丢失)
# RDB 备份每小时一次,跨可用区复制
```

**关键决策**：
- Redis 不是真理之源——budget 状态丢了重新累计就行（最多让用户多用几秒），cache 丢了重新生成
- 不要把 audit / billing / event 放 Redis（持久性不够）
- Redis Cluster 用 hashtag (`{tenant_id}`) 让同租户的 key 落在同一 slot

---

## 6. S3 / Object Storage 布局

### 6.1 Bucket 结构

```
s3://tars-{env}-content/
  ├─ tenants/{tenant_id}/
  │  ├─ content/{hash[0:2]}/{hash[2:32]}      → ContentRef 大对象
  │  └─ debug-snapshots/{date}/{trajectory_id}.json  → 用户主动 export
  └─ shared/
     └─ models/                                → 嵌入式 ONNX / GGUF (跨租户共享)

s3://tars-{env}-archive/
  ├─ events/{tenant_id}/{year}/{month}/events.jsonl.zstd  → 冷归档事件
  ├─ audit/{year}/{month}/audit.jsonl.zstd               → 审计冷归档 (7 年)
  └─ billing/{year}/{month}/billing.jsonl.zstd           → 计费冷归档 (3 年)

s3://tars-{env}-backup/
  ├─ postgres/{date}/{cluster}.dump
  ├─ redis/{date}/redis.rdb
  └─ config/{date}/config-snapshot.tar.gz
```

### 6.2 ContentRef S3 操作

```rust
async fn put_content(&self, content: Vec<u8>, tenant: &TenantId) -> Result<ContentRef> {
    let hash = sha256(&content);
    let hash_hex = hex::encode(&hash);
    let key = format!("tenants/{}/content/{}/{}", 
        tenant, &hash_hex[..2], &hash_hex[2..]);
    
    // 内容寻址:已存在则跳过上传
    if self.s3.head_object(&self.bucket, &key).await.is_ok() {
        return Ok(ContentRef { hash, size: content.len() as u64, ..Default::default() });
    }
    
    self.s3.put_object()
        .bucket(&self.bucket)
        .key(&key)
        .body(content.into())
        .send().await?;
    
    Ok(ContentRef { hash, size: content.len() as u64, ..Default::default() })
}

async fn delete_tenant_content(&self, tenant: &TenantId) -> Result<u64> {
    // GDPR 级联删除时调用
    let prefix = format!("tenants/{}/", tenant);
    let mut deleted = 0u64;
    
    let mut paginator = self.s3.list_objects_v2()
        .bucket(&self.bucket)
        .prefix(&prefix)
        .into_paginator()
        .send();
    
    while let Some(page) = paginator.next().await {
        let objects = page?.contents().to_vec();
        if objects.is_empty() { break; }
        
        let delete_objects: Vec<_> = objects.iter()
            .filter_map(|o| o.key().map(|k| ObjectIdentifier::builder().key(k).build().unwrap()))
            .collect();
        
        self.s3.delete_objects()
            .bucket(&self.bucket)
            .delete(Delete::builder().set_objects(Some(delete_objects)).build().unwrap())
            .send().await?;
        
        deleted += objects.len() as u64;
    }
    
    Ok(deleted)
}
```

### 6.3 冷归档策略

```
30 天后:
  - 月分区从 Postgres 通过 pg_dump → S3 archive bucket
  - 然后 DROP PARTITION (释放热存储)
  - 应用层查询时,如果 occurred_at > 30 天且数据不在 hot,自动从 S3 拉取
```

应用层的"查询冷数据"逻辑：

```rust
async fn fetch_events(&self, traj: TrajectoryId, since: SystemTime) 
    -> Result<Vec<AgentEvent>, StoreError> 
{
    let cutoff = SystemTime::now() - Duration::from_days(30);
    
    if since >= cutoff {
        // 完全在热区
        return self.fetch_from_postgres(traj, since).await;
    }
    
    // 跨冷热边界
    let hot_events = self.fetch_from_postgres(traj, cutoff).await?;
    let cold_events = self.fetch_from_s3_archive(traj, since, cutoff).await?;
    
    let mut all = cold_events;
    all.extend(hot_events);
    all.sort_by_key(|e| e.created_at());
    Ok(all)
}
```

冷数据查询慢（S3 列表 + 下载几十 MB），应用层应该在 UI 上提示用户"加载历史中"。

---

## 7. Migration 策略

### 7.1 工具选型

```rust
// Cargo.toml
[dependencies]
sqlx = { version = "0.x", features = ["postgres", "sqlite", "runtime-tokio", "macros", "migrate"] }
```

```rust
// src/storage/mod.rs
pub async fn run_migrations(pool: &PgPool) -> Result<(), MigrationError> {
    sqlx::migrate!("./migrations/postgres").run(pool).await?;
    Ok(())
}
```

Migration 文件格式：

```
migrations/
  postgres/
    20260101120000_init.sql
    20260201120000_add_partitioning.sql
    20260301120000_add_l3_handle_sessions.sql
  sqlite/
    20260101120000_init.sql
    20260201120000_add_indexes.sql
```

### 7.2 Forward-compatible 演进规则

- **新增字段**：必须有 default value，老代码不感知
- **新增表**：完全向前兼容
- **重命名字段**：分两步——先加新字段双写，下个版本删除老字段
- **删除字段**：只在 N+2 版本删除（N 引入弃用，N+1 不再使用，N+2 物理删除）
- **数据 migration**：必须 idempotent，可重跑
- **Schema migration 与代码部署解耦**：先部署 migration，再部署新代码

### 7.3 滚动升级流程

```
T0: 应用 v1.2 在跑,schema v5
T1: 部署 migration → schema v6 (向前兼容,v1.2 仍能跑)
T2: 部署应用 v1.3 副本 1 (使用 schema v6 字段,但不依赖)
T3: 验证 v1.3 副本 1 健康
T4: 滚动升级所有副本到 v1.3
T5: (下个版本周期) 部署 v1.4 删除 v5 字段
T6: 部署 migration → schema v7 (物理 drop 旧字段)
```

### 7.4 重大 schema 变更

不可避免的破坏性变更（如分区策略调整、主键变更）：

1. **创建新表 schema**
2. **应用层双写**（旧表 + 新表）
3. **后台 backfill 数据**（idempotent）
4. **应用层切到只读新表**
5. **验证 N 周后**，drop 旧表

不允许在主版本内做破坏性变更——必须是 major version bump（v1 → v2）。

---

## 8. Backup 与 Recovery

### 8.1 备份策略

| 数据 | 备份频率 | 介质 | RPO | RTO |
|---|---|---|---|---|
| Postgres (events / audit / billing / config) | 每小时增量 + 每日全量 | S3 + 跨区复制 | 1h | 4h |
| Redis | 每小时 RDB snapshot | S3 | 1h | 1h (可丢失) |
| ContentRef S3 | 跨区复制 (实时) | 不同 region | 0 | 立即 |
| SecretManager | 由 secret manager 自身保证 | Vault 内置 | <5min | 5min |

### 8.2 灾难恢复演练

每季度一次完整 DR 演练：
1. 在隔离环境从最新备份恢复 Postgres + Redis + S3
2. 启动应用,验证基本功能 (login / submit task / cancel task)
3. 跑一组 smoke 用例验证数据完整性
4. 记录 RTO 实际值,与目标对比

### 8.3 Personal 模式备份

SQLite 文件简单 cp 即可，但 Personal 模式默认不做自动备份——用户自己用 Time Machine / 同步盘 / 手动 `tars backup` 命令：

```bash
tars backup --output ~/Documents/tars-backup-2026-05-02.tar.gz
tars restore --input ~/Documents/tars-backup-2026-05-02.tar.gz
```

---

## 9. 性能与容量规划

### 9.1 数据量预估 (Team / SaaS)

假设：100 租户、平均每租户每天 1000 LLM 请求

| 表 | 日增 | 月增 | 年增 |
|---|---|---|---|
| agent_events | 100 × 1000 × 8 (每请求平均 8 events) = 800k | 24M | 290M |
| audit_events | 100k | 3M | 36M |
| billing_events | 100k | 3M | 36M |
| trajectories | 100k (新建) | 3M | 36M |
| content_refs | 50k | 1.5M | 18M |

存储估算：
- agent_events 单行 ~500B (含 JSONB) → 月增 ~12GB → 30 天热数据 ~12GB（每 hash partition ~200MB）
- billing_events 单行 ~200B → 月增 ~600MB
- 总热数据 ~20-30GB，标准 RDS / Aurora 实例完全够

### 9.2 查询模式与索引

| 查询 | 频率 | 索引 |
|---|---|---|
| `SELECT * FROM agent_events WHERE tenant_id=? AND trajectory_id=? ORDER BY created_at` | 高 | `idx_agent_events_trajectory` |
| `SELECT * FROM trajectories WHERE tenant_id=? AND status='active'` | 高 | `idx_trajectories_active` |
| `SELECT SUM(cost_usd) FROM billing_events WHERE tenant_id=? AND occurred_at >= ?` | 中 | `idx_billing_tenant_time` |
| `SELECT * FROM audit_events WHERE occurred_at BETWEEN ? AND ? AND severity='critical'` | 低 | `idx_audit_critical` |

### 9.3 连接池

```toml
[storage.postgres]
pool_size = 50                    # = (CPU 核数 × 2) + effective_io_count, 经验值
acquire_timeout_ms = 3000
idle_timeout_secs = 600
max_lifetime_secs = 1800          # 强制连接轮换,避免 stale connections

[storage.redis]
pool_size = 100
connection_timeout_ms = 500
```

### 9.4 慢查询监控

```sql
-- pg_stat_statements 启用后定期采样
SELECT query, calls, total_exec_time, mean_exec_time, rows
FROM pg_stat_statements
WHERE mean_exec_time > 100        -- > 100ms
ORDER BY total_exec_time DESC
LIMIT 20;
```

慢查询阈值告警：单查询 > 1s 且 > 10 次/分钟 → 触发 SRE 告警。

---

## 10. 跨存储一致性

业务路径上避免跨存储事务。三种典型场景的处理：

### 10.1 写 agent_event + 计费 (跨表)

```rust
// 单 Postgres 事务,因为 agent_events 和 billing_events 都在 Postgres
async fn record_llm_call(&self, event: AgentEvent, billing: BillingEvent) -> Result<()> {
    let mut tx = self.pool.begin().await?;
    sqlx::query!(...).execute(&mut *tx).await?;     // agent_events
    sqlx::query!(...).execute(&mut *tx).await?;     // billing_events
    tx.commit().await?;
    Ok(())
}
```

### 10.2 写 agent_event + 扣 budget (跨存储)

```rust
// Postgres + Redis 跨存储,不能用事务
async fn record_with_budget(&self, event: AgentEvent, cost: f64) -> Result<()> {
    // Step 1: 写 Postgres event (业务真相,必须成功)
    self.event_store.append(event).await?;
    
    // Step 2: 扣 Redis budget (失败不阻塞业务,记 metric 异步对账)
    if let Err(e) = self.budget.deduct(&tenant, cost).await {
        tracing::warn!(?e, "budget deduct failed, will reconcile from billing_events");
        self.metrics.record_budget_lag();
    }
    
    Ok(())
}
```

定时 reconciliation：定期从 billing_events 重算预算应有值，与 Redis 对比，超过阈值则修正。

### 10.3 写 ContentRef + agent_event

```rust
async fn append_with_large_payload(&self, traj: TrajectoryId, payload: Vec<u8>) -> Result<()> {
    // Step 1: 写 ContentStore (S3 / FS),获取 hash
    let content_ref = self.content_store.put(payload).await?;
    
    // Step 2: 写 event (引用 content_ref)
    let event = AgentEvent::StepCompleted { 
        output_ref: content_ref,
        ...
    };
    self.event_store.append(event).await?;
    
    // 失败处理:
    // - Step 1 成功 + Step 2 失败 → S3 有孤儿,Janitor 通过 refcount=0 GC
    // - Step 1 失败 → 业务直接 abort
    Ok(())
}
```

ContentRef 通过 refcount + GC 清理孤儿，不依赖事务。

---

## 11. 数据生命周期与租户删除

呼应 Doc 06 §8.3 的级联删除，本节细化每个存储的清理路径：

```rust
async fn purge_tenant_data(&self, tenant: &TenantId) -> Result<PurgeReport, PurgeError> {
    let mut report = PurgeReport::default();
    
    // 1. Postgres - DROP PARTITION (秒级)
    for partition in self.list_tenant_partitions(tenant).await? {
        self.execute(&format!("DROP TABLE {}", partition)).await?;
        report.dropped_partitions += 1;
    }
    
    // 2. Postgres - 删除 tenant 主键引用的行 (其余表自动 CASCADE)
    sqlx::query!("DELETE FROM tenants WHERE id = $1", tenant.0).execute(...).await?;
    report.deleted_tenant_rows = 1;
    
    // 3. Redis - SCAN + DEL 按前缀
    let patterns = vec![
        format!("cache:l2:{}:*", tenant),
        format!("cache:l2:meta:{}:*", tenant),
        format!("budget:*:{}*", tenant),
        format!("idem:{}:*", tenant),
        format!("session:tenant_active:{}", tenant),
    ];
    for pattern in patterns {
        report.deleted_redis_keys += self.redis.scan_and_delete(&pattern).await?;
    }
    
    // 4. S3 - 按 prefix 删除
    report.deleted_s3_objects += self.s3
        .delete_tenant_content(tenant).await?;
    report.deleted_s3_objects += self.s3_archive
        .delete_tenant_archive(tenant).await?;
    
    // 5. 文件系统 (subprocess HOME)
    let home = format!("/var/lib/tars/tenants/{}/", tenant);
    if Path::new(&home).exists() {
        fs::remove_dir_all(&home).await?;
        report.fs_purged = true;
    }
    
    // 6. Secret manager namespace
    self.secret_manager.delete_namespace(&format!("tenants/{}", tenant)).await?;
    report.secrets_purged = true;
    
    // 7. 写不可篡改 audit (即使 tenant 被删,这条记录保留)
    self.audit.write(AuditEvent::TenantDeleted {
        tenant: tenant.clone(),
        deleted_at: SystemTime::now(),
        report: report.clone(),
    }).await?;
    
    Ok(report)
}
```

**关键不变量**：
- 7 步必须按顺序执行（前面失败则停止）
- 每步都有独立 metric (deleted_partitions / deleted_redis_keys / etc.)
- audit_events 中关于该 tenant 的记录**保留**——它们是关于"曾经发生过"的事实，不属于 tenant 数据
- billing_events 在分区被 drop 时才一起消失（按月分区，租户删除当月之前的会保留到分区到期）。如果合规要求立即删除，需要单独 query 删除该 tenant 的 billing 行

---

## 12. 测试策略

### 12.1 Schema 测试

```rust
#[tokio::test]
async fn migrations_apply_cleanly_to_empty_db() {
    let pool = empty_test_postgres().await;
    sqlx::migrate!("./migrations/postgres").run(&pool).await.unwrap();
    
    // 验证关键表存在
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public'"
    ).fetch_all(&pool).await.unwrap();
    
    assert!(tables.contains(&"tenants".to_string()));
    assert!(tables.contains(&"trajectories".to_string()));
}

#[tokio::test]
async fn migrations_idempotent() {
    let pool = empty_test_postgres().await;
    sqlx::migrate!("./migrations/postgres").run(&pool).await.unwrap();
    sqlx::migrate!("./migrations/postgres").run(&pool).await.unwrap();  // 第二次必须成功
}
```

### 12.2 Repository contract 测试

```rust
// 同一组 contract test 跑两遍 - SQLite 和 Postgres 都必须通过
async fn event_store_contract_test(store: Arc<dyn EventStore>) {
    let event = AgentEvent::TaskCreated { /* ... */ };
    let offset = store.append(event.clone()).await.unwrap();
    
    let fetched = store.fetch(traj_id, offset).await.unwrap();
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0], event);
}

#[tokio::test]
async fn sqlite_event_store_satisfies_contract() {
    let store = SqliteEventStore::new(test_sqlite_path()).await.unwrap();
    event_store_contract_test(Arc::new(store)).await;
}

#[tokio::test]
async fn postgres_event_store_satisfies_contract() {
    let store = PostgresEventStore::new(test_pg_pool()).await.unwrap();
    event_store_contract_test(Arc::new(store)).await;
}
```

### 12.3 分区测试

```rust
#[tokio::test]
async fn drop_tenant_partition_does_not_affect_others() {
    let pool = test_pg_pool().await;
    
    let tenant_a = provision_test_tenant(&pool).await;
    let tenant_b = provision_test_tenant(&pool).await;
    
    insert_test_events(&pool, &tenant_a, 100).await;
    insert_test_events(&pool, &tenant_b, 100).await;
    
    purge_tenant_data(&pool, &tenant_a).await.unwrap();
    
    let count_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE tenant_id = $1"
    ).bind(tenant_a.0).fetch_one(&pool).await.unwrap();
    assert_eq!(count_a, 0);
    
    let count_b: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE tenant_id = $1"
    ).bind(tenant_b.0).fetch_one(&pool).await.unwrap();
    assert_eq!(count_b, 100);
}
```

### 12.4 备份恢复测试

```rust
#[tokio::test]
#[ignore]   // 慢测试,只在 nightly 跑
async fn backup_and_restore_round_trip() {
    let pool_a = test_pg_pool().await;
    populate_test_data(&pool_a).await;
    
    let backup = backup_database(&pool_a).await.unwrap();
    
    let pool_b = empty_test_pg_pool().await;
    restore_database(&pool_b, backup).await.unwrap();
    
    assert_data_equivalent(&pool_a, &pool_b).await;
}
```

---

## 13. 反模式清单

1. **不要在业务路径做跨存储事务**——Postgres + Redis + S3 各自管自己,通过 reconciliation 收敛。
2. **不要把高基数 label 放进表名**（`events_user_x_y` 风格）——用分区,不用动态表名。
3. **不要 UPDATE 事件日志**——append-only,纠错也通过追加新事件表达。
4. **不要 SELECT * 在大表**——event 表行可能 10KB+,只取需要的字段。
5. **不要忘记 PostgreSQL 的 `WAL` 配置**——`wal_buffers` / `checkpoint_timeout` 默认值不适合写密集负载。
6. **不要在 Personal 模式启用 SQLite 多进程**——SQLite 是 file-based 数据库,多进程并发写会冲突,严格单进程。
7. **不要让 Migration 包含数据 migration**——schema 和 data migration 分两步,data migration 是独立 idempotent script。
8. **不要在租户删除时漏掉某个存储**——7 步清单必须每步可观测+可重试。
9. **不要在 audit / billing 表上加业务索引**——这些表是 append-only + 时间范围查询,只需要时间索引。
10. **不要忽略 `pg_stat_statements`**——慢查询不可见就不可优化。
11. **不要让 ContentRef 永不过期**——orphan content (refcount=0) 必须有 GC,否则 S3 账单失控。
12. **不要在 schema 演进时直接重命名字段**——分两步,先加新字段双写,再删老字段。
13. **不要在 Redis 里持久化"绝不能丢"的数据**——即使 AOF everysec 也最多丢 1s,event/audit/billing 必须 Postgres。
14. **不要在 Personal 模式加 retention policy**——用户数据,用户决定何时删。
15. **不要把 secret 值存在业务 DB**——只存 SecretRef (path / var name),值由 SecretResolver 运行时拉取。

---

## 14. 与上下游的契约

### 上游 (业务代码) 承诺

- 通过 Repository trait 访问存储,不直接写 SQL / Redis 命令
- 不假设跨存储原子性
- 大 payload (>16KB) 通过 ContentStore put 后引用,不直接放进 event payload
- 业务路径上的查询都有索引覆盖,定期 review

### 下游 (DB / Redis / S3) 契约

- DB schema migration 通过 `sqlx::migrate!` 管理,有 forward-compatibility 约束
- Redis 是性能优化层,丢数据通过 reconciliation 修复
- S3 是冷存储 + 大对象 + 跨区复制,通过 versioning + lifecycle rule 管理

### 跨节点契约

- 多副本应用通过 Postgres / Redis 同步状态,不直接互相通信
- Postgres 主从延迟 < 100ms (synchronous_commit = on for primary, async replica 仅做读)
- 读副本可承担大部分查询,但事件写入必须主库

---

## 15. 待办与开放问题

- [ ] Postgres 主备切换的应用层透明 (PgBouncer vs HAProxy vs RDS Proxy)
- [ ] ContentRef 的 GC 频率与策略 (linear scan vs incremental marking)
- [ ] S3 跨区复制延迟监控 (避免 disaster recovery 时数据缺失)
- [ ] SQLite 在 Apple Silicon 上的 mmap 性能 (是否值得调大 mmap_size)
- [ ] 大租户单独物理库 (vs hash partition) 的成本/复杂度权衡
- [ ] OLAP 需求的承载方式 (read replica vs CDC 到 ClickHouse vs DuckDB)
- [ ] 跨区域 multi-region 部署的 Postgres 架构 (BDR vs Patroni vs Cloud-managed)
- [ ] Schema migration 的 dry-run / canary 机制
- [ ] Audit 表的 partition pruning 在 7 年保留下的查询性能
- [ ] Redis 持久化策略对 budget 准确性的影响 (重启丢 1s vs 重新对账)
