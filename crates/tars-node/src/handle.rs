//! Resolves roles against the process-global config. [`init`] loads `<home>/config.toml` and
//! builds the provider registry once; [`provider`] and [`pipeline`] look a role up in `[roles]`,
//! and a role that is not there is an error. A handle does not take the role's model: the caller
//! names the model on each call, and [`role_model`] reads the role's for a caller that wants it.
//!
//! ```js
//! const tars = require('tars');
//! tars.init();
//! const p = tars.pipeline('critic', { tenant: 'acme', session: 's', tags: ['dogfood'] });
//! await p.complete({ model: '…', user: 'review this' });
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use napi_derive::napi;

use tars_config::{Config, ConfigError, resolve_home};
use tars_llm::LlmService;
use tars_llm::{LlmProvider, ProviderRegistry, RegistryError};
use tars_types::Caller;

use crate::ctx::{JsContext, build_context};
use crate::errors::{
    JsError, REGISTRY_ERROR, config_to_js, provider_not_registered_to_js, registry_to_js,
    unknown_role_to_js,
};
use crate::{CompleteOptions, CompleteResult, Pipeline, drive_complete};

/// The registry [`init`] built from the config it installed. It lives here, in the binding,
/// because the Node process is the composition root: one `init()` per process.
static REGISTRY: OnceLock<ProviderRegistry> = OnceLock::new();

/// Installs the process-global config and builds the provider registry, so a bad provider or key
/// fails here at startup rather than at the first role lookup.
///
/// A failed `init()` installs nothing and can be called again. **Not idempotent** once it
/// succeeded: a second `init()` rejects with `TarsConfigError`, since the process already runs
/// against a config this call did not provide.
///
/// # Errors
///
/// Rejects with `TarsConfigError` when the config does not load or is already installed, and
/// with `TarsRegistryError` when a provider cannot be built.
#[napi]
pub fn init(home: Option<String>) -> napi::Result<(), String> {
    if REGISTRY.get().is_some() {
        return Err(config_to_js(ConfigError::AlreadyInitialized));
    }
    let loaded = tars_config::load_tars(home.map(PathBuf::from)).map_err(config_to_js)?;
    let built =
        ProviderRegistry::from_config_default(loaded.config.providers(), &loaded.catalog, None)
            .map_err(registry_to_js)?;
    tars_config::install(loaded).map_err(config_to_js)?;
    // The config was installed just above, so a registry already here means a concurrent
    // `init()` won: the same refusal a second `install` gives.
    REGISTRY
        .set(built)
        .map_err(|_| config_to_js(ConfigError::AlreadyInitialized))
}

/// Returns whether [`init`] has run.
#[napi]
pub fn is_initialized() -> bool {
    REGISTRY.get().is_some()
}

/// Returns the tars home (`home`, else `$TARS_HOME`, else `~/.tars`) without loading anything;
/// `None` when none can be found.
#[napi]
pub fn tars_home(home: Option<String>) -> Option<String> {
    resolve_home(home.map(PathBuf::from)).map(|p| p.to_string_lossy().into_owned())
}

/// Returns the raw provider `role` maps to, with no middleware, making its calls under `ctx`.
///
/// # Errors
///
/// Rejects with `TarsNoTenant` or `TarsInvalid*` for a missing or bad `ctx`, `TarsRegistryError`
/// before [`init`], `TarsUnknownRole` for a role not in `[roles]`, and
/// `TarsProviderNotRegistered` for a role whose provider the registry does not hold.
#[napi(ts_args_type = "role: string, ctx: JsContext")]
pub fn provider(role: String, ctx: Option<JsContext>) -> napi::Result<Provider, String> {
    let ctx = build_context(ctx)?;
    let (_id, provider) = resolve_global(&role)?;
    Ok(Provider {
        role,
        provider,
        ctx,
    })
}

/// Returns the provider `role` maps to behind the default chain, making its calls under `ctx`.
///
/// # Errors
///
/// Rejects as [`provider`] does.
#[napi(ts_args_type = "role: string, ctx: JsContext")]
pub fn pipeline(role: String, ctx: Option<JsContext>) -> napi::Result<Pipeline, String> {
    let ctx = build_context(ctx)?;
    let (id, provider) = resolve_global(&role)?;
    Ok(Pipeline::from_provider(id, provider, ctx))
}

/// Returns the provider id `role` maps to.
///
/// # Errors
///
/// Rejects as [`provider`] does, less the `ctx` codes.
#[napi]
pub fn role_provider(role: String) -> napi::Result<String, String> {
    Ok(resolve_global(&role)?.0)
}

/// Returns the model `role` names, a `<series>@latest` spec resolved to the id it names today. A
/// handle does not use it unless the caller passes it to `complete({ model })`.
///
/// # Errors
///
/// Rejects with `TarsUnknownRole` for a role not in `[roles]`, and with `TarsRegistryError`
/// before [`init`] or when the spec does not resolve.
#[napi]
pub fn role_model(role: String) -> napi::Result<String, String> {
    let (registry, cfg) = installed()?;
    let entry = cfg
        .roles()
        .get(&role)
        .ok_or_else(|| unknown_role_to_js(&role))?;
    registry
        .resolve_model(&entry.provider, Some(&entry.model))
        .map_err(registry_to_js)
}

/// Returns the registry and the config [`init`] installed, or `TarsRegistryError` before it ran.
/// It never panics: a panic in a sync napi function can take the Node process down.
fn installed() -> std::result::Result<(&'static ProviderRegistry, &'static Config), JsError> {
    let registry = REGISTRY.get().ok_or_else(not_initialized)?;
    let cfg = Config::try_get().ok_or_else(not_initialized)?;
    Ok((registry, cfg))
}

fn not_initialized() -> JsError {
    napi::Error::new(
        REGISTRY_ERROR.to_string(),
        "tars is not initialized — call init() first".to_string(),
    )
}

/// Looks `role` up in `[roles]` and then in the registry; each miss is its own error.
fn resolve_global(role: &str) -> std::result::Result<(String, Arc<dyn LlmProvider>), JsError> {
    let (registry, cfg) = installed()?;
    let entry = cfg
        .roles()
        .get(role)
        .ok_or_else(|| unknown_role_to_js(role))?;
    let prov = registry.get(&entry.provider).map_err(|e| match e {
        RegistryError::UnknownProvider(_) => provider_not_registered_to_js(role, &entry.provider),
        other => registry_to_js(other),
    })?;
    Ok((entry.provider.to_string(), prov))
}

/// A raw provider, with no middleware, bound to a role and a call context. Its `complete()` is the
/// same as [`Pipeline`]'s, so either stands in for the other.
#[napi]
pub struct Provider {
    role: String,
    provider: Arc<dyn LlmProvider>,
    ctx: Caller,
}

#[napi]
impl Provider {
    /// Returns the role this provider was resolved for.
    #[napi(getter)]
    pub fn role(&self) -> String {
        self.role.clone()
    }

    /// Makes one non-streaming chat call on the raw provider.
    #[napi]
    pub async fn complete(&self, opts: CompleteOptions) -> napi::Result<CompleteResult> {
        let service = LlmService::of(Arc::clone(&self.provider), opts.model.clone());
        drive_complete(service, &self.ctx, opts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_lookup_before_init_is_a_registry_error_not_a_panic() {
        let err = role_model("critic".to_string()).unwrap_err();
        assert_eq!(err.status, "TarsRegistryError");
        let err = role_provider("critic".to_string()).unwrap_err();
        assert_eq!(err.status, "TarsRegistryError");
    }
}
