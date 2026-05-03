//! LLM response cache — Doc 03.
//!
//! M1 ships **L1 only** (`MemoryCacheRegistry`, moka-backed in-process
//! LRU). L2 (Redis-or-SQLite shared cache) lands when `tars-storage`
//! exists. L3 (provider-side `cachedContent` / `cache_control` handles)
//! waits on the `ExplicitCacheProvider` sub-trait (TODO D-1).
//!
//! ## Cache key construction (Doc 03 §3.2)
//!
//! [`CacheKeyFactory::compute`] enforces:
//!
//! - **`hasher_version`** is the first byte hashed — bumping it
//!   invalidates the entire cache without a flush command. Use it as
//!   a kill-switch when a key-construction bug is discovered.
//! - **Tenant + IAM scopes prefix every key**. Without IAM scopes
//!   participating, two principals with different read-rights against
//!   the same RAG corpus would share the same cache slot — the
//!   classic IDOR pattern Doc 03 §3.1 calls out as the prime hazard.
//! - **`temperature != 0`** → key construction fails fast with
//!   [`CacheError::NonDeterministic`]. Caching a stochastic output
//!   defeats the point.
//! - **`ModelHint::Tier` and `ModelHint::Ensemble`** → fail fast.
//!   Routing must resolve to `Explicit` before the cache layer sees
//!   the request. (See Doc 03 §4.2 for the future tier-fingerprint
//!   second pass; not built yet.)

mod error;
mod key;
mod policy;
mod registry;
mod sqlite;

pub use error::CacheError;
pub use key::{CacheKey, CacheKeyFactory};
pub use policy::CachePolicy;
pub use registry::{
    CacheRegistry, CachedResponse, MemoryCacheRegistry, MemoryCacheRegistryConfig,
};
pub use sqlite::{
    default_personal_cache_path, open_at_path, SqliteCacheRegistry, SqliteCacheRegistryConfig,
};
