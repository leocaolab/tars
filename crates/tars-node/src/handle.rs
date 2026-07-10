//! Process-global spine for tars-node.
//!
//! [`init`] is the composition root: it installs the global config from
//! `<home>/config.toml` and eagerly builds the one provider registry.
//! `provider` / `pipeline` resolve a role against them with a single `[roles]`
//! lookup — a role names a `(provider, model)` pair, or it is an error. No
//! fallback chain, no guessing.
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

use std::path::PathBuf;
use std::sync::Arc;

use napi_derive::napi;

use tars_config::{Config, resolve_home};
use tars_pipeline::{ChainOpts, LlmService};
use tars_provider::{LlmProvider, ProviderRegistry};
use tars_types::{ProviderId, RequestContext};

use crate::ctx::{JsContext, build_context, default_context};
use crate::errors::{
    JsError, config_to_js, provider_not_registered_to_js, registry_to_js, unknown_role_to_js,
};
use crate::{CompleteOptions, CompleteResult, Pipeline, drive_complete};

/// Install the process-global config and eagerly build the shared provider
/// registry. A bad provider / key surfaces here, at startup, not on the first
/// role resolution.
///
/// **Not idempotent.** A second `init()` rejects with `TarsConfigError`: the
/// process already runs against a config this call did not provide.
#[napi]
pub fn init(home: Option<String>) -> napi::Result<(), String> {
    tars_config::global::init_tars(home.map(PathBuf::from)).map_err(config_to_js)?;
    ProviderRegistry::init().map_err(registry_to_js)?;
    Ok(())
}

/// Whether [`init`] has run (config installed and registry built).
#[napi]
pub fn is_initialized() -> bool {
    Config::is_loaded() && ProviderRegistry::try_global().is_some()
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
    let (_id, prov, model) = resolve_global(&role)?;
    let inner = LlmService::of(prov, model);
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
    let (id, prov, model) = resolve_global(&role)?;
    let opts = ChainOpts::new(ProviderId::new(id.clone()));
    let inner = LlmService::default_chain(prov, model, opts);
    let ctx = ctx.map(build_context).unwrap_or_else(default_context);
    Ok(Pipeline::from_service(id, inner, ctx))
}

/// One `[roles]` lookup, no guessing. Two distinct failures, each naming what
/// actually went wrong.
fn resolve_global(
    role: &str,
) -> std::result::Result<(String, Arc<dyn LlmProvider>, String), JsError> {
    let registry = ProviderRegistry::global().map_err(registry_to_js)?;
    let cfg = Config::get();
    let entry = cfg.roles.get(role).ok_or_else(|| unknown_role_to_js(role))?;
    let prov = registry
        .get(&entry.provider)
        .ok_or_else(|| provider_not_registered_to_js(role, &entry.provider))?;
    Ok((entry.provider.to_string(), prov, entry.model.clone()))
}

/// Layer 1 provider handle — a raw backend bound to a role + call context.
/// Same `complete()` surface as [`Pipeline`] so a caller can swap one for the
/// other.
#[napi]
pub struct Provider {
    role: String,
    inner: LlmService,
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
        drive_complete(self.inner.clone(), self.ctx.clone(), opts).await
    }
}
