# Doc 09 — Data Persistence and Storage Schema

> Scope: define storage media, table structures, indexes, partitioning, migrations, and backup strategies for all persistent data.
>
> Context: spans the preceding Docs 01-08 — this document introduces no new business concepts, only consolidating scattered storage constraints.
>
> Targets two deployment shapes: Personal (SQLite) and Team/SaaS/Hybrid (Postgres + Redis + S3).

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Same schema across stores** | SQLite and Postgres table structures stay isomorphic where possible; business code is shared |
| **Append-only first** | Events / audit / billing — the three big tables are INSERT-only; UPDATE / DELETE never on business paths |
| **Tenant-level partitioning** | Large tables physically partitioned by tenant_id; tenant deletion completes via DROP PARTITION in seconds |
| **Hot/cold tiering** | Hot data (last 30 days) in OLTP, cold data archived to S3, queries transparent |
| **Zero-downtime migration** | Schema migration must support rolling upgrade; new and old versions coexist |
| **Recoverable backups** | Any point-in-time state recoverable from backups, RPO < 1h, RTO < 4h |
| **Storage decoupled from business** | All schemas exposed via Repository traits; business code writes no SQL |

**Non-goals**:
- No cross-table transactions on business paths (performance bottleneck, complex error recovery)
- No business dependence on a specific SQL dialect (Postgres-specific features hidden behind Repository)
- No denormalization "for performance" that forces the same data to be updated in multiple places
- No concurrent hot writes against SQLite (Personal mode expects a single process; multi-process will conflict)

---

## 2. Storage Medium Matrix

Allocation by deployment shape:

| Data type | Personal | Team / SaaS | Hybrid |
|---|---|---|---|
| Recovery event log (`AgentEventLog`, tars-storage) | SQLite (WAL mode) | Postgres (partitioned) | SQLite (local) |
| Pipeline event log + `LlmRecord` (`tars_melt::event`) | SQLite / JSONL | Postgres / SQLite | SQLite (local) |
| Blackboard (coordination, tars-storage) | SQLite (WAL mode) | Postgres | SQLite (local) |
| Audit log | SQLite (separate file) | Postgres + S3 mirror + SIEM | SQLite + no upload |
| Billing events | SQLite | Postgres | SQLite (local rollup → upload anonymized metric) |
| Tenant config | TOML file (no DB) | Postgres | TOML + cloud sync |
| L1 Cache | In-process memory | In-process memory | In-process memory |
| L2 Cache | SQLite blob | Redis Cluster | SQLite blob |
| L3 Cache index | SQLite | Postgres | SQLite |
| Budget state | SQLite (single process) | Redis (atomic decrement) | SQLite |
| Idempotency Cache | SQLite | Redis | SQLite |
| Session state | SQLite | Redis | SQLite |
| Large Content (ContentRef) | Local FS | S3 | Local FS |
| Secret reference (no value) | TOML | Postgres | TOML |
| Secret value | OS keychain / file | Vault / KMS | OS keychain |

### 2.1 Selection Rationale at a Glance

- **SQLite + WAL**: Personal mode is single-process, no operational overhead, performance is sufficient (<1k events/s is plenty)
- **Postgres**: required for Team/SaaS multi-replica, strong transactional guarantees, mature partitioning
- **Redis**: atomic decrement + pub/sub + high QPS — best fit for Budget, Cache L2, and the blackboard **activation-notify transport**; **never the durable log** (§2.2)
- **S3**: large objects + cold archive + cross-region replication
- **OS keychain / Vault**: secret values must never sit alongside the business DB

### 2.2 The Durable Event/Coordination Stores Are Three Distinct Homes

The old too-generic `EventStore` name is retired. There is no single shared `EventStore<E>`. Three separate stores, each with its own owner, reader, and reliability contract:

| Store | Owner crate | What it holds | Read back by | Reliability contract |
|---|---|---|---|---|
| `AgentEventLog` | **tars-storage** | `AgentEvent` (Doc 04), one per agent decision | Runtime itself, to replay/resume a trajectory | **Recovery plane**: fsync-before-ack; complete + ordered per trajectory; idempotent = exactly-once **effect** (via `StepIdempotencyKey`), not exactly-once delivery. **MUST NOT be downsampled** — business truth, not MELT. |
| `PipelineEventLog` + `LlmRecord` | **`tars_melt::event`** (Doc 08 §3) | one `PipelineEvent` per `Pipeline.call`, plus the per-call `LlmRecord` (the `ChatRequest` + `ChatResponse`, held under a `ContentRef`) | eval / `tars events` / debug / test / replay | Read-able **observability/eval** E-pillar. Full fidelity, never sampled, durable. The producing pipeline never reads it back. Not recovery truth. |
| Blackboard | **tars-storage** | coordination substrate (entities + append-only per-entity timeline) | steps, to read current scoped state | **Coordination plane**. The durable board is the truth; the **activation** design (notify + reconcile) is **deferred — not designed here** (see principle below). |

**`LlmRecord`, never "body".** The per-call request+response record is an `LlmRecord`. There is no `BodyStore` and no "body" concept — that name is retired. `ContentRef` remains the generic tenant-scoped CAS reference type; the thing it points at, for an LLM call, is an `LlmRecord`.

**Backends — one guarantee, three realizations.** Every durable log commits to the same guarantee (**durable, append-only, forever**), realized by one of:

| Backend | When | Note |
|---|---|---|
| **JSONL file** | simplest, greppable | Personal / local debug |
| **SQLite** | embedded + indexed | Personal / single-node |
| **Postgres** | networked + indexed + `LISTEN/NOTIFY` | Team / SaaS |

- **Never Redis or in-memory as the durable log** — volatile ≠ system-of-record. In-memory is **test-only**. Redis is legitimate ONLY as the activation-notify transport (pub/sub) or budget/cache (§5), never the log. Records cold-tier to S3 (§6). (Physical placement of the durable log / event store is out of scope for this doc.)

**Per-plane reliability.**
- **Recovery (`AgentEventLog`)**: fsync-before-ack + idempotent exactly-once effect (above). A failed append fails the trajectory (Doc 08 §3 key invariant).
- **Activation (blackboard)**: the notify is a **hint** — ephemeral, best-effort (in-proc `tokio::Notify`/`watch` locally; Redis pub/sub or Postgres `LISTEN/NOTIFY` networked); the **truth** is the durable board. Reliability = **reconcile-against-state** (edge-triggered notify + level-triggered reconcile, like Kubernetes controllers): a dropped notify is caught by the next reconcile, a crash triggers full reconcile, and a wake never blocks on the durable write. Full activation design is **deferred** (Doc 19 §4).

---

## 3. Postgres Schema (Team / SaaS)

### 3.1 Overview

```
public schema (core business):
  tenants
  workspaces
  sessions
  iam_scope_assignments
  
  trajectories                       (partitioned by tenant_id)
  agent_events                       (partitioned by tenant_id, sub-partitioned by month)
  content_refs
  
  l3_cache_handles
  l3_handle_sessions                 (reference relations)
  
  idempotency_cache                  (partitioned by tenant_id)
  
  pending_compensations
  compensation_failures              (alert source)
  
audit schema (compliance):
  audit_events                       (partitioned by time, append-only)
  audit_signatures                   (HMAC, 1:1 with event)
  
billing schema (billing):
  billing_events                     (partitioned by month)
  billing_aggregations_hourly
  billing_aggregations_daily
  billing_aggregations_monthly
  
config schema (configuration):
  tenant_configs                     (JSONB)
  config_change_log                  (append-only, who changed what)
  secret_refs                        (references, not values)
  
ops schema (operational metadata):
  schema_migrations                  (refinery / sqlx::migrate!)
  cardinality_observations           (Doc 08 §5.5)
  reconciliation_runs                (Doc 03 §7.3)
```

### 3.2 Key Table DDL

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

#### 3.2.2 trajectories (partitioned by tenant)

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

-- Create partitions (typical deployment uses 64 hash partitions)
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

-- Indexes
CREATE INDEX idx_trajectories_active 
    ON trajectories (tenant_id, status, updated_at) 
    WHERE status IN ('active', 'suspended');
CREATE INDEX idx_trajectories_root_task 
    ON trajectories (tenant_id, root_task_id);
CREATE INDEX idx_trajectories_parent 
    ON trajectories (tenant_id, parent_id) 
    WHERE parent_id IS NOT NULL;
```

#### 3.2.3 agent_events (partitioned by tenant + time)

The largest table, expected 10⁶-10⁹ rows. Two-level partitioning: tenant_id (HASH) → created_at (RANGE by month):

```sql
CREATE TABLE agent_events (
    id              BIGSERIAL,
    tenant_id       UUID NOT NULL,
    trajectory_id   UUID NOT NULL,
    step_seq        INT,                  -- nullable: non-step events have none
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    content_ref     BYTEA,                -- large payloads referenced indirectly via content_refs
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

-- Each hash partition is sub-partitioned by month, automatically maintained by pg_partman:
-- SELECT partman.create_parent('public.agent_events_p0', 'created_at', 'native', 'monthly');

CREATE INDEX idx_agent_events_trajectory 
    ON agent_events (tenant_id, trajectory_id, created_at);
CREATE INDEX idx_agent_events_type 
    ON agent_events (tenant_id, event_type, created_at);

-- BRIN index suits append-only time series; 100x smaller than B-tree
CREATE INDEX idx_agent_events_brin 
    ON agent_events USING BRIN (created_at);
```

**Cold archive**: month partitions older than 30 days are exported via `pg_dump` → S3, then `DROP PARTITION`. After archiving, queries follow the hot/cold mixed path in §7.

#### 3.2.4 content_refs

```sql
CREATE TABLE content_refs (
    hash            BYTEA PRIMARY KEY,        -- SHA-256, 32 bytes
    tenant_id       UUID NOT NULL,
    size_bytes      BIGINT NOT NULL,
    mime_type       TEXT,
    backend         TEXT NOT NULL CHECK (backend IN ('inline', 's3', 'fs')),
    inline_data     BYTEA,                    -- only when backend = 'inline'
    s3_key          TEXT,                     -- only when backend = 's3'
    fs_path         TEXT,                     -- only when backend = 'fs'
    refcount        INT NOT NULL DEFAULT 0,   -- reference count, used by GC
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_content_refs_tenant ON content_refs(tenant_id);
CREATE INDEX idx_content_refs_zero_refcount ON content_refs(refcount) WHERE refcount = 0;
CREATE INDEX idx_content_refs_unaccessed ON content_refs(last_accessed_at) 
    WHERE refcount = 0;

-- Backend selection policy (in the application layer):
-- size <= 16KB    → inline
-- size <= 1MB     → fs (local)
-- size > 1MB      → s3
```

#### 3.2.5 l3_cache_handles + reference counting

```sql
CREATE TABLE l3_cache_handles (
    id              UUID PRIMARY KEY,
    tenant_id       UUID NOT NULL,
    cache_key_hash  BYTEA NOT NULL,           -- fingerprint per Doc 03 §3
    provider        TEXT NOT NULL,
    external_id     TEXT NOT NULL,            -- provider-side cache id
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

-- Reference relation table (Doc 03 §7.2)
CREATE TABLE l3_handle_sessions (
    handle_id       UUID NOT NULL REFERENCES l3_cache_handles(id) ON DELETE CASCADE,
    session_id      UUID NOT NULL,
    referenced_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (handle_id, session_id)
);

CREATE INDEX idx_l3_handle_sessions_session ON l3_handle_sessions(session_id);
```

#### 3.2.6 audit_events (partitioned by time)

```sql
CREATE TABLE audit.audit_events (
    id              BIGSERIAL,
    tenant_id       UUID,                     -- nullable: system-level events (config reload)
    principal_id    TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    severity        TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    payload         JSONB NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (occurred_at, id)
) PARTITION BY RANGE (occurred_at);

-- Monthly partitions, 7-year retention
SELECT partman.create_parent(
    'audit.audit_events', 
    'occurred_at', 
    'native', 
    'monthly',
    p_premake := 12,
    p_retention := '7 years',
    p_retention_keep_table := false  -- actually delete (after compliance allows)
);

CREATE INDEX idx_audit_tenant ON audit.audit_events(tenant_id, occurred_at) 
    WHERE tenant_id IS NOT NULL;
CREATE INDEX idx_audit_type ON audit.audit_events(event_type, occurred_at);
CREATE INDEX idx_audit_critical ON audit.audit_events(occurred_at) 
    WHERE severity = 'critical';

-- HMAC signature table (1:1 with event, tamper-resistant)
CREATE TABLE audit.audit_signatures (
    event_id        BIGINT NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL,
    hmac_sha256     BYTEA NOT NULL,
    signing_key_id  TEXT NOT NULL,
    PRIMARY KEY (occurred_at, event_id),
    FOREIGN KEY (occurred_at, event_id) REFERENCES audit.audit_events(occurred_at, id)
);
```

**Key invariants**:
- `audit_events` has no UPDATE / DELETE permissions — enforced via Postgres role
- Even superuser can only delete via a special approval workflow (compliance requirement)
- `audit_signatures` is populated immediately after event write; HMAC keys rotate periodically

```sql
-- Enforce append-only
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

-- Pre-aggregations (written by scheduled jobs to speed up dashboard queries)
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

-- Configuration change history (append-only)
CREATE TABLE config.config_change_log (
    id              BIGSERIAL PRIMARY KEY,
    tenant_id       UUID,                     -- NULL means a global configuration change
    changed_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    changed_by      TEXT NOT NULL,
    change_kind     TEXT NOT NULL,
    diff            JSONB NOT NULL,           -- patch of before/after
    reason          TEXT
);

CREATE INDEX idx_config_log_tenant ON config.config_change_log(tenant_id, changed_at);
```

---

## 4. SQLite Schema (Personal)

The SQLite-mode schema is broadly isomorphic to Postgres, with a few key differences:

### 4.1 Schema Differences

| Dimension | Postgres | SQLite (Personal) |
|---|---|---|
| Data types | UUID / TIMESTAMPTZ / JSONB / BYTEA | TEXT (UUID string) / INTEGER (unix epoch) / TEXT (JSON string) / BLOB |
| Partitioning | HASH + RANGE | none (single tenant, low volume, not needed) |
| Concurrency | MVCC | WAL mode + busy_timeout |
| Foreign keys | enabled automatically | must `PRAGMA foreign_keys = ON` |
| Schema isolation | schema namespace | flat single file |

### 4.2 SQLite Startup Configuration

```rust
async fn init_sqlite(path: &Path) -> Result<Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_init(|conn| {
            // Key PRAGMAs
            conn.execute_batch("
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;       -- safe enough under WAL
                PRAGMA foreign_keys = ON;
                PRAGMA busy_timeout = 5000;        -- 5s lock wait
                PRAGMA cache_size = -32000;        -- 32MB in-memory cache
                PRAGMA temp_store = MEMORY;
                PRAGMA mmap_size = 268435456;      -- 256MB mmap
            ")?;
            Ok(())
        });
    
    let pool = Pool::builder()
        .max_size(8)                              // Personal mode, low concurrency
        .build(manager)?;
    
    Ok(pool)
}
```

### 4.3 SQLite agent_events (example, mirroring Postgres)

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

Note: SQLite has no tenant_id field (Personal is always single-tenant, hardcoded as `local`).

### 4.4 Sharing Code Between SQLite and Postgres

Through `sqlx`'s `Database` abstraction, business code is shielded by a trait. This is the **recovery** log (§2.2) — `AgentEventLog`, holding `AgentEvent`; it is not the generic `EventStore` name (retired) and not `tars_melt::event`:

```rust
#[async_trait]
pub trait AgentEventLog: Send + Sync {
    async fn append(&self, event: AgentEvent) -> Result<EventOffset, StoreError>;
    async fn fetch(&self, traj: TrajectoryId, since: EventOffset) -> Result<Vec<AgentEvent>, StoreError>;
}

pub struct SqliteAgentEventLog { pool: Pool<SqliteConnectionManager> }
pub struct PostgresAgentEventLog { pool: PgPool }

#[async_trait] impl AgentEventLog for SqliteAgentEventLog { ... }
#[async_trait] impl AgentEventLog for PostgresAgentEventLog { ... }
```

Implementation chosen at startup based on `mode` configuration:

```rust
let event_log: Arc<dyn AgentEventLog> = match config.storage.backend {
    StorageBackend::Sqlite => Arc::new(SqliteAgentEventLog::new(...).await?),
    StorageBackend::Postgres => Arc::new(PostgresAgentEventLog::new(...).await?),
};
```

---

## 5. Redis Data Structures (Team / SaaS)

### 5.1 Key Naming Convention

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
                                             (short-lived, held by leader)

invalidation_channel                       → pub/sub channel (cross-instance)
```

### 5.2 Atomic Budget Decrement (Lua)

A non-atomic GET + business check + SET pattern is unacceptable (race condition):

```lua
-- KEYS[1] = budget key (e.g., "budget:cost:daily:acme:2026-05-02")
-- ARGV[1] = amount to deduct
-- ARGV[2] = hard limit
-- returns: { remaining, granted (1/0) }

local current = redis.call('GET', KEYS[1])
current = tonumber(current) or 0
local amount = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])

if current + amount > limit then
    return { current, 0 }
end

local new_value = redis.call('INCRBYFLOAT', KEYS[1], amount)

-- Set TTL on first creation (24h, automatic reset across days)
if current == 0 then
    redis.call('EXPIRE', KEYS[1], 86400)
end

return { new_value, 1 }
```

Application-side call:

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

### 5.3 Invalidation Broadcast (pub/sub)

```rust
// Each instance subscribes at startup
async fn subscribe_invalidation(redis: &RedisClient, l1: Arc<L1Cache>) {
    let mut sub = redis.subscribe("cache:invalidation").await.unwrap();
    while let Some(msg) = sub.next().await {
        if let Ok(event) = serde_json::from_str::<InvalidationEvent>(&msg.payload) {
            l1.apply_invalidation(&event).await;
        }
    }
}

// Writer side
async fn invalidate(&self, key: &CacheKey) {
    let event = InvalidationEvent::Key(key.clone());
    let payload = serde_json::to_string(&event).unwrap();
    self.redis.publish("cache:invalidation", payload).await.ok();
    
    // Apply locally immediately
    self.local_l1.remove(key).await;
    self.l2.delete(key).await.ok();
}
```

### 5.4 Recommended Redis Configuration

```toml
[storage.redis]
url = { source = "vault", path = "secret/data/tars/redis" }
pool_size = 50
connection_timeout_ms = 1000
command_timeout_ms = 500          # short timeout, fallback on failure does not block business

# Persistence policy
# AOF everysec: at most 1s of data loss, suitable for budget/cache (small loss tolerable)
# RDB backups hourly, replicated across availability zones
```

**Key decisions**:
- Redis is not the source of truth — losing budget state can be re-accumulated (worst case, the user gets a few extra seconds), losing cache means regenerating
- Do not put audit / billing / event data in Redis (insufficient durability)
- Redis Cluster uses hashtags (`{tenant_id}`) so same-tenant keys land in the same slot

---

## 6. S3 / Object Storage Layout

### 6.1 Bucket Structure

```
s3://tars-{env}-content/
  ├─ tenants/{tenant_id}/
  │  ├─ content/{hash[0:2]}/{hash[2:32]}      → ContentRef large objects
  │  └─ debug-snapshots/{date}/{trajectory_id}.json  → user-initiated export
  └─ shared/
     └─ models/                                → embedded ONNX / GGUF (cross-tenant shared)

s3://tars-{env}-archive/
  ├─ events/{tenant_id}/{year}/{month}/events.jsonl.zstd  → cold-archived events
  ├─ audit/{year}/{month}/audit.jsonl.zstd               → audit cold archive (7 years)
  └─ billing/{year}/{month}/billing.jsonl.zstd           → billing cold archive (3 years)

s3://tars-{env}-backup/
  ├─ postgres/{date}/{cluster}.dump
  ├─ redis/{date}/redis.rdb
  └─ config/{date}/config-snapshot.tar.gz
```

### 6.2 ContentRef S3 Operations

```rust
async fn put_content(&self, content: Vec<u8>, tenant: &TenantId) -> Result<ContentRef> {
    let hash = sha256(&content);
    let hash_hex = hex::encode(&hash);
    let key = format!("tenants/{}/content/{}/{}", 
        tenant, &hash_hex[..2], &hash_hex[2..]);
    
    // Content addressing: skip upload if it already exists
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
    // Called during GDPR cascade deletion
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

### 6.3 Cold Archive Strategy

```
After 30 days:
  - Month partitions exported from Postgres via pg_dump → S3 archive bucket
  - Then DROP PARTITION (free hot storage)
  - On query, if occurred_at > 30 days and data not in hot tier, automatically pull from S3
```

Application-layer "query cold data" logic:

```rust
async fn fetch_events(&self, traj: TrajectoryId, since: SystemTime) 
    -> Result<Vec<AgentEvent>, StoreError> 
{
    let cutoff = SystemTime::now() - Duration::from_days(30);
    
    if since >= cutoff {
        // Entirely in the hot zone
        return self.fetch_from_postgres(traj, since).await;
    }
    
    // Crosses the hot/cold boundary
    let hot_events = self.fetch_from_postgres(traj, cutoff).await?;
    let cold_events = self.fetch_from_s3_archive(traj, since, cutoff).await?;
    
    let mut all = cold_events;
    all.extend(hot_events);
    all.sort_by_key(|e| e.created_at());
    Ok(all)
}
```

Cold queries are slow (S3 listing + downloading tens of MB); the application layer should surface "loading history" in the UI.

---

## 7. Migration Strategy

### 7.1 Tooling

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

Migration file layout:

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

### 7.2 Forward-compatible Evolution Rules

- **Add field**: must have a default value; old code is unaware
- **Add table**: fully forward-compatible
- **Rename field**: do it in two steps — first add the new field with dual writes, remove the old field in the next version
- **Drop field**: only drop in N+2 (N introduces deprecation, N+1 stops using it, N+2 physically removes)
- **Data migration**: must be idempotent and rerunnable
- **Schema migration decoupled from code deployment**: deploy the migration first, then the new code

### 7.3 Rolling Upgrade Flow

```
T0: app v1.2 running, schema v5
T1: deploy migration → schema v6 (forward compatible, v1.2 still runs)
T2: deploy app v1.3 replica 1 (uses schema v6 fields, but does not depend on them)
T3: verify v1.3 replica 1 is healthy
T4: roll all replicas to v1.3
T5: (next release cycle) deploy v1.4 to remove v5 fields
T6: deploy migration → schema v7 (physically drop old fields)
```

### 7.4 Major Schema Changes

Unavoidable breaking changes (e.g. partitioning policy adjustment, primary key change):

1. **Create a new table schema**
2. **Application-layer dual writes** (old + new tables)
3. **Background backfill of data** (idempotent)
4. **Application layer switches to read-only against the new table**
5. **After N weeks of validation**, drop the old table

Breaking changes are not allowed within a major version — they require a major version bump (v1 → v2).

---

## 8. Backup and Recovery

### 8.1 Backup Strategy

| Data | Backup frequency | Medium | RPO | RTO |
|---|---|---|---|---|
| Postgres (events / audit / billing / config) | hourly incremental + daily full | S3 + cross-region replication | 1h | 4h |
| Redis | hourly RDB snapshot | S3 | 1h | 1h (loss acceptable) |
| ContentRef S3 | cross-region replication (real-time) | different region | 0 | immediate |
| SecretManager | guaranteed by the secret manager itself | Vault built-in | <5min | 5min |

### 8.2 Disaster Recovery Drills

Quarterly full DR drills:
1. Restore Postgres + Redis + S3 from the latest backup in an isolated environment
2. Start the application, verify basic functions (login / submit task / cancel task)
3. Run a smoke suite to verify data integrity
4. Record actual RTO and compare against target

### 8.3 Personal Mode Backup

The SQLite file can simply be `cp`'d, but Personal mode performs no automatic backups by default — users handle it themselves with Time Machine / sync drives / manual `tars backup`:

```bash
tars backup --output ~/Documents/tars-backup-2026-05-02.tar.gz
tars restore --input ~/Documents/tars-backup-2026-05-02.tar.gz
```

---

## 9. Performance and Capacity Planning

### 9.1 Volume Estimation (Team / SaaS)

Assume: 100 tenants, 1000 LLM requests per tenant per day on average

| Table | Daily | Monthly | Yearly |
|---|---|---|---|
| agent_events | 100 × 1000 × 8 (avg 8 events per request) = 800k | 24M | 290M |
| audit_events | 100k | 3M | 36M |
| billing_events | 100k | 3M | 36M |
| trajectories | 100k (new) | 3M | 36M |
| content_refs | 50k | 1.5M | 18M |

Storage estimates:
- agent_events row ~500B (with JSONB) → ~12GB monthly → ~12GB of 30-day hot data (~200MB per hash partition)
- billing_events row ~200B → ~600MB monthly
- Total hot data ~20-30GB; a standard RDS / Aurora instance is more than sufficient

### 9.2 Query Patterns and Indexes

| Query | Frequency | Index |
|---|---|---|
| `SELECT * FROM agent_events WHERE tenant_id=? AND trajectory_id=? ORDER BY created_at` | high | `idx_agent_events_trajectory` |
| `SELECT * FROM trajectories WHERE tenant_id=? AND status='active'` | high | `idx_trajectories_active` |
| `SELECT SUM(cost_usd) FROM billing_events WHERE tenant_id=? AND occurred_at >= ?` | medium | `idx_billing_tenant_time` |
| `SELECT * FROM audit_events WHERE occurred_at BETWEEN ? AND ? AND severity='critical'` | low | `idx_audit_critical` |

### 9.3 Connection Pools

```toml
[storage.postgres]
pool_size = 50                    # = (CPU cores × 2) + effective_io_count, rule of thumb
acquire_timeout_ms = 3000
idle_timeout_secs = 600
max_lifetime_secs = 1800          # forced connection rotation, avoids stale connections

[storage.redis]
pool_size = 100
connection_timeout_ms = 500
```

### 9.4 Slow Query Monitoring

```sql
-- With pg_stat_statements enabled, sample periodically
SELECT query, calls, total_exec_time, mean_exec_time, rows
FROM pg_stat_statements
WHERE mean_exec_time > 100        -- > 100ms
ORDER BY total_exec_time DESC
LIMIT 20;
```

Slow query alert threshold: a single query > 1s and > 10 times/minute → SRE alert.

---

## 10. Cross-Storage Consistency

Avoid cross-storage transactions on the business path. Three typical scenarios are handled as follows:

### 10.1 Write agent_event + billing (cross-table)

```rust
// Single Postgres transaction, since both agent_events and billing_events live in Postgres
async fn record_llm_call(&self, event: AgentEvent, billing: BillingEvent) -> Result<()> {
    let mut tx = self.pool.begin().await?;
    sqlx::query!(...).execute(&mut *tx).await?;     // agent_events
    sqlx::query!(...).execute(&mut *tx).await?;     // billing_events
    tx.commit().await?;
    Ok(())
}
```

### 10.2 Write agent_event + deduct budget (cross-store)

```rust
// Postgres + Redis spans stores; transaction not possible
async fn record_with_budget(&self, event: AgentEvent, cost: f64) -> Result<()> {
    // Step 1: write Postgres event (business truth, must succeed)
    self.event_log.append(event).await?;
    
    // Step 2: deduct Redis budget (failure does not block business; record metric, async reconcile)
    if let Err(e) = self.budget.deduct(&tenant, cost).await {
        tracing::warn!(?e, "budget deduct failed, will reconcile from billing_events");
        self.metrics.record_budget_lag();
    }
    
    Ok(())
}
```

Periodic reconciliation: recompute the expected budget from billing_events on a schedule, compare with Redis, and correct if it exceeds threshold.

### 10.3 Write ContentRef + agent_event

```rust
async fn append_with_large_payload(&self, traj: TrajectoryId, payload: Vec<u8>) -> Result<()> {
    // Step 1: write ContentStore (S3 / FS), get the hash
    let content_ref = self.content_store.put(payload).await?;
    
    // Step 2: write the event (referencing content_ref)
    let event = AgentEvent::StepCompleted { 
        output_ref: content_ref,
        ...
    };
    self.event_log.append(event).await?;
    
    // Failure handling:
    // - Step 1 succeeds + Step 2 fails → S3 has an orphan, GC'd by Janitor via refcount=0
    // - Step 1 fails → business aborts directly
    Ok(())
}
```

ContentRef relies on refcount + GC to clean orphans, not transactions.

---

## 11. Data Lifecycle and Tenant Deletion

Echoing Doc 06 §8.3's cascading deletion, this section details the cleanup path per storage:

```rust
async fn purge_tenant_data(&self, tenant: &TenantId) -> Result<PurgeReport, PurgeError> {
    let mut report = PurgeReport::default();
    
    // 1. Postgres - DROP PARTITION (seconds)
    for partition in self.list_tenant_partitions(tenant).await? {
        self.execute(&format!("DROP TABLE {}", partition)).await?;
        report.dropped_partitions += 1;
    }
    
    // 2. Postgres - delete rows referenced by tenant primary key (rest cascades automatically)
    sqlx::query!("DELETE FROM tenants WHERE id = $1", tenant.0).execute(...).await?;
    report.deleted_tenant_rows = 1;
    
    // 3. Redis - SCAN + DEL by prefix
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
    
    // 4. S3 - delete by prefix
    report.deleted_s3_objects += self.s3
        .delete_tenant_content(tenant).await?;
    report.deleted_s3_objects += self.s3_archive
        .delete_tenant_archive(tenant).await?;
    
    // 5. Filesystem (subprocess HOME)
    let home = format!("/var/lib/tars/tenants/{}/", tenant);
    if Path::new(&home).exists() {
        fs::remove_dir_all(&home).await?;
        report.fs_purged = true;
    }
    
    // 6. Secret manager namespace
    self.secret_manager.delete_namespace(&format!("tenants/{}", tenant)).await?;
    report.secrets_purged = true;
    
    // 7. Write tamper-resistant audit (kept even after the tenant is deleted)
    self.audit.write(AuditEvent::TenantDeleted {
        tenant: tenant.clone(),
        deleted_at: SystemTime::now(),
        report: report.clone(),
    }).await?;
    
    Ok(report)
}
```

**Key invariants**:
- The 7 steps must run in order (stop on any failure)
- Each step has its own metric (deleted_partitions / deleted_redis_keys / etc.)
- Records about this tenant in audit_events are **retained** — they are facts about "what once happened" and are not tenant data
- billing_events disappear only when their partition is dropped (monthly partitions; rows from before the tenant deletion in the current month survive until the partition expires). If compliance requires immediate deletion, separately query and delete that tenant's billing rows

---

## 12. Testing Strategy

### 12.1 Schema Tests

```rust
#[tokio::test]
async fn migrations_apply_cleanly_to_empty_db() {
    let pool = empty_test_postgres().await;
    sqlx::migrate!("./migrations/postgres").run(&pool).await.unwrap();
    
    // Verify key tables exist
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
    sqlx::migrate!("./migrations/postgres").run(&pool).await.unwrap();  // second run must succeed
}
```

### 12.2 Repository Contract Tests

```rust
// Same contract test runs twice - must pass against both SQLite and Postgres
async fn agent_event_log_contract_test(store: Arc<dyn AgentEventLog>) {
    let event = AgentEvent::TaskCreated { /* ... */ };
    let offset = store.append(event.clone()).await.unwrap();
    
    let fetched = store.fetch(traj_id, offset).await.unwrap();
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0], event);
}

#[tokio::test]
async fn sqlite_agent_event_log_satisfies_contract() {
    let store = SqliteAgentEventLog::new(test_sqlite_path()).await.unwrap();
    agent_event_log_contract_test(Arc::new(store)).await;
}

#[tokio::test]
async fn postgres_agent_event_log_satisfies_contract() {
    let store = PostgresAgentEventLog::new(test_pg_pool()).await.unwrap();
    agent_event_log_contract_test(Arc::new(store)).await;
}
```

### 12.3 Partition Tests

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

### 12.4 Backup/Restore Tests

```rust
#[tokio::test]
#[ignore]   // slow test, only runs nightly
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

## 13. Anti-pattern List

1. **Do not run cross-storage transactions on business paths** — Postgres + Redis + S3 each manage their own; converge via reconciliation.
2. **Do not put high-cardinality labels into table names** (`events_user_x_y` style) — use partitioning, not dynamic table names.
3. **Do not UPDATE the event log** — append-only; corrections are also expressed by appending new events.
4. **Do not SELECT * on big tables** — event rows can be 10KB+; fetch only what you need.
5. **Do not forget PostgreSQL's `WAL` configuration** — defaults for `wal_buffers` / `checkpoint_timeout` are not suitable for write-heavy workloads.
6. **Do not enable multi-process SQLite in Personal mode** — SQLite is a file-based database; concurrent multi-process writes conflict, strictly single-process.
7. **Do not bundle data migration into a Migration** — schema and data migrations are two steps; data migration is an independent idempotent script.
8. **Do not skip a storage during tenant deletion** — every step of the 7-item checklist must be observable and retryable.
9. **Do not add business indexes on audit / billing tables** — these tables are append-only + range-by-time queries, only the time index is needed.
10. **Do not ignore `pg_stat_statements`** — invisible slow queries cannot be optimized.
11. **Do not let ContentRef live forever** — orphan content (refcount=0) needs GC, otherwise the S3 bill spirals.
12. **Do not rename fields directly during schema evolution** — two steps: first add the new field with dual writes, then drop the old.
13. **Do not persist "must-not-lose" data in Redis** — even AOF everysec loses up to 1s; event/audit/billing must go to Postgres.
14. **Do not add retention policies in Personal mode** — it is the user's data; the user decides when to delete.
15. **Do not store secret values in the business DB** — store only SecretRef (path / var name); values are pulled at runtime by SecretResolver.

---

## 14. Contracts with Up- and Downstream

### Upstream (business code) commitments

- Access storage via Repository traits, never write SQL / Redis commands directly
- Do not assume cross-storage atomicity
- Large payloads (>16KB) go through ContentStore put first, then are referenced; never inlined into event payloads
- All queries on the business path are covered by indexes; review periodically

### Downstream (DB / Redis / S3) contracts

- DB schema migrations are managed via `sqlx::migrate!` with forward-compatibility constraints
- Redis is a performance-optimization layer; data loss is repaired by reconciliation
- S3 is cold storage + large objects + cross-region replication, managed via versioning + lifecycle rules

### Cross-node contracts

- Multi-replica apps sync state via Postgres / Redis; they do not communicate directly
- Postgres primary/replica lag < 100ms (synchronous_commit = on for primary, async replicas only for reads)
- Read replicas can serve most queries, but event writes must hit the primary

---

## 15. TODO and Open Questions

- [ ] Application-layer transparency for Postgres primary/standby switchover (PgBouncer vs HAProxy vs RDS Proxy)
- [ ] ContentRef GC frequency and policy (linear scan vs incremental marking)
- [ ] S3 cross-region replication latency monitoring (avoid data gaps during disaster recovery)
- [ ] SQLite mmap performance on Apple Silicon (whether enlarging mmap_size is worthwhile)
- [ ] Cost/complexity trade-off of dedicated physical DBs for large tenants (vs hash partition)
- [ ] How to host OLAP needs (read replica vs CDC into ClickHouse vs DuckDB)
- [ ] Postgres architecture for multi-region deployments (BDR vs Patroni vs cloud-managed)
- [ ] Dry-run / canary mechanism for schema migrations
- [ ] Audit table partition pruning query performance under 7-year retention
- [ ] Impact of Redis persistence policy on budget accuracy (lose 1s on restart vs full reconcile)
