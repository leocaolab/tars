//! The config + runtime-handle spine (Doc 06 / Doc 12 §6): the process-global
//! `init`, a [`Workspaces`] manager, and the per-workspace [`Handle`] (exposed
//! to Python as `Tars`).
//!
//! ```python
//! import tars
//! tars.init()                              # global config, once (~/.tars)
//! ws = tars.Workspaces("arc")              # one manager per tool
//! handle = ws.open("/repo/myproject")      # canonical root; store under .arc/tars/
//! with handle.context(session="s", tags=["dogfood"]):
//!     resp = handle.pipeline("critic").complete(model="…", user="review this")
//! ws.close("/repo/myproject")              # cancel() + store drain
//! ```
//!
//! Providers / keys are **global** (built once by `init`, shared by every
//! workspace); a second `open` never rebuilds the registry. The handle binds
//! that global registry to one workspace's `[roles]` map + observability sink.
//!
//! **Out of scope this pass (DAG):** `runtime().run(plan)`, the `Plan`
//! builder, and the `Runtime` pyclass. Provider + pipeline single-call land
//! first; the DAG wave is deferred.
//
// TODO(dag): bind `Tars::runtime()` → a `Runtime` pyclass with
// `run(plan, on_event=…)` over a `Plan` builder pyclass (Doc 12 §6.3 layer 3).
// The Rust seam already exists (`tars_handle::Tars::runtime()` returns an
// `Arc<dyn tars_runtime::Runtime>`); this pass deliberately does not expose it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use pyo3::prelude::*;

use tars_config::{Config, resolve_home};
use tars_handle::{Tars as HandleTars, WorkspaceResolution, resolve_workspace_root};
use tars_types::ids::SessionId;

use crate::context::ContextGuard;
use crate::errors::{TarsHandleError, config_to_py, handle_to_py};
use crate::{Pipeline, Provider};

/// Recover a poisoned lock rather than propagate the poison: the guarded map
/// is plain data, so a prior panic left no invariant broken — recovering
/// keeps one panic from cascading into `unwrap`-on-poison across every method.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ── Global config (init) ──────────────────────────────────────────────

/// Load the process-global config once (Doc 06 process-isolation model).
///
/// `home` maps to `Config::resolve_home` (`--tars_home` > `$TARS_HOME` >
/// `~/.tars`) and reads `<home>/config.toml`. Idempotent: a second call is a
/// no-op (first load wins), so multiple entry points may call it defensively.
/// Providers / keys are global — built once, shared by every workspace.
#[pyfunction]
#[pyo3(signature = (home = None))]
pub(crate) fn init(home: Option<PathBuf>) -> PyResult<()> {
    // NOTE: `<home>/.env` loading is intentionally NOT invoked here — the
    // config layer does not wire it today, and inventing it would be a
    // behavior the Rust side does not enforce. Add it here once/if
    // `Config::load` grows it.
    Config::load(home).map_err(config_to_py)
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

// ── Workspaces manager ────────────────────────────────────────────────

/// Multi-workspace manager — the Python mirror of Rust
/// `Mutex<HashMap<canonical_root, Tars>>`. One manager per `tool`. Opening a
/// second workspace does **not** rebuild the global registry.
#[pyclass]
pub(crate) struct Workspaces {
    tool: String,
    open: Mutex<HashMap<PathBuf, Py<Handle>>>,
}

impl Workspaces {
    /// Resolve `path` to the canonical workspace root for this tool (walk-up
    /// where a `.<tool>/` marker beats `.git`). A directory-less resolution
    /// (`Standalone`) is an error here — the caller wants `Tars.standalone`.
    fn resolve_root(&self, path: &PathBuf) -> PyResult<PathBuf> {
        let resolved = resolve_workspace_root(&self.tool, path)
            .map_err(|e| handle_to_py(tars_handle::TarsError::Io(e)))?;
        match resolved {
            WorkspaceResolution::Workspace(root) => Ok(root),
            WorkspaceResolution::Standalone => Err(TarsHandleError::new_err(format!(
                "no workspace root for {path:?}: no `.{}/` marker and no `.git` up the \
                 tree. Use `tars.Tars.standalone(tool, session)` for a directory-less \
                 session.",
                self.tool
            ))),
        }
    }
}

#[pymethods]
impl Workspaces {
    #[new]
    fn new(tool: String) -> Self {
        Self {
            tool,
            open: Mutex::new(HashMap::new()),
        }
    }

    #[getter]
    fn tool(&self) -> &str {
        &self.tool
    }

    /// Resolve `path` to a canonical workspace root and return the
    /// cached-or-new handle (bound to this workspace's `[roles]` + store and
    /// the global registry).
    fn open(&self, py: Python<'_>, path: PathBuf) -> PyResult<Py<Handle>> {
        let root = self.resolve_root(&path)?;
        let mut open = lock(&self.open);
        if let Some(h) = open.get(&root) {
            return Ok(h.clone_ref(py));
        }
        let inner = HandleTars::for_workspace(&self.tool, &root).map_err(handle_to_py)?;
        let handle = Py::new(py, Handle::wrap(inner))?;
        open.insert(root, handle.clone_ref(py));
        Ok(handle)
    }

    /// Deterministic close: cancel the handle then drop the manager's
    /// reference (Doc 06 §10). Drop drains once in-flight jobs release their
    /// own clones. No-op if `path` was never opened.
    fn close(&self, py: Python<'_>, path: PathBuf) -> PyResult<()> {
        let root = self.resolve_root(&path)?;
        let removed = lock(&self.open).remove(&root);
        if let Some(h) = removed {
            h.borrow(py).inner.cancel();
        }
        Ok(())
    }

    /// The cached handle for `path`, or `None` if not open.
    fn get(&self, py: Python<'_>, path: PathBuf) -> PyResult<Option<Py<Handle>>> {
        let root = self.resolve_root(&path)?;
        Ok(lock(&self.open).get(&root).map(|h| h.clone_ref(py)))
    }

    /// Canonical roots of all currently-open workspaces.
    fn roots(&self) -> Vec<String> {
        lock(&self.open)
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    /// Cancel + drop every open handle.
    fn close_all(&self, py: Python<'_>) {
        let drained: Vec<Py<Handle>> = lock(&self.open).drain().map(|(_, h)| h).collect();
        for h in &drained {
            h.borrow(py).inner.cancel();
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type = None, _exc_value = None, _traceback = None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<PyObject>,
        _exc_value: Option<PyObject>,
        _traceback: Option<PyObject>,
    ) -> bool {
        self.close_all(py);
        false
    }

    fn __repr__(&self) -> String {
        format!(
            "Workspaces(tool={:?}, open={})",
            self.tool,
            lock(&self.open).len()
        )
    }
}

// ── Per-workspace handle (Tars) ───────────────────────────────────────

/// Per-scope runtime handle (Doc 06 §6 C3), exposed to Python as `Tars`.
/// Binds the global registry to one workspace's `[roles]` map + observability
/// sink + cancellation. Obtain via [`Workspaces::open`] or the `standalone` /
/// `for_workspace` static constructors.
#[pyclass(name = "Tars")]
pub(crate) struct Handle {
    inner: std::sync::Arc<HandleTars>,
}

impl Handle {
    fn wrap(inner: HandleTars) -> Self {
        Self {
            inner: std::sync::Arc::new(inner),
        }
    }
}

#[pymethods]
impl Handle {
    /// Build a handle for an already-resolved workspace `root`. Requires
    /// [`init`] to have run (the global registry is built from it).
    #[staticmethod]
    fn for_workspace(tool: String, root: PathBuf) -> PyResult<Self> {
        HandleTars::for_workspace(&tool, &root)
            .map(Self::wrap)
            .map_err(handle_to_py)
    }

    /// Build a directory-less handle (Doc 06 CUJ-4): store lives under
    /// `~/.tars/standalone/<tool>/<session>/`. A missing / empty `session`
    /// gets a fresh UUID.
    #[staticmethod]
    #[pyo3(signature = (tool, session = None))]
    fn standalone(tool: String, session: Option<String>) -> PyResult<Self> {
        let session = session
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        HandleTars::standalone(&tool, SessionId::new(session))
            .map(Self::wrap)
            .map_err(handle_to_py)
    }

    /// Resolve `role` → a raw [`Provider`] (layer 1, no middleware), bound to
    /// this workspace's registry. Role resolution follows the handle's order
    /// (flat `[roles]` → tier → literal id → default tier → sole provider).
    fn provider(&self, role: String) -> PyResult<Provider> {
        let provider = self.inner.provider(&role).map_err(handle_to_py)?;
        let id = provider.id().to_string();
        Ok(Provider::from_provider(id, provider))
    }

    /// Resolve `role` → a middleware-wrapped [`Pipeline`] (layer 2), with this
    /// scope's observability sink wired into the event-emitter layer.
    fn pipeline(&self, role: String) -> PyResult<Pipeline> {
        // Resolve the provider first for its id + capabilities; then build the
        // sink-wired pipeline. Both go through the same role resolution (cheap
        // registry lookups), so the two calls agree on the chosen provider.
        let provider = self.inner.provider(&role).map_err(handle_to_py)?;
        let id = provider.id().to_string();
        let capabilities = provider.capabilities().clone();
        let built = self.inner.pipeline(&role).map_err(handle_to_py)?;
        Ok(Pipeline::from_built(id, built, capabilities))
    }

    /// Inspect the `[roles]` mapping: resolve `role` to the provider id it
    /// binds to, without building a pipeline.
    fn role_provider(&self, role: String) -> PyResult<String> {
        Ok(self
            .inner
            .provider(&role)
            .map_err(handle_to_py)?
            .id()
            .to_string())
    }

    /// A context manager that establishes a [`RequestContext`] for the calls
    /// inside its `with` block (Doc 12 §6.2). Each handle call inside
    /// re-scopes `RUN_CONTEXT` from it — the task-local never crosses FFI, so
    /// the scope is re-established per call from a thread-local set here.
    #[pyo3(signature = (session = None, tags = None, tenant = None, trace = None))]
    fn context(
        &self,
        session: Option<String>,
        tags: Option<Vec<String>>,
        tenant: Option<String>,
        trace: Option<String>,
    ) -> ContextGuard {
        ContextGuard::new(session, tags, tenant, trace)
    }

    /// The canonical workspace root this handle is bound to.
    #[getter]
    fn root(&self) -> String {
        self.inner.root().to_string_lossy().into_owned()
    }

    /// Cancel this scope's work (idempotent). Fire before releasing the handle
    /// so a hung job can't pin it (Doc 06 §10).
    fn close(&self) {
        self.inner.cancel();
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type = None, _exc_value = None, _traceback = None))]
    fn __exit__(
        &self,
        _exc_type: Option<PyObject>,
        _exc_value: Option<PyObject>,
        _traceback: Option<PyObject>,
    ) -> bool {
        self.inner.cancel();
        false
    }

    fn __repr__(&self) -> String {
        format!("Tars(root={:?})", self.inner.root())
    }
}
