//! Process-global, built-once [`ProviderRegistry`] singleton (Doc 06 §C2).
//!
//! Under process isolation (one tenant per process) a single immutable
//! registry is *correct*: build every declared provider once from the global
//! [`tars_config::Config`], share it as an `Arc`. Repeat calls just clone the
//! Arc — never a per-request or per-tenant rebuild.
//!
//! The registry is **derived state** — a pure function of `cfg.providers`, not
//! an independent fact. [`ProviderRegistry::init`] builds it eagerly at the
//! composition root, right after `tars_config::init_tars`, so a bad provider
//! declaration or a missing key surfaces *there* rather than on the first LLM
//! call, deep inside a pipeline. [`ProviderRegistry::global`] is then a pure
//! getter: no build, no side effect.
//!
//! This is the one static that would break shared-process multi-tenancy; we
//! choose process isolation (multi-tenant = N single-tenant processes)
//! precisely so it stays this simple.

use std::sync::{Arc, OnceLock};

use tars_config::Config;

use crate::registry::{ProviderRegistry, RegistryError};

/// The one process-global registry cell. This is the single registry global
/// in the workspace (Doc 06 §C2): [`ProviderRegistry::init`] populates it, and
/// every consumer reads it via [`ProviderRegistry::global`] /
/// [`ProviderRegistry::try_global`].
static REGISTRY: OnceLock<Arc<ProviderRegistry>> = OnceLock::new();

impl ProviderRegistry {
    /// Build the process-global registry from the installed [`Config`], eagerly.
    ///
    /// Call once at the composition root, after `tars_config::init_tars`:
    ///
    /// ```ignore
    /// tars_config::init_tars(home)?;      // primary state: the config
    /// ProviderRegistry::init()?;          // derived state: every provider, built now
    /// ```
    ///
    /// Building here means a bad provider declaration / missing feature / bad
    /// HTTP base surfaces at startup, not on the first request.
    ///
    /// Errors with [`RegistryError::ConfigNotInitialized`] when the config was
    /// never installed, and [`RegistryError::AlreadyInitialized`] on a second
    /// call — never a silent no-op over a registry the caller didn't build.
    pub fn init() -> Result<(), RegistryError> {
        if REGISTRY.get().is_some() {
            return Err(RegistryError::AlreadyInitialized);
        }
        let cfg = Config::try_get().ok_or(RegistryError::ConfigNotInitialized)?;
        let built = Arc::new(ProviderRegistry::from_config_default(&cfg.providers)?);
        REGISTRY
            .set(built)
            .map_err(|_| RegistryError::AlreadyInitialized)
    }

    /// The process-global provider registry. A **pure getter** — it never
    /// builds. [`ProviderRegistry::init`] must have run at the composition root.
    pub fn global() -> Result<Arc<ProviderRegistry>, RegistryError> {
        REGISTRY
            .get()
            .cloned()
            .ok_or(RegistryError::NotInitialized)
    }

    /// The process-global registry if it has already been built, else `None`.
    /// Non-building, non-erroring — for defensive callers/tests.
    pub fn try_global() -> Option<Arc<ProviderRegistry>> {
        REGISTRY.get().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole registry-cell contract, in ONE test.
    ///
    /// `REGISTRY` is a process-global `OnceLock`, and cargo runs a binary's
    /// tests on threads of one process. Splitting "before init" and "after init"
    /// into two tests makes them race for that cell: whichever runs second sees
    /// the other's state. So the ordered assertions live in a single test — the
    /// only shape that can observe the un-initialized state at all.
    #[test]
    fn registry_cell_reports_before_init_and_shares_one_arc_after() {
        // 1. Before init: `global()` is a pure getter. It reports, it does not
        //    lazily build, and a failed call leaves the cell empty.
        assert!(
            ProviderRegistry::try_global().is_none(),
            "precondition: nothing may have initialized the registry yet"
        );
        assert!(
            matches!(ProviderRegistry::global(), Err(RegistryError::NotInitialized)),
            "an un-initialized registry must report, never lazily build"
        );
        assert!(
            ProviderRegistry::try_global().is_none(),
            "a failed global() must not have populated the cell"
        );

        // 2. init() needs the config installed first.
        Config::set(Config::default()).expect("first install in this process");
        ProviderRegistry::init().expect("empty providers config builds cleanly");

        // 3. A second init is reported, never a silent no-op over a registry
        //    the caller did not build.
        assert!(
            matches!(ProviderRegistry::init(), Err(RegistryError::AlreadyInitialized)),
            "a second init must error"
        );

        // 4. After init, every getter hands back the SAME Arc — built once.
        let a = ProviderRegistry::global().expect("initialized above");
        let b = ProviderRegistry::global().expect("second call clones the cached Arc");
        assert!(Arc::ptr_eq(&a, &b), "global() must return the same Arc");
    }
}
