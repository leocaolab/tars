//! Python exception hierarchy for tars-py.
//!
//! Maps the typed Rust error enums (`tars_config::ConfigError`,
//! `tars_types::ProviderError`) to a small set of Python exception
//! classes so callers can `try / except` on the right thing instead of
//! string-matching the message of a generic `RuntimeError`.
//!
//! Hierarchy (all rooted at `TarsError`, which itself extends `Exception`):
//!
//! ```text
//! TarsError                     base — catch-all
//! ├── TarsConfigError           load / parse / validate / unknown provider id
//! ├── TarsProviderError         backend call failed (auth, rate-limit, parse, …)
//! ├── TarsRuntimeError          HTTP base / registry build / internal wiring
//! └── TarsRoleError             a `[roles]` name did not resolve to a provider
//! ```
//!
//! `TarsProviderError` carries three structured attributes the caller
//! can branch on without touching the message string:
//!
//! - `kind: str`      — variant name (e.g. `"rate_limited"`, `"auth"`)
//! - `retry_after: float | None` — seconds, when the provider hinted one
//! - `is_retriable: bool`        — convenience for fallback logic
//!
//! Some variants add their own typed attributes (B-20.v2):
//! `ValidationFailed` carries `validator: str` and
//! `validation_reason: {kind, message, detail}` so a fix-stage branches
//! on `validation_reason["kind"]` rather than parsing the message.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use tars_config::ConfigError;
use tars_types::ValidationReason;
use tars_types::error::{ErrorClass, ProviderError};

create_exception!(tars._tars_py, TarsError, PyException);
create_exception!(tars._tars_py, TarsConfigError, TarsError);
create_exception!(tars._tars_py, TarsProviderError, TarsError);
create_exception!(tars._tars_py, TarsRuntimeError, TarsError);
// A `role` did not resolve to a provider. Rooted at `TarsError` so a catch-all
// still matches; carries a typed `kind` tag plus the real `role` / `provider`
// that failed, so callers branch on structure rather than the message.
create_exception!(tars._tars_py, TarsRoleError, TarsError);

/// Map a `ConfigError` to its Python exception.
pub fn config_to_py(err: ConfigError) -> PyErr {
    TarsConfigError::new_err(err.to_string())
}

/// `role` is not in the `[roles]` table. Carries `kind` + `role` so a caller
/// branches on structure rather than parsing the message.
pub fn unknown_role_to_py(role: &str) -> PyErr {
    let exc = TarsRoleError::new_err(format!(
        "role `{role}` is not configured — add a [roles.{role}] section with \
         `provider` and `model`"
    ));
    decorate(exc, |value| {
        value.setattr("kind", "unknown_role")?;
        value.setattr("role", role)
    })
}

/// `role` resolves to a provider id the registry does not hold — the `[roles]`
/// entry and the `[providers]` table disagree. Carries the real role and the
/// real provider id, never a placeholder.
pub fn provider_not_registered_to_py(role: &str, provider: &tars_types::ProviderId) -> PyErr {
    let exc = TarsRoleError::new_err(format!(
        "role `{role}` names provider `{provider}`, which is not in the registry — \
         add a [providers.{provider}] section"
    ));
    decorate(exc, |value| {
        value.setattr("kind", "provider_not_registered")?;
        value.setattr("role", role)?;
        value.setattr("provider", provider.to_string())
    })
}

/// Attach structured attributes to `exc`; if decoration itself fails, that
/// failure is what the caller sees (never a silently half-built exception).
fn decorate(exc: PyErr, f: impl FnOnce(&Bound<'_, PyAny>) -> PyResult<()>) -> PyErr {
    Python::with_gil(|py| match f(&exc.value(py).clone().into_any()) {
        Ok(()) => exc,
        Err(decorate_err) => decorate_err,
    })
}

/// Map a generic runtime/wiring error (HTTP base build, registry build,
/// anything that's *not* a config or provider call failure) to
/// `TarsRuntimeError`.
pub fn runtime_to_py<E: std::fmt::Display>(context: &str, err: E) -> PyErr {
    TarsRuntimeError::new_err(format!("{context}: {err}"))
}

/// Map a `ProviderError` to a `TarsProviderError`. Sets `kind`,
/// `retry_after`, and `is_retriable` attributes so callers don't have
/// to parse the message.
pub fn provider_to_py(err: ProviderError) -> PyErr {
    let kind = provider_kind(&err);
    let retry_after = err.retry_after().map(|d| d.as_secs_f64());
    let is_retriable = matches!(
        err.class(),
        ErrorClass::Retriable | ErrorClass::MaybeRetriable
    );
    let message = err.to_string();

    Python::with_gil(|py| {
        // The doc contract promises `kind` / `retry_after` /
        // `is_retriable` (and variant-specific attributes) on the
        // returned exception. Decorating it can in principle fail on the
        // Python side (setattr, list/tuple construction), so we build it
        // in a fallible helper and propagate any failure as the returned
        // exception instead of silently handing back a half-populated
        // object that violates the contract.
        match build_provider_exc(py, &err, kind, retry_after, is_retriable, message) {
            Ok(exc) => exc,
            Err(decorate_err) => decorate_err,
        }
    })
}

/// Build a fully-decorated `TarsProviderError` (or subclass). Returns the
/// exception on success, or the `PyErr` that occurred while decorating it.
/// Keeping every Python operation behind `?` is what makes the documented
/// attribute contract enforceable rather than best-effort.
fn build_provider_exc(
    py: Python<'_>,
    err: &ProviderError,
    kind: &'static str,
    retry_after: Option<f64>,
    is_retriable: bool,
    message: String,
) -> PyResult<PyErr> {
    // Pick the right exception class. Subclassing for specific variants
    // gives callers idiomatic `except SubclassError as e` branching with
    // typed attributes; generic variants stay on the base
    // `TarsProviderError`.
    let exc = TarsProviderError::new_err(message);

    // Common attributes — set on every TarsProviderError (and therefore
    // on subclasses too via Python attribute lookup).
    let value = exc.value(py);
    value.setattr("kind", kind)?;
    value.setattr("retry_after", retry_after)?;
    value.setattr("is_retriable", is_retriable)?;

    // Variant-specific structured attributes.
    match err {
        ProviderError::UnknownTool { name } => {
            value.setattr("tool_name", name)?;
        }
        ProviderError::ValidationFailed { validator, reason } => {
            // `validation_reason: {kind, message, detail}` (B-20.v2) —
            // lets the caller's fix-stage branch on `reason["kind"]` +
            // structured `detail` instead of grepping the message.
            // `validator` is also surfaced for "which check failed".
            value.setattr("validator", validator)?;
            let d = PyDict::new(py);
            d.set_item("kind", reason.kind())?;
            d.set_item("message", reason.to_string())?;
            match validation_reason_detail(reason) {
                Some(v) => {
                    let json_mod = py.import("json")?;
                    let s = serde_json::to_string(&v).map_err(|e| {
                        pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "failed to serialize validation reason detail: {e}"
                        ))
                    })?;
                    let obj = json_mod.call_method1("loads", (s,))?;
                    d.set_item("detail", obj)?;
                }
                None => d.set_item("detail", py.None())?,
            }
            value.setattr("validation_reason", d)?;
        }
        _ => {}
    }
    Ok(exc)
}

/// Structured `detail` payload for a [`ValidationReason`], mirroring
/// the typed fields so the Python `validation_reason["detail"]` dict
/// carries machine-readable specifics (`field` / `length` / `max` /
/// `parse_error`) rather than only the rendered message. `Custom`
/// passes the caller's own detail through verbatim.
fn validation_reason_detail(r: &ValidationReason) -> Option<serde_json::Value> {
    use ValidationReason as R;
    match r {
        R::JsonShape { parse_error } => Some(serde_json::json!({ "parse_error": parse_error })),
        R::NotEmpty { field } => Some(serde_json::json!({ "field": field })),
        R::MaxLength { field, length, max } => Some(serde_json::json!({
            "field": field,
            "length": *length,
            "max": *max,
        })),
        R::Custom { detail, .. } => detail.clone(),
        // `#[non_exhaustive]` wildcard — a future variant exposes no
        // structured detail until taught here.
        _ => None,
    }
}

fn provider_kind(err: &ProviderError) -> &'static str {
    err.kind().as_str()
}

/// Register all exception classes in the Python module so callers can
/// `from tars import TarsError, TarsConfigError, …`.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("TarsError", py.get_type::<TarsError>())?;
    m.add("TarsConfigError", py.get_type::<TarsConfigError>())?;
    m.add("TarsProviderError", py.get_type::<TarsProviderError>())?;
    m.add("TarsRuntimeError", py.get_type::<TarsRuntimeError>())?;
    m.add("TarsRoleError", py.get_type::<TarsRoleError>())?;
    Ok(())
}
