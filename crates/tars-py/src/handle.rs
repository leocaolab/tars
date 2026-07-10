//! Process-global spine for tars-py.
//!
//! [`init`] is the composition root: it installs the global
//! [`Config`](tars_config::Config) from `<home>/config.toml` and eagerly builds
//! the one [`ProviderRegistry`], so a bad provider or a missing key surfaces
//! here rather than on the first completion.
//!
//! Role resolution is a single lookup against `[roles]` — `critic` names a
//! `(provider, model)` pair in config, or it is an error. There is no fallback
//! to a tier, to a literal provider id, or to "the only provider": a role the
//! operator did not configure must say so.
//!
//! ```python
//! import tars
//! tars.init()                                   # global config + registry, once
//! with tars.context(session="s", tags=["dogfood"]):
//!     resp = tars.pipeline("critic").complete(model="…", user="review this")
//! ```
//!
//! NOTE: this public surface is PROVISIONAL — a fuller SDK redesign (and a
//! separate model-decoupling change) lands later. It uses the current
//! registry / pipeline / `ChatRequest` APIs unchanged.

use std::path::PathBuf;
use std::sync::Arc;

use pyo3::prelude::*;

use tars_config::{Config, resolve_home};
use tars_provider::{LlmProvider, ProviderRegistry};

use crate::context::ContextGuard;
use crate::errors::{config_to_py, provider_not_registered_to_py, runtime_to_py, unknown_role_to_py};
use crate::{Pipeline, Provider};

// ── Global init ───────────────────────────────────────────────────────

/// Install the process-global config and build the one provider registry.
///
/// `home` maps to `resolve_home` (`--tars_home` > `$TARS_HOME` > `~/.tars`) and
/// reads `<home>/config.toml`. Building the registry here is deliberate: a bad
/// provider declaration or a missing API key raises now, not on the first call.
///
/// **Not idempotent.** A second `init()` raises `TarsConfigError` — the process
/// already runs against a config this call did not provide, and silently
/// returning would hide that.
#[pyfunction]
#[pyo3(signature = (home = None))]
pub(crate) fn init(home: Option<PathBuf>) -> PyResult<()> {
    tars_config::global::init_tars(home).map_err(config_to_py)?;
    ProviderRegistry::init().map_err(|e| runtime_to_py("provider registry", e))?;
    Ok(())
}

/// Whether [`init`] has already run (config installed and registry built).
#[pyfunction]
pub(crate) fn is_initialized() -> bool {
    Config::is_loaded() && ProviderRegistry::try_global().is_some()
}

/// Resolve the tars home directory (`--tars_home` > `$TARS_HOME` > `~/.tars`)
/// without loading anything. `None` when no home is discoverable.
#[pyfunction]
#[pyo3(signature = (home = None))]
pub(crate) fn tars_home(home: Option<PathBuf>) -> Option<String> {
    resolve_home(home).map(|p| p.to_string_lossy().into_owned())
}

// ── Role → provider / pipeline ────────────────────────────────────────

/// Resolve `role` → a raw [`Provider`] (layer 1, no middleware) against the
/// process-global config + registry. Requires [`init`].
#[pyfunction]
pub(crate) fn provider(role: String) -> PyResult<Provider> {
    let (id, prov) = resolve_global_role(&role)?;
    Ok(Provider::from_provider(id, prov))
}

/// Resolve `role` → a middleware-wrapped [`Pipeline`] (the canonical default
/// chain) against the process-global config + registry. Requires [`init`].
#[pyfunction]
pub(crate) fn pipeline(role: String) -> PyResult<Pipeline> {
    let (id, prov) = resolve_global_role(&role)?;
    Ok(Pipeline::from_provider(id, prov, vec![], None))
}

/// Inspect the `[roles]` mapping: the provider id `role` binds to.
#[pyfunction]
pub(crate) fn role_provider(role: String) -> PyResult<String> {
    Ok(resolve_global_role(&role)?.0)
}

/// Inspect the `[roles]` mapping: the model `role` binds to.
#[pyfunction]
pub(crate) fn role_model(role: String) -> PyResult<String> {
    let cfg = Config::get();
    let entry = cfg.roles.get(&role).ok_or_else(|| unknown_role_to_py(&role))?;
    Ok(entry.model.clone())
}

/// One lookup, no guessing. Two distinct failures, each carrying what actually
/// went wrong: the role isn't configured, or it names a provider the registry
/// doesn't hold.
fn resolve_global_role(role: &str) -> PyResult<(String, Arc<dyn LlmProvider>)> {
    let registry =
        ProviderRegistry::global().map_err(|e| runtime_to_py("provider registry", e))?;
    let cfg = Config::get();
    let entry = cfg.roles.get(role).ok_or_else(|| unknown_role_to_py(role))?;
    let prov = registry
        .get(&entry.provider)
        .ok_or_else(|| provider_not_registered_to_py(role, &entry.provider))?;
    Ok((entry.provider.to_string(), prov))
}

/// A context manager that establishes a `RequestContext` for the calls inside
/// its `with` block (Doc 12 §6.2). Each `complete()` inside re-scopes
/// `RUN_CONTEXT` from it.
#[pyfunction]
#[pyo3(signature = (session = None, tags = None, tenant = None, trace = None))]
pub(crate) fn context(
    session: Option<String>,
    tags: Option<Vec<String>>,
    tenant: Option<String>,
    trace: Option<String>,
) -> ContextGuard {
    ContextGuard::new(session, tags, tenant, trace)
}
