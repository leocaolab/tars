//! [`SqliteCacheRegistry`] — Personal-mode persistent cache.
//!
//! Single SQLite file holds the response cache; one in-process moka
//! instance front-runs it as L1. Same `CacheRegistry` trait as the
//! pure-memory implementation, so middleware doesn't change shape.
//!
//! ## Why one type that holds both L1 + L2 (instead of composing)
//!
//! Doc 03 §4.3's lookup flow is "L1 → L2 → fill L1 on L2 hit". A
//! standalone `LayeredCacheRegistry<L1, L2>` adapter would express
//! that, but for personal mode there's only one process, so L1 and L2
//! are always paired with a fixed lifetime relationship. Collapsing
//! them avoids two layers of `Arc<dyn CacheRegistry>` indirection on
//! every hot-path lookup. When Team mode lands and L2 becomes
//! cross-instance Redis, *that's* when a composing adapter pays its
//! way (so each Redis impl is reusable with any L1 backend).
//!
//! ## Concurrency model
//!
//! `rusqlite::Connection` is `Send` but not `Sync`. We hold a single
//! connection inside `Arc<Mutex<...>>` and run every SQLite call inside
//! `tokio::task::spawn_blocking` so we never block the runtime. SQLite
//! WAL allows concurrent readers but our serialised access doesn't
//! exploit that — fine for cache workloads, where L1 absorbs the
//! contention.
//!
//! ## TTL handling
//!
//! Each row carries `expires_at_ms`. Lookups filter expired rows;
//! writes also do a best-effort sweep of expired rows to keep the file
//! from growing unboundedly under pure-write workloads. No background
//! janitor task — Doc 14 M3 will introduce a real one when EventStore
//! lands.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use moka::future::Cache as MokaCache;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::CacheError;
use crate::key::CacheKey;
use crate::policy::CachePolicy;
use crate::registry::{CacheRegistry, CachedResponse};

/// Every-N-writes interval at which we sweep expired rows. Cheap (a
/// single indexed DELETE) but we don't need it on every write.
const SWEEP_EVERY_N_WRITES: u64 = 64;

/// The on-disk schema version. Bump on incompatible changes; we
/// migrate forward by REPLACE-ing the schema and clearing the file
/// (this is a *cache* — wiping it is a free operation).
const SCHEMA_VERSION: i64 = 1;

/// Per-row default TTL when [`CachePolicy::l1_ttl`] is `None`. M1's L1
/// has 5 min as its in-memory TTL; the persistent layer is fine with
/// 24h since the file lives across runs.
const DEFAULT_L2_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Per-row default TTL for the in-process moka L1 mirror.
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
    l2: Arc<Mutex<Connection>>,
    l2_ttl: Duration,
    write_count: Arc<std::sync::atomic::AtomicU64>,
}

impl SqliteCacheRegistry {
    /// Open (creating if needed) the cache file at `path`. The parent
    /// directory must exist.
    pub fn open(config: SqliteCacheRegistryConfig) -> Result<Arc<Self>, CacheError> {
        let conn = Connection::open(&config.path).map_err(|e| {
            CacheError::Backend(format!("opening sqlite cache at {:?}: {e}", config.path))
        })?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;

        let l1 = MokaCache::builder()
            .max_capacity(config.l1_max_entries)
            .time_to_live(config.l1_ttl)
            .build();

        Ok(Arc::new(Self {
            l1,
            l2: Arc::new(Mutex::new(conn)),
            l2_ttl: config.l2_ttl,
            write_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }))
    }

    /// Open an in-memory SQLite cache — useful for tests that want
    /// L2 semantics without touching the filesystem.
    pub fn in_memory() -> Result<Arc<Self>, CacheError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| CacheError::Backend(format!("opening in-memory sqlite: {e}")))?;
        Self::pragma_setup(&conn)?;
        Self::migrate(&conn)?;

        let l1 = MokaCache::builder()
            .max_capacity(10_000)
            .time_to_live(DEFAULT_L1_TTL)
            .build();

        Ok(Arc::new(Self {
            l1,
            l2: Arc::new(Mutex::new(conn)),
            l2_ttl: DEFAULT_L2_TTL,
            write_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }))
    }

    fn pragma_setup(conn: &Connection) -> Result<(), CacheError> {
        // Concurrent readers + single writer + crash-safe (Doc 09 §4.2).
        // `query_row` for pragmas that return a value (journal_mode);
        // execute_batch for pure side-effect ones.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| CacheError::Backend(format!("pragma journal_mode: {e}")))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| CacheError::Backend(format!("pragma synchronous: {e}")))?;
        conn.pragma_update(None, "temp_store", "MEMORY")
            .map_err(|e| CacheError::Backend(format!("pragma temp_store: {e}")))?;
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<(), CacheError> {
        // user_version is the canonical SQLite-builtin migration marker.
        let current: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|e| CacheError::Backend(format!("read user_version: {e}")))?;

        if current == SCHEMA_VERSION {
            return Ok(());
        }
        if current != 0 {
            // Existing schema we don't know about — wipe (cache is
            // ephemeral; correctness > preservation).
            conn.execute("DROP TABLE IF EXISTS cache_entries", [])
                .map_err(|e| CacheError::Backend(format!("drop old schema: {e}")))?;
        }

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS cache_entries (
                fingerprint   BLOB    PRIMARY KEY,
                value         BLOB    NOT NULL,
                created_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_cache_expires
                ON cache_entries(expires_at_ms);
            "#,
        )
        .map_err(|e| CacheError::Backend(format!("create schema: {e}")))?;

        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| CacheError::Backend(format!("set user_version: {e}")))?;
        Ok(())
    }

    /// L2-only count of non-expired rows. Useful for diagnostics +
    /// tests; not in the trait surface to avoid pretending it's free.
    pub fn l2_entry_count(&self) -> Result<u64, CacheError> {
        let now = now_ms();
        let conn = self.l2.lock().expect("l2 mutex poisoned");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cache_entries WHERE expires_at_ms > ?",
                params![now],
                |r| r.get(0),
            )
            .map_err(|e| CacheError::Backend(format!("count rows: {e}")))?;
        Ok(n as u64)
    }
}

#[async_trait]
impl CacheRegistry for SqliteCacheRegistry {
    async fn lookup(
        &self,
        key: &CacheKey,
        policy: &CachePolicy,
    ) -> Result<Option<CachedResponse>, CacheError> {
        if !policy.l1 && !policy.l2 {
            return Ok(None);
        }

        // L1 fast path.
        if policy.l1 {
            if let Some(arc) = self.l1.get(&key.fingerprint).await {
                return Ok(Some((*arc).clone()));
            }
        }

        // L2 fall-through.
        if !policy.l2 {
            return Ok(None);
        }
        let l2 = self.l2.clone();
        let fp = key.fingerprint;
        let now = now_ms();
        let blob: Option<Vec<u8>> = tokio::task::spawn_blocking(move || -> Result<_, CacheError> {
            let conn = l2.lock().expect("l2 mutex poisoned");
            conn.query_row(
                "SELECT value FROM cache_entries WHERE fingerprint = ? AND expires_at_ms > ?",
                params![fp.as_slice(), now],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|e| CacheError::Backend(format!("l2 lookup: {e}")))
        })
        .await
        .map_err(|e| CacheError::Backend(format!("spawn_blocking: {e}")))??;

        let Some(blob) = blob else {
            return Ok(None);
        };
        let value: CachedResponse = serde_json::from_slice(&blob)
            .map_err(|e| CacheError::Backend(format!("decode l2 row: {e}")))?;

        // Refill L1 so the next lookup skips the SQLite hop.
        if policy.l1 {
            self.l1.insert(key.fingerprint, Arc::new(value.clone())).await;
        }
        Ok(Some(value))
    }

    async fn write(
        &self,
        key: CacheKey,
        value: CachedResponse,
        policy: &CachePolicy,
    ) -> Result<(), CacheError> {
        if !policy.l1 && !policy.l2 {
            return Ok(());
        }

        if policy.l1 {
            self.l1.insert(key.fingerprint, Arc::new(value.clone())).await;
        }
        if !policy.l2 {
            return Ok(());
        }

        let blob = serde_json::to_vec(&value)
            .map_err(|e| CacheError::Backend(format!("encode for l2: {e}")))?;
        let now = now_ms();
        let ttl_ms = policy
            .l2_ttl
            .unwrap_or(self.l2_ttl)
            .as_millis()
            .min(i64::MAX as u128) as i64;
        let expires_at = now.saturating_add(ttl_ms);

        let l2 = self.l2.clone();
        let fp = key.fingerprint;
        let writes_so_far =
            self.write_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::task::spawn_blocking(move || -> Result<(), CacheError> {
            let conn = l2.lock().expect("l2 mutex poisoned");
            conn.execute(
                "INSERT OR REPLACE INTO cache_entries
                   (fingerprint, value, created_at_ms, expires_at_ms)
                   VALUES (?, ?, ?, ?)",
                params![fp.as_slice(), blob, now, expires_at],
            )
            .map_err(|e| CacheError::Backend(format!("l2 write: {e}")))?;

            // Cheap janitor: every Nth write, sweep expired rows.
            if writes_so_far % SWEEP_EVERY_N_WRITES == 0 {
                let _ = conn.execute(
                    "DELETE FROM cache_entries WHERE expires_at_ms <= ?",
                    params![now],
                );
                // Errors here are non-fatal — best-effort cleanup.
            }
            Ok(())
        })
        .await
        .map_err(|e| CacheError::Backend(format!("spawn_blocking: {e}")))??;

        Ok(())
    }

    async fn invalidate(&self, key: &CacheKey) -> Result<(), CacheError> {
        self.l1.invalidate(&key.fingerprint).await;

        let l2 = self.l2.clone();
        let fp = key.fingerprint;
        tokio::task::spawn_blocking(move || -> Result<(), CacheError> {
            let conn = l2.lock().expect("l2 mutex poisoned");
            conn.execute(
                "DELETE FROM cache_entries WHERE fingerprint = ?",
                params![fp.as_slice()],
            )
            .map_err(|e| CacheError::Backend(format!("l2 invalidate: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| CacheError::Backend(format!("spawn_blocking: {e}")))??;
        Ok(())
    }

    fn entry_count(&self) -> u64 {
        // Cheap approximation — moka knows its own size; SQLite count
        // would need a query. This number is a diagnostic hint, not a
        // correctness signal, so L1's view is "good enough".
        self.l1.entry_count()
    }
}

/// Path the `tars-cli` (and other Personal-mode binaries) use by
/// default. Returns `None` only on platforms with no XDG-equivalent
/// cache dir, in which case callers should fall back to in-memory.
pub fn default_personal_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("tars").join("cache.sqlite"))
}

/// Open the cache at `path`, creating the parent directory if needed.
/// Convenience wrapper for callers that just want "give me a working
/// cache, you handle the housekeeping".
pub fn open_at_path(path: &Path) -> Result<Arc<SqliteCacheRegistry>, CacheError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CacheError::Backend(format!("create cache dir {parent:?}: {e}"))
        })?;
    }
    SqliteCacheRegistry::open(SqliteCacheRegistryConfig::new(path))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{CacheHitInfo, ChatResponse, ProviderId, StopReason, Usage};

    fn key(id: u8) -> CacheKey {
        let mut fp = [0u8; 32];
        fp[0] = id;
        CacheKey { fingerprint: fp, debug_label: format!("test-{id}") }
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
        let r = SqliteCacheRegistry::in_memory().unwrap();
        let k = key(1);
        let policy = CachePolicy { l1: true, l2: true, ..CachePolicy::default() };

        assert!(r.lookup(&k, &policy).await.unwrap().is_none());
        r.write(k.clone(), value("hi"), &policy).await.unwrap();
        let hit = r.lookup(&k, &policy).await.unwrap().unwrap();
        assert_eq!(hit.response.text, "hi");
        assert_eq!(hit.original_usage.input_tokens, 100);
    }

    #[tokio::test]
    async fn write_survives_close_and_reopen() {
        // The point of L2: a fresh process opens the same file and
        // sees the entry. This is the test that proves Doc 14 §7.2's
        // "second `tars run` hits cache" works in personal mode.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");

        {
            let r = open_at_path(&path).unwrap();
            let policy = CachePolicy { l1: true, l2: true, ..CachePolicy::default() };
            r.write(key(7), value("persisted"), &policy).await.unwrap();
            // Drop r → close connection → flush WAL on next open.
        }

        let r2 = open_at_path(&path).unwrap();
        let policy = CachePolicy { l1: true, l2: true, ..CachePolicy::default() };
        let hit = r2.lookup(&key(7), &policy).await.unwrap().unwrap();
        assert_eq!(hit.response.text, "persisted");
    }

    #[tokio::test]
    async fn l1_disabled_still_uses_l2() {
        let r = SqliteCacheRegistry::in_memory().unwrap();
        let policy_l2_only = CachePolicy {
            l1: false,
            l2: true,
            ..CachePolicy::default()
        };
        r.write(key(3), value("x"), &policy_l2_only).await.unwrap();

        // Now lookup with l1+l2: L1 misses (was never written), L2 hits,
        // and that hit refills L1 for next time.
        let policy_full = CachePolicy { l1: true, l2: true, ..CachePolicy::default() };
        let hit = r.lookup(&key(3), &policy_full).await.unwrap().unwrap();
        assert_eq!(hit.response.text, "x");
    }

    #[tokio::test]
    async fn fully_disabled_policy_writes_and_reads_nothing() {
        let r = SqliteCacheRegistry::in_memory().unwrap();
        let off = CachePolicy::off();
        r.write(key(1), value("x"), &off).await.unwrap();
        // And verify with default (l1) policy: nothing got persisted.
        assert!(r.lookup(&key(1), &CachePolicy::default()).await.unwrap().is_none());
        assert_eq!(r.l2_entry_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn invalidate_removes_from_both_layers() {
        let r = SqliteCacheRegistry::in_memory().unwrap();
        let policy = CachePolicy { l1: true, l2: true, ..CachePolicy::default() };
        r.write(key(1), value("x"), &policy).await.unwrap();
        assert!(r.lookup(&key(1), &policy).await.unwrap().is_some());
        r.invalidate(&key(1)).await.unwrap();
        assert!(r.lookup(&key(1), &policy).await.unwrap().is_none());
        assert_eq!(r.l2_entry_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn expired_l2_rows_are_filtered_at_lookup() {
        // TTL of 0 → row expires "immediately" (any time after the
        // insert qualifies). We can't time-pause across spawn_blocking
        // boundaries cleanly, so use the policy's explicit TTL.
        let r = SqliteCacheRegistry::in_memory().unwrap();
        let policy_short = CachePolicy {
            l1: false, // skip L1 so we test the L2 expiry filter directly
            l2: true,
            l2_ttl: Some(Duration::ZERO),
            ..CachePolicy::default()
        };
        r.write(key(2), value("ephemeral"), &policy_short).await.unwrap();
        // Sleep one ms so wall-clock advances past the zero-TTL row.
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(r.lookup(&key(2), &policy_short).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn distinct_keys_dont_collide() {
        let r = SqliteCacheRegistry::in_memory().unwrap();
        let policy = CachePolicy { l1: true, l2: true, ..CachePolicy::default() };
        r.write(key(1), value("a"), &policy).await.unwrap();
        r.write(key(2), value("b"), &policy).await.unwrap();
        assert_eq!(r.lookup(&key(1), &policy).await.unwrap().unwrap().response.text, "a");
        assert_eq!(r.lookup(&key(2), &policy).await.unwrap().unwrap().response.text, "b");
        assert_eq!(r.l2_entry_count().unwrap(), 2);
    }

    #[tokio::test]
    async fn schema_version_marker_is_set() {
        // Fresh DB should land at SCHEMA_VERSION.
        let r = SqliteCacheRegistry::in_memory().unwrap();
        let conn = r.l2.lock().unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
