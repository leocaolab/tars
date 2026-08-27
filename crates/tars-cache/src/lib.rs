//! LLM response cache.
//!
//! Two registry implementations behind one [`CacheRegistry`] trait:
//! [`MemoryCacheRegistry`] (L1 only, moka-backed in-process LRU) and
//! [`SqliteCacheRegistry`] (L1 + persistent L2). L3 (provider-side
//! `cachedContent` / `cache_control` handles) is not built yet.
//!
//! ## Cache key construction
//!
//! [`CacheKeyFactory::compute`] enforces:
//!
//! - **`hasher_version`** is the first byte hashed — bumping it
//!   invalidates the entire cache without a flush command. Use it as
//!   a kill-switch when a key-construction bug is discovered.
//! - **Tenant + IAM scopes prefix every key**. Without IAM scopes
//!   participating, two principals with different read-rights against
//!   the same RAG corpus would share the same cache slot — the classic
//!   IDOR hazard.
//! - **`temperature != 0`** → key construction fails fast with
//!   [`CacheError::NonDeterministic`]. Caching a stochastic output
//!   defeats the point.

mod clock;
mod error;
mod key;
mod policy;
mod registry;
mod sqlite;

pub use clock::{Clock, SystemClock};
pub use error::CacheError;
pub use key::{CacheKey, CacheKeyFactory};
pub use policy::{CacheLayerPolicy, CachePolicy};
pub use registry::{CacheRegistry, CachedResponse, MemoryCacheRegistry, MemoryCacheRegistryConfig};
pub use sqlite::{
    SqliteCacheRegistry, SqliteCacheRegistryConfig, default_personal_cache_path, open_at_path,
};

