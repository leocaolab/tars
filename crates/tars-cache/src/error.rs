//! Cache-layer errors.
//!
//! These split into **"don't cache this request"** signals
//! (`NonDeterministic`, `UnresolvedTier`, `UncacheableEnsemble`) which
//! the middleware should treat as benign — just skip the cache and call
//! the inner provider — and **real failures**
//! (`Serialize`, `Backend`) which should be surfaced via tracing but
//! likewise never stop the request (Doc 03 §4.3 "缓存错误绝不传染业务").

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    /// `temperature > 0` (or omitted, since provider defaults are
    /// non-zero) — caching a stochastic output is meaningless.
    #[error("not cacheable: temperature must be explicitly 0.0")]
    NonDeterministic,

    /// `ModelHint::Tier(...)` reached the cache layer without Routing
    /// resolving it to a concrete model. See Doc 03 §4.2 for the
    /// planned tier-fingerprint workaround.
    #[error("not cacheable: ModelHint::Tier must be resolved by Routing first")]
    UnresolvedTier,

    /// `ModelHint::Ensemble(...)` reached the cache layer. Ensembles
    /// fan out to multiple providers and shouldn't share a single cache
    /// slot.
    #[error("not cacheable: ModelHint::Ensemble has no single cache identity")]
    UncacheableEnsemble,

    /// JSON serialisation of a request component failed (tools schema,
    /// tool-call args, structured-output schema). Should be impossible
    /// for in-memory `Value`s but reported for completeness.
    #[error("serialize: {0}")]
    Serialize(#[source] serde_json::Error),

    /// Underlying storage failure (moka eviction, future Redis
    /// connection error, …). Catchall — the middleware logs and
    /// degrades to a miss.
    #[error("backend: {0}")]
    Backend(String),
}

impl CacheError {
    /// "This request was never cacheable to begin with" — distinct
    /// from a real failure. The middleware uses this to decide whether
    /// to log loudly or quietly skip caching.
    pub fn is_not_cacheable(&self) -> bool {
        // Full match (not `matches!`) so adding a new variant forces an
        // explicit cacheable-vs-not-cacheable classification at compile time.
        match self {
            Self::NonDeterministic | Self::UnresolvedTier | Self::UncacheableEnsemble => true,
            Self::Serialize(_) | Self::Backend(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_cacheable_classifier_partitions_correctly() {
        assert!(CacheError::NonDeterministic.is_not_cacheable());
        assert!(CacheError::UnresolvedTier.is_not_cacheable());
        assert!(CacheError::UncacheableEnsemble.is_not_cacheable());
        assert!(!CacheError::Backend("eviction".into()).is_not_cacheable());
        // Serialize is a real failure, not a "not cacheable" signal —
        // exercise the variant explicitly so refactors of `is_not_cacheable`
        // can't silently misclassify it.
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(!CacheError::Serialize(serde_err).is_not_cacheable());
    }

    #[test]
    fn display_messages_are_stable() {
        // Operators rely on these strings in tracing output; pin them so
        // a typo in an `#[error(...)]` attribute can't slip through review.
        assert_eq!(
            CacheError::NonDeterministic.to_string(),
            "not cacheable: temperature must be explicitly 0.0"
        );
        assert_eq!(
            CacheError::UnresolvedTier.to_string(),
            "not cacheable: ModelHint::Tier must be resolved by Routing first"
        );
        assert_eq!(
            CacheError::UncacheableEnsemble.to_string(),
            "not cacheable: ModelHint::Ensemble has no single cache identity"
        );
        assert_eq!(
            CacheError::Backend("redis timeout".into()).to_string(),
            "backend: redis timeout"
        );
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let serde_msg = serde_err.to_string();
        assert_eq!(
            CacheError::Serialize(serde_err).to_string(),
            format!("serialize: {serde_msg}")
        );
    }
}
