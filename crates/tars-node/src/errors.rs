//! Typed-error → JS mapping for the handle-based surface (Doc 12 §7.3).
//!
//! The doc contract is a **discriminable class hierarchy**
//! (`TarsError → TarsConfigError / TarsProviderError / TarsHandleError`),
//! *not* one stringified message. napi-rs's JS `Error` carries a `.code`
//! property whose value is the Rust `Error<S>`'s status string
//! (`napi_create_error(env, code, msg)`), so we key the JS `.code` off the
//! Rust error's typed **variant** and keep the human `.message` = the real
//! underlying error text (never a sterile sentinel that throws the truth away).
//!
//! ```js
//! try { handle.pipeline('critic'); }
//! catch (e) {
//!   if (e.code === 'TarsUnknownRole') { /* branch on the variant */ }
//! }
//! ```
//!
//! ## Sync vs async
//!
//! Every **synchronous** boundary (`init`, `Workspaces.open`,
//! `TarsHandle.provider` / `.pipeline`, …) returns `Result<T, String>` — the
//! napi alias `Result<T, S = Status>` with `S = String`, i.e.
//! `Result<T, napi::Error<String>>` — so `.code` is our domain string.
//! The **async** `complete()` path is locked by napi to
//! `napi::Result<T>` (`Error<Status>`), so its rejections carry
//! `code === 'GenericFailure'`; we still surface the provider error's typed
//! `kind` as the leading token of the message. See [`provider_reason`].

use napi::Error;

use tars_config::ConfigError;
use tars_handle::TarsError;
use tars_provider::RegistryError;
use tars_types::ProviderError;

/// A JS-facing error whose `.code` is a domain-typed string (see module docs).
pub(crate) type JsError = Error<String>;

/// Map a [`TarsError`] to a JS error with a discriminable `.code`. One code per
/// variant; the message is the variant's real `Display` text (the truth), never
/// a placeholder.
pub(crate) fn tars_to_js(err: TarsError) -> JsError {
    let code = match &err {
        TarsError::Registry(_) => "TarsRegistryError",
        TarsError::Storage(_) => "TarsStorageError",
        TarsError::MeltStore(_) => "TarsEventStoreError",
        TarsError::WorkspaceConfig(_) => "TarsWorkspaceConfigError",
        TarsError::Io(_) => "TarsIoError",
        TarsError::UnknownRole { .. } => "TarsUnknownRole",
    };
    Error::new(code.to_string(), err.to_string())
}

/// Map a [`ConfigError`] (global config load / parse) to `TarsConfigError`.
pub(crate) fn config_to_js(err: ConfigError) -> JsError {
    Error::new("TarsConfigError".to_string(), err.to_string())
}

/// Map a [`RegistryError`] (provider-registry build) to `TarsRegistryError`.
pub(crate) fn registry_to_js(err: RegistryError) -> JsError {
    Error::new("TarsRegistryError".to_string(), err.to_string())
}

/// Map a filesystem error at a handle boundary to `TarsIoError`.
pub(crate) fn io_to_js(err: std::io::Error) -> JsError {
    Error::new("TarsIoError".to_string(), err.to_string())
}

/// A poisoned `Workspaces` mutex (a prior panic while holding it). Surfaced
/// typed rather than `unwrap`-panicking the whole Node process.
pub(crate) fn poisoned() -> JsError {
    Error::new(
        "TarsPoisoned".to_string(),
        "workspaces registry lock poisoned by a prior panic".to_string(),
    )
}

/// Async-path reason for a provider-call [`ProviderError`]. napi's async
/// bridge fixes `.code` to `GenericFailure`, so we lead the message with the
/// provider error's typed `kind` (e.g. `rate_limited: …`) — the caller can
/// still branch on the kind token without a raw `.contains()` grep of prose.
pub(crate) fn provider_reason(err: ProviderError) -> napi::Error {
    napi::Error::from_reason(format!("{}: {err}", err.kind().as_str()))
}
