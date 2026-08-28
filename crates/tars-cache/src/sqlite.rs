//! [`SqliteCacheRegistry`] — Personal-mode persistent cache.
//!
//! Single SQLite file holds the response cache; one in-process moka
//! instance front-runs it as L1. Same `CacheRegistry` trait as the
//! pure-memory implementation, so middleware doesn't change shape.
//!
//! ## Why one type that holds both L1 + L2 (instead of composing)
//!
//! The lookup flow is "L1 → L2 → fill L1 on L2 hit". A standalone
//! `LayeredCacheRegistry<L1, L2>` adapter would express
//! that, but for personal mode there's only one process, so L1 and L2
//! are always paired with a fixed lifetime relationship. Collapsing
//! them avoids two layers of `Arc<dyn CacheRegistry>` indirection on
//! every hot-path lookup. When Team mode lands and L2 becomes
//! cross-instance Redis, *that's* when a composing adapter pays its
//! way (so each Redis impl is reusable with any L1 backend).
//!
//! ## Concurrency model
//!
//! L2 is a [`tars_storage::Db`] over this cache's own DB file.
//! Every L2 call `.await`s the pool directly — no `spawn_blocking`, no
//! shared `Mutex<Connection>` — so we never block the runtime. SQLite
//! WAL allows concurrent readers; the pool doesn't specifically exploit
//! that, but that's fine for cache workloads where L1 absorbs the
//! contention.
//!
//! ## TTL handling
//!
//! Each row carries `expires_at_ms`. Lookups filter expired rows;
//! writes also do a best-effort sweep of expired rows to keep the file
//! from growing unboundedly under pure-write workloads. No background
//! janitor task.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache as MokaCache;
use tars_storage::Db;

use crate::clock::{Clock, system_clock};
use crate::error::CacheError;
use crate::key::CacheKey;
use crate::policy::CachePolicy;
use crate::registry::{CacheRegistry, CachedResponse};

/// Applied once at open on the cache's own pool;
/// `_sqlx_migrations` is the version-of-record.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/cache");

/// Cheap (a single indexed DELETE) but we don't need it on every write.
const SWEEP_EVERY_N_WRITES: u64 = 64;

/// The persistent layer is fine with 24h since the file lives across runs.
const DEFAULT_L2_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const DEFAULT_L1_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct SqliteCacheRegistryConfig {
    pub path: PathBuf,
    pub l1_max_entries: u64,
    pub l1_ttl: Duration,
    pub l2_ttl: Duration,
}

impl SqliteCacheRegistryConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            l1_max_entries: 10_000,
            l1_ttl: DEFAULT_L1_TTL,
            l2_ttl: DEFAULT_L2_TTL,
        }
    }
}

#[derive(Clone)]
pub struct SqliteCacheRegistry {
    l1: MokaCache<[u8; 32], Arc<CachedResponse>>,
    /// Cheap to clone.
    l2: Db,
    l2_ttl: Duration,
    write_count: Arc<std::sync::atomic::AtomicU64>,
    /// Used for every TTL-expiry decision, so expiry is testable without sleeping.
    clock: Arc<dyn Clock>,
}

const DEFAULT_L1_MAX_ENTRIES: u64 = 10_000;

impl SqliteCacheRegistry {
    /// The composition root opens it once
    /// and hands it in; the cache carries a handle, never a path. Runs this
    /// cache's migrator on it and builds the L1 moka mirror.
    pub async fn new(db: Db) -> Result<Arc<Self>, CacheError> {
        Self::new_with_clock(
            db,
            system_clock(),
            DEFAULT_L1_MAX_ENTRIES,
            DEFAULT_L1_TTL,
            DEFAULT_L2_TTL,
        )
        .await
    }

    pub async fn new_with_clock(
        db: Db,
        clock: Arc<dyn Clock>,
        l1_max_entries: u64,
        l1_ttl: Duration,
        l2_ttl: Duration,
    ) -> Result<Arc<Self>, CacheError> {
        db.migrate(&MIGRATOR)
            .await
            .map_err(|e| CacheError::Backend(format!("schema migration: {e}")))?;

        let l1 = MokaCache::builder()
            .max_capacity(l1_max_entries)
            .time_to_live(l1_ttl)
            .build();

        Ok(Arc::new(Self {
            l1,
            l2: db,
            l2_ttl,
            write_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            clock,
        }))
    }

    /// The parent directory must exist.
    pub async fn open(config: SqliteCacheRegistryConfig) -> Result<Arc<Self>, CacheError> {
        Self::open_with_clock(config, system_clock()).await
    }

    pub async fn open_with_clock(
        config: SqliteCacheRegistryConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, CacheError> {
        let pool = Db::open_sqlite(&config.path).await.map_err(|e| {
            CacheError::Backend(format!("opening sqlite cache at {:?}: {e}", config.path))
        })?;
        Self::new_with_clock(
            pool,
            clock,
            config.l1_max_entries,
            config.l1_ttl,
            config.l2_ttl,
        )
        .await
    }

    /// Useful for tests that want L2 semantics without touching the filesystem.
    pub async fn in_memory() -> Result<Arc<Self>, CacheError> {
        Self::in_memory_with_clock(system_clock()).await
    }

    /// Lets a test advance time and assert expiry without sleeping.
    pub async fn in_memory_with_clock(clock: Arc<dyn Clock>) -> Result<Arc<Self>, CacheError> {
        let pool = Db::sqlite_in_memory()
            .await
            .map_err(|e| CacheError::Backend(format!("opening in-memory sqlite: {e}")))?;
        Self::new_with_clock(
            pool,
            clock,
            DEFAULT_L1_MAX_ENTRIES,
            DEFAULT_L1_TTL,
            DEFAULT_L2_TTL,
        )
        .await
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

#[async_trait]
impl CacheRegistry for SqliteCacheRegistry {
    async fn lookup(
        &self,
        key: &CacheKey,
        policy: &CachePolicy,
    ) -> Result<Option<CachedResponse>, CacheError> {
        if !policy.l1.is_enabled() && !policy.l2.is_enabled() {
            return Ok(None);
        }

        if policy.l1.is_enabled() {
            if let Some(arc) = self.l1.get(&key.fingerprint).await {
                return Ok(Some((*arc).clone()));
            }
        }

        if !policy.l2.is_enabled() {
            return Ok(None);
        }
        let fp = key.fingerprint;
        let now = self.now_ms();
        let blob: Option<Vec<u8>> = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT value FROM cache_entries WHERE fingerprint = ? AND expires_at_ms > ?",
        )
        .bind(fp.to_vec())
        .bind(now)
        .fetch_optional(self.l2.sqlite())
        .await
        .map_err(|e| CacheError::Backend(format!("l2 lookup: {e}")))?;

        let Some(blob) = blob else {
            return Ok(None);
        };
        let value: CachedResponse = serde_json::from_slice(&blob)
            .map_err(|e| CacheError::Backend(format!("decode l2 row: {e}")))?;

        // Refill L1 so the next lookup skips the SQLite hop.
        if policy.l1.is_enabled() {
            self.l1
                .insert(key.fingerprint, Arc::new(value.clone()))
                .await;
        }
        Ok(Some(value))
    }

    async fn write(
        &self,
        key: CacheKey,
        value: CachedResponse,
        policy: &CachePolicy,
    ) -> Result<(), CacheError> {
        if !policy.l1.is_enabled() && !policy.l2.is_enabled() {
            return Ok(());
        }

        if policy.l1.is_enabled() {
            self.l1
                .insert(key.fingerprint, Arc::new(value.clone()))
                .await;
        }
        if !policy.l2.is_enabled() {
            return Ok(());
        }

        let blob = serde_json::to_vec(&value)
            .map_err(|e| CacheError::Backend(format!("encode for l2: {e}")))?;
        let now = self.now_ms();
        // `l2_ttl_effective` returns `None` if L2 is off; we already
        // returned above when `!policy.l2` so this is equivalent to
        // `policy.l2_ttl` here — but going through the accessor keeps
        // future refactors honest (no chance of a reader landing here
        // that forgot the `!policy.l2` guard above).
        let ttl_ms = policy
            .l2_ttl_effective()
            .unwrap_or(self.l2_ttl)
            .as_millis()
            .min(i64::MAX as u128) as i64;
        let expires_at = now.saturating_add(ttl_ms);

        let fp = key.fingerprint;
        let writes_so_far = self
            .write_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        sqlx::query(
            "INSERT OR REPLACE INTO cache_entries
               (fingerprint, value, created_at_ms, expires_at_ms)
               VALUES (?, ?, ?, ?)",
        )
        .bind(fp.to_vec())
        .bind(blob)
        .bind(now)
        .bind(expires_at)
        .execute(self.l2.sqlite())
        .await
        .map_err(|e| CacheError::Backend(format!("l2 write: {e}")))?;

        // Cheap janitor: every Nth write, sweep expired rows.
        if writes_so_far % SWEEP_EVERY_N_WRITES == 0 {
            // [arc:intentional-handle] reason: the sweep is opportunistic
            // GC, not part of the write's correctness contract — the row
            // we just inserted is durable regardless. A failed sweep must
            // not fail the caller's write, but it can be the first sign of
            // disk-full / IO trouble, so surface it via a warn carrying the
            // error object instead of dropping it silently.
            if let Err(e) = sqlx::query("DELETE FROM cache_entries WHERE expires_at_ms <= ?")
                .bind(now)
                .execute(self.l2.sqlite())
                .await
            {
                tracing::warn!(error = %e, "cache: l2 expired-row sweep failed (non-fatal)");
            }
        }

        Ok(())
    }

    async fn invalidate(&self, key: &CacheKey) -> Result<(), CacheError> {
        self.l1.invalidate(&key.fingerprint).await;

        let fp = key.fingerprint;
        sqlx::query("DELETE FROM cache_entries WHERE fingerprint = ?")
            .bind(fp.to_vec())
            .execute(self.l2.sqlite())
            .await
            .map_err(|e| CacheError::Backend(format!("l2 invalidate: {e}")))?;
        Ok(())
    }

    fn entry_count(&self) -> u64 {
        // This number is a diagnostic hint, not a
        // correctness signal, so L1's view is "good enough".
        self.l1.entry_count()
    }
}

/// Returns `None` only on platforms with no XDG-equivalent
/// cache dir, in which case callers should fall back to in-memory.
pub fn default_personal_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("tars").join("cache.sqlite"))
}

pub async fn open_at_path(path: &Path) -> Result<Arc<SqliteCacheRegistry>, CacheError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CacheError::Backend(format!("create cache dir {parent:?}: {e}")))?;
    }
    SqliteCacheRegistry::open(SqliteCacheRegistryConfig::new(path)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheLayerPolicy;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::SystemTime;
    use tars_types::{CacheHitInfo, ChatResponse, ProviderId, StopReason, Usage};

    /// Test clock whose "now" is a settable/advanceable `AtomicI64`, so
    /// TTL expiry can be exercised deterministically without sleeping.
    struct FakeClock(AtomicI64);

    impl FakeClock {
        fn new(start_ms: i64) -> Arc<Self> {
            Arc::new(Self(AtomicI64::new(start_ms)))
        }
        fn advance(&self, delta_ms: i64) {
            self.0.fetch_add(delta_ms, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// L2-only count of non-expired rows. Test-only assertion helper,
    /// scoped to this `mod tests` so it doesn't appear as a `pub(crate)`
    /// fn in the production source surface. Production callers should
    /// observe L2 through the cache's `lookup` / `insert` surface and
    /// let metrics flow through the telemetry middleware.
    impl SqliteCacheRegistry {
        pub(super) async fn l2_entry_count(&self) -> Result<u64, CacheError> {
            let now = self.now_ms();
            let n: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM cache_entries WHERE expires_at_ms > ?")
                    .bind(now)
                    .fetch_one(self.l2.sqlite())
                    .await
                    .map_err(|e| CacheError::Backend(format!("count rows: {e}")))?;
            Ok(n as u64)
        }
    }

    fn key(id: u8) -> CacheKey {
        let mut fp = [0u8; 32];
        fp[0] = id;
        CacheKey {
            fingerprint: fp,
            debug_label: format!("test-{id}"),
        }
    }

    fn value(text: &str) -> CachedResponse {
        CachedResponse {
            response: ChatResponse {
                actual_model: "m".into(),
                text: text.into(),
                thinking: String::new(),
                tool_calls: vec![],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
                cache_hit: CacheHitInfo::default(),
                validation_summary: Default::default(),
                created: 0,
            },
            cached_at: SystemTime::now(),
            origin_provider: ProviderId::new("test_p"),
            original_usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn write_then_lookup_round_trips_in_memory() {
        let r = SqliteCacheRegistry::in_memory().await.unwrap();
        let k = key(1);
        let policy = CachePolicy {
            l1: CacheLayerPolicy::Default,
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        };

        assert!(r.lookup(&k, &policy).await.unwrap().is_none());
        r.write(k.clone(), value("hi"), &policy).await.unwrap();
        let hit = r.lookup(&k, &policy).await.unwrap().unwrap();
        assert_eq!(hit.response.text, "hi");
        assert_eq!(hit.original_usage.input_tokens, 100);
    }

    #[tokio::test]
    async fn write_survives_close_and_reopen() {
        // The point of L2: a fresh process opens the same file and
        // sees the entry — a second `tars run` hits cache in personal
        // mode.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");

        {
            let r = open_at_path(&path).await.unwrap();
            let policy = CachePolicy {
                l1: CacheLayerPolicy::Default,
                l2: CacheLayerPolicy::Default,
                l3: CacheLayerPolicy::Disabled,
            };
            r.write(key(7), value("persisted"), &policy).await.unwrap();
            // Drop r → close pool → flush WAL on next open.
        }

        let r2 = open_at_path(&path).await.unwrap();
        let policy = CachePolicy {
            l1: CacheLayerPolicy::Default,
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        };
        let hit = r2.lookup(&key(7), &policy).await.unwrap().unwrap();
        assert_eq!(hit.response.text, "persisted");
    }

    #[tokio::test]
    async fn l1_disabled_still_uses_l2() {
        let r = SqliteCacheRegistry::in_memory().await.unwrap();
        let policy_l2_only = CachePolicy {
            l1: CacheLayerPolicy::Disabled,
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        };
        r.write(key(3), value("x"), &policy_l2_only).await.unwrap();

        // Now lookup with l1+l2: L1 misses (was never written), L2 hits,
        // and that hit refills L1 for next time.
        let policy_full = CachePolicy {
            l1: CacheLayerPolicy::Default,
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        };
        let hit = r.lookup(&key(3), &policy_full).await.unwrap().unwrap();
        assert_eq!(hit.response.text, "x");
    }

    #[tokio::test]
    async fn fully_disabled_policy_writes_and_reads_nothing() {
        let r = SqliteCacheRegistry::in_memory().await.unwrap();
        let off = CachePolicy::off();
        r.write(key(1), value("x"), &off).await.unwrap();
        // And verify with default (l1) policy: nothing got persisted.
        assert!(
            r.lookup(&key(1), &CachePolicy::default())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(r.l2_entry_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn invalidate_removes_from_both_layers() {
        let r = SqliteCacheRegistry::in_memory().await.unwrap();
        let policy = CachePolicy {
            l1: CacheLayerPolicy::Default,
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        };
        r.write(key(1), value("x"), &policy).await.unwrap();
        assert!(r.lookup(&key(1), &policy).await.unwrap().is_some());
        r.invalidate(&key(1)).await.unwrap();
        assert!(r.lookup(&key(1), &policy).await.unwrap().is_none());
        assert_eq!(r.l2_entry_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn expired_l2_rows_are_filtered_at_lookup() {
        // TTL of 0 → row expires "immediately" (any time after the
        // insert qualifies). Use the policy's explicit TTL and let a
        // short real-time sleep advance the wall clock past it.
        let r = SqliteCacheRegistry::in_memory().await.unwrap();
        let policy_short = CachePolicy {
            l1: CacheLayerPolicy::Disabled,
            l2: CacheLayerPolicy::Override {
                ttl: Duration::ZERO,
            },
            l3: CacheLayerPolicy::Disabled,
        };
        r.write(key(2), value("ephemeral"), &policy_short)
            .await
            .unwrap();
        // Sleep one ms so wall-clock advances past the zero-TTL row.
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(r.lookup(&key(2), &policy_short).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn distinct_keys_dont_collide() {
        let r = SqliteCacheRegistry::in_memory().await.unwrap();
        let policy = CachePolicy {
            l1: CacheLayerPolicy::Default,
            l2: CacheLayerPolicy::Default,
            l3: CacheLayerPolicy::Disabled,
        };
        r.write(key(1), value("a"), &policy).await.unwrap();
        r.write(key(2), value("b"), &policy).await.unwrap();
        assert_eq!(
            r.lookup(&key(1), &policy)
                .await
                .unwrap()
                .unwrap()
                .response
                .text,
            "a"
        );
        assert_eq!(
            r.lookup(&key(2), &policy)
                .await
                .unwrap()
                .unwrap()
                .response
                .text,
            "b"
        );
        assert_eq!(r.l2_entry_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn l2_ttl_expiry_is_testable_via_injected_clock() {
        // The whole point of injecting the clock: prove TTL expiry
        // deterministically, without sleeping. Start the clock at a
        // fixed instant, write an L2 row with a 1s TTL, then jump the
        // clock past it and assert the lookup filters the expired row.
        let clock = FakeClock::new(1_000_000);
        let r = SqliteCacheRegistry::in_memory_with_clock(clock.clone())
            .await
            .unwrap();
        // L1 off so the lookup exercises L2's clock-driven TTL filter,
        // not moka's own (real-time) expiry.
        let policy = CachePolicy {
            l1: CacheLayerPolicy::Disabled,
            l2: CacheLayerPolicy::Override {
                ttl: Duration::from_millis(1000),
            },
            l3: CacheLayerPolicy::Disabled,
        };

        r.write(key(9), value("soon-expires"), &policy)
            .await
            .unwrap();

        // Still within the TTL window: present.
        assert_eq!(
            r.lookup(&key(9), &policy)
                .await
                .unwrap()
                .unwrap()
                .response
                .text,
            "soon-expires"
        );
        assert_eq!(r.l2_entry_count().await.unwrap(), 1);

        // Advance the fake clock past the TTL — no sleeping.
        clock.advance(1_500);

        assert!(r.lookup(&key(9), &policy).await.unwrap().is_none());
        assert_eq!(r.l2_entry_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn baseline_migration_is_applied() {
        // Fresh DB should have sqlx's baseline migration recorded as
        // applied — `_sqlx_migrations` is the version-of-record.
        let r = SqliteCacheRegistry::in_memory().await.unwrap();
        let max_version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(r.l2.sqlite())
            .await
            .unwrap();
        assert_eq!(max_version, 1);
    }
}
