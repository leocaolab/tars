//! The handle-based spine (Doc 12 §7.2): `init` → [`Workspaces`] →
//! [`TarsHandle`] → [`crate::Pipeline`] / [`Provider`], mirroring the Doc 06
//! per-scope model. Wraps [`tars_handle::Tars`].
//!
//! **Async shape (Doc 12 §7).** JS's idiom is Promises, and the one genuinely
//! async operation — the LLM round-trip — stays async (`complete()` returns a
//! `Promise`). The setup operations here (`init`, `open`, role resolution) are
//! fast, local, and — crucially — carry **typed discriminable `.code`s**
//! (napi's async bridge would flatten every rejection's `.code` to
//! `GenericFailure`; see [`crate::errors`]). So they are synchronous by
//! deliberate choice; this is the documented deviation from the doc sketch's
//! `Promise`-returning `open`/`close`.
//!
//! **DAG is out of scope this pass** — see the `TODO(dag)` on
//! [`TarsHandle`]. Provider + pipeline are bound first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use napi_derive::napi;

use tars_config::{Config, resolve_home};
use tars_handle::{Tars, WorkspaceResolution, resolve_workspace_root};
use tars_pipeline::{LlmService, ProviderService};
use tars_types::{RequestContext, SessionId};

use crate::ctx::{JsContext, build_context, default_context};
use crate::errors::{JsError, config_to_js, io_to_js, poisoned, registry_to_js, tars_to_js};
use crate::{CompleteOptions, CompleteResult, Pipeline, drive_complete};

/// Load the process-global config **once** (Doc 06 §C1): providers + keys +
/// tier routing + any global `[roles]`, built from `~/.tars/config.toml`
/// (or `--tars_home` / `$TARS_HOME`; `home` overrides). Also eagerly builds
/// the shared provider registry so a bad provider/key surfaces here, at
/// startup, rather than on the first role resolution — and so opening a second
/// workspace never rebuilds it. Idempotent (a second call is a no-op).
#[napi]
pub fn init(home: Option<String>) -> napi::Result<(), String> {
    Config::load(home.map(PathBuf::from)).map_err(config_to_js)?;
    // Trigger the built-once global registry now (it reads `Config::get`).
    tars_provider::ProviderRegistry::global().map_err(registry_to_js)?;
    Ok(())
}

/// Whether [`init`] has run (the global config is loaded).
#[napi]
pub fn is_initialized() -> bool {
    Config::is_loaded()
}

/// Resolve the tars home dir that [`init`] would read from
/// (`home` > `$TARS_HOME` > `~/.tars`). `None` only when no home is
/// discoverable at all.
#[napi]
pub fn tars_home(home: Option<String>) -> Option<String> {
    resolve_home(home.map(PathBuf::from)).map(|p| p.to_string_lossy().into_owned())
}

/// Multi-workspace manager — the napi mirror of
/// `Mutex<HashMap<canonical_root, Tars>>` (Doc 06 §10). One per tool. Opening a
/// second workspace does **not** rebuild the global registry; `close` cancels +
/// drops the scope.
#[napi]
pub struct Workspaces {
    tool: String,
    open: Mutex<HashMap<PathBuf, Arc<Tars>>>,
}

#[napi]
impl Workspaces {
    /// A manager for `tool` (e.g. `"arc"`). Its marker dir (`.<tool>/`) is what
    /// [`Workspaces::open`] walks up to find.
    #[napi(constructor)]
    pub fn new(tool: String) -> Self {
        Self {
            tool,
            open: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve `path` to a canonical workspace root (`.<tool>/` marker beats
    /// `.git` — the monorepo rule; a bare dir with neither is opened as its own
    /// root) and return the cached-or-newly-opened handle.
    #[napi]
    pub fn open(&self, path: String) -> napi::Result<TarsHandle, String> {
        let root = self.resolve_root(&path)?;
        let mut map = self.open.lock().map_err(|_| poisoned())?;
        if !map.contains_key(&root) {
            let tars = Tars::for_workspace(&self.tool, &root).map_err(tars_to_js)?;
            map.insert(root.clone(), Arc::new(tars));
        }
        let inner = Arc::clone(map.get(&root).expect("inserted above"));
        Ok(TarsHandle::wrap(inner, self.tool.clone()))
    }

    /// The already-open handle for `path`, or `null` if not open. Does not open.
    #[napi]
    pub fn get(&self, path: String) -> napi::Result<Option<TarsHandle>, String> {
        let root = self.resolve_root(&path)?;
        let map = self.open.lock().map_err(|_| poisoned())?;
        Ok(map
            .get(&root)
            .map(|inner| TarsHandle::wrap(Arc::clone(inner), self.tool.clone())))
    }

    /// Close `path`: remove it from the map and cancel its scope. Returns
    /// whether a handle was open. The scope's stores drain once any in-flight
    /// job releases its `Arc` (deterministic `Drop`).
    #[napi]
    pub fn close(&self, path: String) -> napi::Result<bool, String> {
        let root = self.resolve_root(&path)?;
        let mut map = self.open.lock().map_err(|_| poisoned())?;
        match map.remove(&root) {
            Some(tars) => {
                tars.cancel();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Canonical roots currently open.
    #[napi]
    pub fn roots(&self) -> napi::Result<Vec<String>, String> {
        let map = self.open.lock().map_err(|_| poisoned())?;
        Ok(map
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect())
    }

    /// Close every open workspace (cancel + drop each scope).
    #[napi]
    pub fn close_all(&self) -> napi::Result<(), String> {
        let mut map = self.open.lock().map_err(|_| poisoned())?;
        for (_, tars) in map.drain() {
            tars.cancel();
        }
        Ok(())
    }

    /// Shared path→canonical-root resolution used by every method so `get` /
    /// `close` key on the exact same root `open` inserted under.
    fn resolve_root(&self, path: &str) -> std::result::Result<PathBuf, JsError> {
        let entry = PathBuf::from(path);
        match resolve_workspace_root(&self.tool, &entry).map_err(io_to_js)? {
            WorkspaceResolution::Workspace(root) => Ok(root),
            // No marker and no `.git`: open the given dir as its own root.
            WorkspaceResolution::Standalone => entry.canonicalize().map_err(io_to_js),
        }
    }
}

/// A per-scope handle bound to one workspace (or standalone). Cheap to hold —
/// shares the underlying [`Tars`] scope by `Arc`, plus the explicit
/// [`RequestContext`] this handle carries into its calls (default: a fresh
/// single-user context; override with [`TarsHandle::context`]).
#[napi]
pub struct TarsHandle {
    inner: Arc<Tars>,
    tool: String,
    ctx: RequestContext,
}

impl TarsHandle {
    fn wrap(inner: Arc<Tars>, tool: String) -> Self {
        Self {
            inner,
            tool,
            ctx: default_context(),
        }
    }
}

#[napi]
impl TarsHandle {
    /// A handle with no workspace (Doc 06 §7 standalone): store lives under
    /// `~/.tars/standalone/<tool>/<session>/`. `session` defaults to `"local"`.
    #[napi(factory)]
    pub fn standalone(tool: String, session: Option<String>) -> napi::Result<TarsHandle, String> {
        let session = SessionId::new(session.unwrap_or_else(|| "local".to_string()));
        let tars = Tars::standalone(&tool, session).map_err(tars_to_js)?;
        Ok(TarsHandle::wrap(Arc::new(tars), tool))
    }

    /// Open a handle for an explicit workspace `root` (no walk-up). Prefer
    /// [`Workspaces::open`] for the cached/marker-resolving path; this is the
    /// direct `--workspace <dir>` form.
    #[napi(factory)]
    pub fn for_workspace(tool: String, root: String) -> napi::Result<TarsHandle, String> {
        let tars = Tars::for_workspace(&tool, Path::new(&root)).map_err(tars_to_js)?;
        Ok(TarsHandle::wrap(Arc::new(tars), tool))
    }

    /// A derived handle that carries the given explicit [`JsContext`] into every
    /// call it makes (the FFI ctx boundary — see [`crate::ctx`]). Shares the same
    /// underlying scope; only the context differs.
    #[napi]
    pub fn context(&self, ctx: JsContext) -> TarsHandle {
        TarsHandle {
            inner: Arc::clone(&self.inner),
            tool: self.tool.clone(),
            ctx: build_context(ctx),
        }
    }

    /// Layer 1 — the raw provider for `role` (no middleware), resolved via the
    /// workspace `[roles]` map → global `[roles]` → tier → literal id.
    #[napi]
    pub fn provider(&self, role: String) -> napi::Result<Provider, String> {
        let provider = self.inner.provider(&role).map_err(tars_to_js)?;
        let inner: Arc<dyn LlmService> = ProviderService::new(provider);
        Ok(Provider {
            role,
            inner,
            ctx: self.ctx.clone(),
        })
    }

    /// Layer 2 — the middleware-wrapped pipeline for `role`, with this scope's
    /// event sink wired in. Same `complete()` surface as [`Provider`].
    #[napi]
    pub fn pipeline(&self, role: String) -> napi::Result<Pipeline, String> {
        let pipeline = self.inner.pipeline(&role).map_err(tars_to_js)?;
        Ok(Pipeline::from_service(
            role,
            Arc::new(pipeline),
            self.ctx.clone(),
        ))
    }

    /// The canonical workspace root this handle is bound to.
    #[napi]
    pub fn root(&self) -> String {
        self.inner.root().to_string_lossy().into_owned()
    }

    /// The tool this handle was opened for.
    #[napi(getter)]
    pub fn tool(&self) -> String {
        self.tool.clone()
    }

    /// Cancel this scope's in-flight work (idempotent). The `Workspaces`
    /// lifecycle owns removal from the map; this just fires the token.
    #[napi]
    pub fn close(&self) {
        self.inner.cancel();
    }

    // TODO(dag): `runtime() -> RuntimeHandle { run(plan, onEvent) }` + the
    // `Plan` builder (Doc 12 §7.3/§7.4). `Tars::runtime()` already yields the
    // `Arc<dyn Runtime>` backed by this scope's event store; binding the DAG
    // executor + Plan builder + event callback is the next pass. Out of scope
    // here — provider + pipeline are bound first.
}

/// Layer 1 provider handle — a raw backend bound to a role + this scope's ctx.
/// Same call surface as [`Pipeline`] so a caller can swap one for the other.
#[napi]
pub struct Provider {
    role: String,
    inner: Arc<dyn LlmService>,
    ctx: RequestContext,
}

#[napi]
impl Provider {
    /// The role name this provider was resolved for.
    #[napi(getter)]
    pub fn role(&self) -> String {
        self.role.clone()
    }

    /// Single non-streaming chat completion against the raw provider. Re-scopes
    /// `RUN_CONTEXT` with this handle's ctx at the boundary (Doc 06 §9).
    #[napi]
    pub async fn complete(&self, opts: CompleteOptions) -> napi::Result<CompleteResult> {
        drive_complete(Arc::clone(&self.inner), self.ctx.clone(), opts).await
    }
}
