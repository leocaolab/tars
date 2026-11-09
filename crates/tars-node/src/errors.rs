//! Turns tars errors into JS errors whose `.code` names the kind of failure and whose `.message`
//! is the underlying error's own text, so JS branches on `e.code`.
//!
//! ```js
//! try { pipeline('critic'); }
//! catch (e) {
//!   if (e.code === 'TarsUnknownRole') { /* branch on the variant */ }
//! }
//! ```
//!
//! `init`, `provider`, `pipeline`, `roleProvider`, `roleModel` and the `Pipeline` factories
//! return `napi::Result<T, String>`, so `.code` is a string of ours. napi fixes the async
//! `complete()` to `napi::Error<Status>`, whose `.code` is always `GenericFailure`; there the kind
//! leads the message instead (see [`llm_reason`]).

use napi::Error;

use tars_config::ConfigError;
use tars_llm::LlmError;
use tars_llm::RegistryError;
use tars_types::ProviderId;
use tars_utils::display_chain;

/// A JS-facing error whose `.code` is a domain-typed string (see module docs).
pub(crate) type JsError = Error<String>;

/// Says that `role` has no `[roles]` entry, naming the section to add.
pub(crate) fn unknown_role_to_js(role: &str) -> JsError {
    Error::new(
        "TarsUnknownRole".to_string(),
        format!(
            "role `{role}` is not configured — add a [roles.{role}] section with \
             `provider` and `model`"
        ),
    )
}

/// Says that `role` names a provider the registry does not hold, naming both.
pub(crate) fn provider_not_registered_to_js(role: &str, provider: &ProviderId) -> JsError {
    Error::new(
        "TarsProviderNotRegistered".to_string(),
        format!(
            "role `{role}` names provider `{provider}`, which is not in the registry — \
             add a [providers.{provider}] section"
        ),
    )
}

/// Refuses a handle asked for without a ctx, which would have no tenant to make its calls under;
/// the message says what to pass.
pub(crate) fn no_tenant_to_js() -> JsError {
    Error::new(
        "TarsNoTenant".to_string(),
        "no tenant: pass the caller's context, e.g. pipeline(role, { tenant: 'acme' })".to_string(),
    )
}

/// A ctx field that carries an id.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CtxField {
    Tenant,
    Session,
    Trace,
}

impl CtxField {
    fn code(self) -> &'static str {
        match self {
            Self::Tenant => "TarsInvalidTenant",
            Self::Session => "TarsInvalidSession",
            Self::Trace => "TarsInvalidTrace",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Tenant => "ctx.tenant",
            Self::Session => "ctx.session",
            Self::Trace => "ctx.trace",
        }
    }
}

/// Refuses a ctx field whose value is not a valid id (it is empty); `.code` names the field and
/// the message carries the id type's own refusal.
pub(crate) fn invalid_ctx_id_to_js(field: CtxField, err: serde_json::Error) -> JsError {
    Error::new(field.code().to_string(), format!("{}: {err}", field.path()))
}

pub(crate) fn config_to_js(err: ConfigError) -> JsError {
    Error::new("TarsConfigError".to_string(), display_chain(&err))
}

/// The `.code` of a provider registry that could not be built or does not hold a provider.
pub(crate) const REGISTRY_ERROR: &str = "TarsRegistryError";

pub(crate) fn registry_to_js(err: RegistryError) -> JsError {
    Error::new(REGISTRY_ERROR.to_string(), display_chain(&err))
}

/// Returns a failed call of the async path, its message led by the error's kind
/// (`rate_limited: …`) because napi fixes `.code` there.
pub(crate) fn llm_reason(err: LlmError) -> napi::Error {
    napi::Error::from_reason(format!("{}: {}", err.kind().as_str(), display_chain(&err)))
}
