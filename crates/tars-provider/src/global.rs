//! Process-global, built-once [`ProviderRegistry`] singleton (Doc 06 §C2).
//!
//! Under process isolation (one tenant per process) a single immutable
//! registry is *correct*: build every declared provider once from the global
//! [`tars_config::Config`], share it as an `Arc`. Repeat calls just clone the
//! Arc — never a per-request or per-tenant rebuild.
//!
//! This is the one static that would break shared-process multi-tenancy; we
//! choose process isolation (multi-tenant = N single-tenant processes)
//! precisely so it stays this simple.

use std::sync::{Arc, OnceLock};

use tars_config::Config;

use crate::registry::{ProviderRegistry, RegistryError};

/// The one process-global registry cell. This is the single registry global
/// in the workspace (Doc 06 §C2): the facade `tars_handle::init` populates it,
/// and every consumer reads it via [`ProviderRegistry::global`] /
/// [`ProviderRegistry::try_global`]. It lives in `tars-provider` (not the
/// facade) because `global()` is defined here and the facade sits *above*
/// this crate — moving the cell up would invert the dependency.
static REGISTRY: OnceLock<Arc<ProviderRegistry>> = OnceLock::new();

impl ProviderRegistry {
    /// The process-global provider registry, built once from
    /// [`Config::get`]. The first caller eagerly builds every declared
    /// provider (see [`ProviderRegistry::from_config_default`]); every later
    /// caller just clones the shared `Arc`.
    ///
    /// Requires [`Config::load`] / [`Config::set`] to have run at the
    /// composition root — [`Config::get`] panics otherwise, the sanctioned
    /// startup contract.
    pub fn global() -> Result<Arc<ProviderRegistry>, RegistryError> {
        if let Some(existing) = REGISTRY.get() {
            return Ok(existing.clone());
        }
        let cfg = Config::get();
        let built = Arc::new(ProviderRegistry::from_config_default(&cfg.providers)?);
        // First writer wins. On a lost race our freshly-built copy is dropped
        // and we return the winner; either way every caller sees one registry.
        let _ = REGISTRY.set(built);
        Ok(REGISTRY
            .get()
            .expect("REGISTRY set above (by us or the race winner)")
            .clone())
    }

    /// The process-global registry if it has already been built, else `None`.
    /// Non-building, non-panicking — for defensive callers/tests that must not
    /// trigger a lazy build or require `Config` to be loaded.
    pub fn try_global() -> Option<Arc<ProviderRegistry>> {
        REGISTRY.get().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two calls resolve the *same* Arc (pointer-equal) — built once, shared.
    #[test]
    fn global_is_built_once_and_shared() {
        // Install a minimal config directly (bypasses the file load).
        if !Config::is_loaded() {
            Config::set(Config::default());
        }
        let a = ProviderRegistry::global().expect("empty providers config builds cleanly");
        let b = ProviderRegistry::global().expect("second call clones the cached Arc");
        assert!(Arc::ptr_eq(&a, &b), "global() must return the same Arc");
    }
}
