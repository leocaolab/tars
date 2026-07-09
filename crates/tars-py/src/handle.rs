//! Process-global spine for tars-py (post scope-facade).
//!
//! The old `Workspaces` / `Tars` per-scope classes are gone. Providers and
//! keys are global (built once by [`init`]); role resolution now runs against
//! the process-global [`Config`] + [`ProviderRegistry`] via
//! [`tars_handle::resolve_role`]. The store dir survives only as a path helper
//! ([`store_dir`]) the caller opens itself.
//!
//! ```python
//! import tars
//! tars.init()                                   # global config, once (~/.tars)
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
use tars_handle::{WorkspaceResolution, resolve_role, resolve_workspace_root, workspace_store_dir};
use tars_provider::{LlmProvider, ProviderRegistry};

use crate::context::ContextGuard;
use crate::errors::{handle_to_py, init_to_py, runtime_to_py};
use crate::{Pipeline, Provider};

// ── Global init ───────────────────────────────────────────────────────

/// Load the process-global config once and build the one provider registry
/// (Doc 06 process-isolation model).
///
/// `home` maps to `resolve_home` (`--tars_home` > `$TARS_HOME` > `~/.tars`) and
/// reads `<home>/config.toml`. Idempotent: a second call is a no-op (first load
/// wins). Providers / keys are global — built once, shared by every call.
#[pyfunction]
#[pyo3(signature = (home = None))]
pub(crate) fn init(home: Option<PathBuf>) -> PyResult<()> {
    match tars_handle::init_from_home(home) {
        // First-wins: a second init is a no-op for the caller (mirrors the old
        // idempotent `init`), not an error.
        Ok(()) | Err(tars_handle::InitError::AlreadyInitialized) => Ok(()),
        Err(e) => Err(init_to_py(e)),
    }
}

/// Whether [`init`] has already loaded the global config.
#[pyfunction]
pub(crate) fn is_initialized() -> bool {
    Config::is_loaded()
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

/// Inspect the `[roles]` mapping: resolve `role` to the provider id it binds
/// to, without building anything.
#[pyfunction]
pub(crate) fn role_provider(role: String) -> PyResult<String> {
    Ok(resolve_global_role(&role)?.0)
}

fn resolve_global_role(role: &str) -> PyResult<(String, Arc<dyn LlmProvider>)> {
    let registry =
        ProviderRegistry::global().map_err(|e| runtime_to_py("provider registry", e))?;
    let cfg = Config::get();
    let (id, prov) =
        resolve_role(&cfg.roles, &cfg.routing, &registry, role).map_err(handle_to_py)?;
    Ok((id.to_string(), prov))
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

// ── Kept path helpers (plain "where does it live", not a scope) ───────

/// Resolve `path` to a canonical workspace root for `tool` (walk-up; a
/// `.<tool>/` marker beats `.git`). `None` when neither a marker nor `.git` is
/// found up the tree.
#[pyfunction]
pub(crate) fn resolve_root(tool: String, path: PathBuf) -> PyResult<Option<String>> {
    let resolved = resolve_workspace_root(&tool, &path)
        .map_err(|e| runtime_to_py("resolve workspace root", e))?;
    Ok(match resolved {
        WorkspaceResolution::Workspace(root) => Some(root.to_string_lossy().into_owned()),
        WorkspaceResolution::Standalone => None,
    })
}

/// The per-project store dir for `tool` under `root`: `<root>/.<tool>/tars/`.
/// A plain path — the caller opens whatever stores it wants there.
#[pyfunction]
pub(crate) fn store_dir(tool: String, root: PathBuf) -> String {
    workspace_store_dir(&tool, &root)
        .to_string_lossy()
        .into_owned()
}
