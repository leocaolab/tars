//! [`CacheRegistry`] trait + the in-process L1 implementation.
//!
//! The L1 implementation uses [`moka::future::Cache`] which gives us:
//! - size-based eviction (entries, not bytes — close enough at this scale)
//! - per-entry TTL via `expire_after_create` policy
//! - lock-free reads
//!
//! M1 deliberately omits L2 (Redis / SQLite) and L3 (provider-side
//! handles). Adding either is a new `CacheRegistry` impl with the
//! same trait surface.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use moka::future::Cache as MokaCache;
use serde::{Deserialize, Serialize};

use tars_types::{ChatResponse, ProviderId, Usage};

use crate::error::CacheError;
use crate::key::CacheKey;
use crate::policy::CachePolicy;

/// What we put into the cache. Wraps the response with enough metadata
/// to (a) replay correctly and (b) tell observers "you saved $X".
///
/// `cached_at` uses `tars_types::systemtime_millis` so the on-wire/
/// on-disk format stays portable across platforms (default `SystemTime`
/// serde uses a tagged `(secs, nanos)` struct that isn't a stable
/// format — pre-Unix-epoch times even error out on some platforms).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedResponse {
    pub response: ChatResponse,
    #[serde(with = "tars_types::systemtime_millis")]
    pub cached_at: SystemTime,
    pub origin_provider: ProviderId,
    /// Usage figures from the original (cache-miss) call. Lets the
    /// "cost saved" stat be honest about what the cache replaced.
    pub original_usage: Usage,
}

/// Multi-level cache. M1 only has an in-memory L1 implementor
/// ([`MemoryCacheRegistry`]); future L2/L3 backends will be additional
/// types implementing this same trait.
#[async_trait]
pub trait CacheRegistry: Send + Sync + 'static {
    /// Look up a previously cached response for `key`. `Ok(None)` =
    /// miss; errors are typed but the middleware degrades them to
    /// misses (Doc 03 §4.3).
    async fn lookup(
        &self,
        key: &CacheKey,
        policy: &CachePolicy,
    ) -> Result<Option<CachedResponse>, CacheError>;

    /// Store a successful response. Caller is responsible for the
    /// "should we cache this?" decision (Doc 03 §5.1) — the registry
    /// just persists what it's given.
    async fn write(
        &self,
        key: CacheKey,
        value: CachedResponse,
        policy: &CachePolicy,
    ) -> Result<(), CacheError>;

    /// Drop a single entry. Used for explicit business-driven
    /// invalidation (the upstream code knows the cached answer is now
    /// stale — e.g. a doc was edited).
    async fn invalidate(&self, key: &CacheKey) -> Result<(), CacheError>;

    /// Best-effort entry count (for diagnostics; may lag actual state
    /// on heavily-concurrent workloads).
    fn entry_count(&self) -> u64;
}

#[derive(Clone, Debug)]
pub struct MemoryCacheRegistryConfig {
    /// Hard upper bound on entries. Eviction policy is W-TinyLFU
    /// (moka default) — surprisingly resilient to scan-style workloads.
    pub max_entries: u64,
    /// Default TTL when [`CachePolicy::l1_ttl`] is `None`. 5 minutes
    /// matches Doc 03 §2.1's "L1 typical".
    pub default_ttl: Duration,
}

impl Default for MemoryCacheRegistryConfig {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            default_ttl: Duration::from_secs(300),
        }
    }
}

#[derive(Clone)]
pub struct MemoryCacheRegistry {
    inner: MokaCache<[u8; 32], Arc<CachedResponse>>,
    default_ttl: Duration,
}

impl MemoryCacheRegistry {
    pub fn new(config: MemoryCacheRegistryConfig) -> Self {
        // Single global TTL applied to every entry. moka's plain
        // `insert` doesn't take a per-entry TTL — per-entry overrides
        // would require an `Expiry` policy impl plus a per-value TTL
        // field, which is deliberate non-goal for M1's L1. Callers
        // passing `policy.l1_ttl` get a debug log in `write` and the
        // entry still uses `default_ttl`. L2 (Sqlite) honours
        // `l1_ttl` on its own builder.
        let inner: MokaCache<[u8; 32], Arc<CachedResponse>> = MokaCache::builder()
            .max_capacity(config.max_entries)
            .time_to_live(config.default_ttl)
            .build();
        Self { inner, default_ttl: config.default_ttl }
    }

    pub fn default_arc() -> Arc<Self> {
        Arc::new(Self::new(MemoryCacheRegistryConfig::default()))
    }
}

#[async_trait]
impl CacheRegistry for MemoryCacheRegistry {
    async fn lookup(
        &self,
        key: &CacheKey,
        policy: &CachePolicy,
    ) -> Result<Option<CachedResponse>, CacheError> {
        if !policy.l1 {
            return Ok(None);
        }
        let hit = self.inner.get(&key.fingerprint).await;
        Ok(hit.map(|arc| (*arc).clone()))
    }

    async fn write(
        &self,
        key: CacheKey,
        value: CachedResponse,
        policy: &CachePolicy,
    ) -> Result<(), CacheError> {
        if !policy.l1 {
            return Ok(());
        }
        // Audit `tars-cache-src-registry-1`: previously this computed
        // `policy.l1_ttl.unwrap_or(self.default_ttl)` and immediately
        // discarded it with `let _ = ...`, while the constructor doc
        // claimed `l1_ttl` would override on a per-write basis. moka's
        // per-entry TTL actually requires an `Expiry` policy impl on
        // the builder — `Cache::insert` doesn't take a TTL. Be honest
        // about it: log a debug message when a caller tries to
        // override and we silently ignore. SqliteCacheRegistry's L2
        // path DOES honor `l1_ttl` (per its own builder); a future
        // moka builder rev that exposes per-entry insert can plumb
        // this through here.
        if let Some(requested) = policy.l1_ttl {
            if requested != self.default_ttl {
                tracing::debug!(
                    requested_ms = requested.as_millis() as u64,
                    actual_ms = self.default_ttl.as_millis() as u64,
                    "cache: MemoryCacheRegistry honors only the constructor-time \
                     default_ttl; per-write l1_ttl override ignored",
                );
            }
        }
        self.inner.insert(key.fingerprint, Arc::new(value)).await;
        Ok(())
    }

    async fn invalidate(&self, key: &CacheKey) -> Result<(), CacheError> {
        self.inner.invalidate(&key.fingerprint).await;
        Ok(())
    }

    fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{CacheHitInfo, StopReason};

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
    async fn write_then_lookup_round_trips() {
        let r = MemoryCacheRegistry::new(MemoryCacheRegistryConfig::default());
        let k = key(1);
        assert!(r.lookup(&k, &CachePolicy::default()).await.unwrap().is_none());
        r.write(k.clone(), value("hi"), &CachePolicy::default()).await.unwrap();
        let hit = r.lookup(&k, &CachePolicy::default()).await.unwrap().unwrap();
        assert_eq!(hit.response.text, "hi");
        assert_eq!(hit.original_usage.input_tokens, 100);
    }

    #[tokio::test]
    async fn lookup_with_l1_disabled_policy_misses_even_if_present() {
        let r = MemoryCacheRegistry::new(MemoryCacheRegistryConfig::default());
        let k = key(1);
        r.write(k.clone(), value("hi"), &CachePolicy::default()).await.unwrap();

        let no_l1 = CachePolicy { l1: false, ..CachePolicy::default() };
        assert!(r.lookup(&k, &no_l1).await.unwrap().is_none());
        // And a write with l1=false is a no-op.
        let k2 = key(2);
        r.write(k2.clone(), value("ho"), &no_l1).await.unwrap();
        assert!(r.lookup(&k2, &CachePolicy::default()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn invalidate_removes_entry() {
        let r = MemoryCacheRegistry::new(MemoryCacheRegistryConfig::default());
        let k = key(1);
        r.write(k.clone(), value("hi"), &CachePolicy::default()).await.unwrap();
        r.invalidate(&k).await.unwrap();
        // The lookup-path removal is synchronous w.r.t. the awaited
        // `invalidate`, so this assertion is the authoritative
        // contract test. `entry_count()` may lag because moka applies
        // its eviction bookkeeping via background sync.
        assert!(r.lookup(&k, &CachePolicy::default()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn distinct_keys_dont_collide() {
        let r = MemoryCacheRegistry::new(MemoryCacheRegistryConfig::default());
        r.write(key(1), value("a"), &CachePolicy::default()).await.unwrap();
        r.write(key(2), value("b"), &CachePolicy::default()).await.unwrap();
        assert_eq!(r.lookup(&key(1), &CachePolicy::default()).await.unwrap().unwrap().response.text, "a");
        assert_eq!(r.lookup(&key(2), &CachePolicy::default()).await.unwrap().unwrap().response.text, "b");
    }

    #[tokio::test]
    async fn entries_expire_after_default_ttl() {
        let r = MemoryCacheRegistry::new(MemoryCacheRegistryConfig {
            max_entries: 100,
            default_ttl: Duration::from_millis(100),
        });
        let k = key(1);
        r.write(k.clone(), value("hi"), &CachePolicy::default()).await.unwrap();
        assert!(
            r.lookup(&k, &CachePolicy::default()).await.unwrap().is_some(),
            "entry must be present immediately after write",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            r.lookup(&k, &CachePolicy::default()).await.unwrap().is_none(),
            "entry must be evicted after default_ttl elapses",
        );
    }

    #[tokio::test]
    async fn per_write_l1_ttl_override_is_silently_ignored() {
        // Documents the limitation called out in the audit comment in
        // `write`: callers may pass `policy.l1_ttl`, and we log + drop
        // it. The default_ttl of the registry wins.
        let r = MemoryCacheRegistry::new(MemoryCacheRegistryConfig {
            max_entries: 100,
            default_ttl: Duration::from_secs(300),
        });
        let k = key(1);
        let policy_short = CachePolicy {
            l1_ttl: Some(Duration::from_millis(50)),
            ..CachePolicy::default()
        };
        r.write(k.clone(), value("hi"), &policy_short).await.unwrap();
        // Sleep well past the requested-but-ignored override.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            r.lookup(&k, &CachePolicy::default()).await.unwrap().is_some(),
            "MemoryCacheRegistry honours only the constructor-time \
             default_ttl; a shorter per-write l1_ttl must not expire \
             the entry early",
        );
    }

    #[tokio::test]
    async fn capacity_eviction_caps_size_at_max_entries() {
        let r = MemoryCacheRegistry::new(MemoryCacheRegistryConfig {
            max_entries: 2,
            default_ttl: Duration::from_secs(300),
        });
        for i in 1u8..=5 {
            r.write(key(i), value(&format!("v{i}")), &CachePolicy::default())
                .await
                .unwrap();
        }
        // moka applies eviction via background sync; force it.
        r.inner.run_pending_tasks().await;
        assert!(
            r.entry_count() <= 2,
            "cache must cap at max_entries=2 (got {})",
            r.entry_count(),
        );

        // At least one of the early keys must be gone.
        let mut survivors = 0;
        for i in 1u8..=5 {
            if r.lookup(&key(i), &CachePolicy::default()).await.unwrap().is_some() {
                survivors += 1;
            }
        }
        assert!(
            survivors <= 2,
            "at most max_entries survivors expected, got {survivors}",
        );
    }
}
