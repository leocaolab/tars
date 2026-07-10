//! Typed-error → JS mapping for the handle-based surface (Doc 12 §7.3).
//!
//! The doc contract is a **discriminable class hierarchy**
//! (`TarsError → TarsConfigError / TarsProviderError / TarsUnknownRole / …`),
//! *not* one stringified message. napi-rs's JS `Error` carries a `.code`
//! property whose value is the Rust `Error<S>`'s status string
//! (`napi_create_error(env, code, msg)`), so we key the JS `.code` off the
//! Rust error's typed **variant** and keep the human `.message` = the real
//! underlying error text (never a sterile sentinel that throws the truth away).
//!
//! ```js
//! try { pipeline('critic'); }
//! catch (e) {
//!   if (e.code === 'TarsUnknownRole') { /* branch on the variant */ }
//! }
//! ```
//!
//! ## Sync vs async
//!
//! Every **synchronous** boundary (`init`, `provider(role)` /
//! `pipeline(role)`, …) returns `Result<T, String>` — the
//! napi alias `Result<T, S = Status>` with `S = String`, i.e.
//! `Result<T, napi::Error<String>>` — so `.code` is our domain string.
//! The **async** `complete()` path is locked by napi to
//! `napi::Result<T>` (`Error<Status>`), so its rejections carry
//! `code === 'GenericFailure'`; we still surface the provider error's typed
//! `kind` as the leading token of the message. See [`provider_reason`].

use napi::Error;

use tars_config::ConfigError;
use tars_provider::RegistryError;
use tars_types::{ProviderError, ProviderId};

/// A JS-facing error whose `.code` is a domain-typed string (see module docs).
pub(crate) type JsError = Error<String>;

/// `role` is not in the `[roles]` table. The message names the role and the
/// section to add — never a sterile sentinel.
pub(crate) fn unknown_role_to_js(role: &str) -> JsError {
    Error::new(
        "TarsUnknownRole".to_string(),
        format!(
            "role `{role}` is not configured — add a [roles.{role}] section with \
             `provider` and `model`"
        ),
    )
}

/// `role` names a provider the registry does not hold — `[roles]` and
/// `[providers]` disagree. Carries both real names in the message.
pub(crate) fn provider_not_registered_to_js(role: &str, provider: &ProviderId) -> JsError {
    Error::new(
        "TarsProviderNotRegistered".to_string(),
        format!(
            "role `{role}` names provider `{provider}`, which is not in the registry — \
             add a [providers.{provider}] section"
        ),
    )
}

/// Map a [`ConfigError`] (composition-root config load / parse / validate) to a
/// discriminable JS error `.code`.
pub(crate) fn config_to_js(err: ConfigError) -> JsError {
    Error::new("TarsConfigError".to_string(), err.to_string())
}

/// Map a [`RegistryError`] (provider-registry build / lookup) to
/// `TarsRegistryError`.
pub(crate) fn registry_to_js(err: RegistryError) -> JsError {
    Error::new("TarsRegistryError".to_string(), err.to_string())
}

/// Async-path reason for a provider-call [`ProviderError`]. napi's async
/// bridge fixes `.code` to `GenericFailure`, so we lead the message with the
/// provider error's typed `kind` (e.g. `rate_limited: …`) — the caller can
/// still branch on the kind token without a raw `.contains()` grep of prose.
pub(crate) fn provider_reason(err: ProviderError) -> napi::Error {
    napi::Error::from_reason(format!("{}: {err}", err.kind().as_str()))
}
