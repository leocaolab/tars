//! Process-global spine for tars-node (post scope-facade).
//!
//! The old `Workspaces` / `TarsHandle` per-scope classes are gone. `init`
//! builds the process-global config + registry; `provider` / `pipeline` resolve
//! a role against them via [`tars_handle::resolve_role`]. The store dir survives
//! only as a path helper ([`workspace_store_dir`]) the caller opens itself.
//!
//! ```js
//! const tars = require('tars');
//! tars.init();
//! const p = tars.pipeline('critic', { session: 's', tags: ['dogfood'] });
//! await p.complete({ model: '…', user: 'review this' });
//! ```
//!
//! NOTE: this public surface is PROVISIONAL — a fuller SDK redesign (and a
//! separate model-decoupling change) lands later. It uses the current
//! registry / pipeline / `ChatRequest` APIs unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use napi_derive::napi;

use tars_config::{Config, resolve_home};
use tars_handle::{
    WorkspaceResolution, resolve_role, resolve_workspace_root as rs_resolve_root,
    workspace_store_dir as rs_store_dir,
};
use tars_pipeline::{LlmService, Pipeline as RsPipeline, PipelineOpts, ProviderService};
use tars_provider::{LlmProvider, ProviderRegistry};
use tars_types::{ProviderId, RequestContext};

use crate::ctx::{JsContext, build_context, default_context};
use crate::errors::{JsError, init_to_js, io_to_js, registry_to_js, tars_to_js};
use crate::{CompleteOptions, CompleteResult, Pipeline, drive_complete};

/// Load the process-global config + build the shared provider registry once
/// (Doc 06 §C1). Idempotent (a second call is a no-op). A bad provider / key
/// surfaces here, at startup, not on the first role resolution.
#[napi]
pub fn init(home: Option<String>) -> napi::Result<(), String> {
    match tars_handle::init_from_home(home.map(PathBuf::from)) {
        // First-wins: a second init is a no-op for the caller, not an error.
        Ok(()) | Err(tars_handle::InitError::AlreadyInitialized) => Ok(()),
        Err(e) => Err(init_to_js(e)),
    }
}

/// Whether [`init`] has run (the global config is loaded).
#[napi]
pub fn is_initialized() -> bool {
    Config::is_loaded()
}

/// Resolve the tars home dir (`home` > `$TARS_HOME` > `~/.tars`) without
/// loading anything. `None` only when no home is discoverable.
#[napi]
pub fn tars_home(home: Option<String>) -> Option<String> {
    resolve_home(home.map(PathBuf::from)).map(|p| p.to_string_lossy().into_owned())
}

/// Resolve `role` → a raw [`Provider`] (layer 1) against the process-global
/// config + registry. Optional `ctx` binds an explicit call context.
#[napi]
pub fn provider(role: String, ctx: Option<JsContext>) -> napi::Result<Provider, String> {
    let (_id, prov) = resolve_global(&role)?;
    let inner: Arc<dyn LlmService> = ProviderService::new(prov);
    Ok(Provider {
        role,
        inner,
        ctx: ctx.map(build_context).unwrap_or_else(default_context),
    })
}

/// Resolve `role` → a middleware-wrapped [`Pipeline`] (the canonical default
/// chain) against the process-global config + registry. Optional `ctx` binds
/// an explicit call context.
#[napi]
pub fn pipeline(role: String, ctx: Option<JsContext>) -> napi::Result<Pipeline, String> {
    let (id, prov) = resolve_global(&role)?;
    let opts = PipelineOpts::new(ProviderId::new(id.clone()));
    let rs = RsPipeline::default_chain(prov, opts);
    let inner: Arc<dyn LlmService> = Arc::new(rs);
    let ctx = ctx.map(build_context).unwrap_or_else(default_context);
    Ok(Pipeline::from_service(id, inner, ctx))
}

fn resolve_global(role: &str) -> std::result::Result<(String, Arc<dyn LlmProvider>), JsError> {
    let registry = ProviderRegistry::global().map_err(registry_to_js)?;
    let cfg = Config::get();
    let (id, prov) =
        resolve_role(&cfg.roles, &cfg.routing, &registry, role).map_err(tars_to_js)?;
    Ok((id.to_string(), prov))
}

/// Resolve `path` to a canonical workspace root for `tool` (walk-up; `.<tool>/`
/// marker beats `.git`). `null` when neither a marker nor `.git` is found.
#[napi]
pub fn resolve_workspace_root(tool: String, path: String) -> napi::Result<Option<String>, String> {
    match rs_resolve_root(&tool, Path::new(&path)).map_err(io_to_js)? {
        WorkspaceResolution::Workspace(root) => Ok(Some(root.to_string_lossy().into_owned())),
        WorkspaceResolution::Standalone => Ok(None),
    }
}

/// The per-project store dir for `tool` under `root`: `<root>/.<tool>/tars/`.
/// A plain path — the caller opens whatever stores it wants there.
#[napi]
pub fn workspace_store_dir(tool: String, root: String) -> String {
    rs_store_dir(&tool, Path::new(&root))
        .to_string_lossy()
        .into_owned()
}

/// Layer 1 provider handle — a raw backend bound to a role + call context.
/// Same `complete()` surface as [`Pipeline`] so a caller can swap one for the
/// other.
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
