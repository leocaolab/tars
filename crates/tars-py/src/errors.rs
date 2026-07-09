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
//! └── TarsHandleError           workspace-open / role-resolution (Doc 06 handle)
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
use pyo3::types::{PyDict, PyList};

use tars_config::ConfigError;
use tars_types::ValidationReason;
use tars_types::error::{ErrorClass, ProviderError};

create_exception!(tars._tars_py, TarsError, PyException);
create_exception!(tars._tars_py, TarsConfigError, TarsError);
create_exception!(tars._tars_py, TarsProviderError, TarsError);
create_exception!(tars._tars_py, TarsRuntimeError, TarsError);
// Doc 06 handle failures: opening a workspace (registry / store / io) or
// resolving a `role` to a provider. Rooted at `TarsError` so a catch-all
// still matches; carries a typed `kind` tag (+ `role`/`tried` for the
// unknown-role case) so callers branch on structure, not the message.
create_exception!(tars._tars_py, TarsHandleError, TarsError);
// Subclass of TarsProviderError — `isinstance(e, TarsProviderError)` still
// matches, so existing catch-all blocks keep working. Caller can branch
// on `except TarsRoutingExhaustedError` for typed access to
// `skipped_candidates` without adding `hasattr` checks on the parent.
create_exception!(tars._tars_py, TarsRoutingExhaustedError, TarsProviderError);

/// Map a `ConfigError` to its Python exception.
pub fn config_to_py(err: ConfigError) -> PyErr {
    TarsConfigError::new_err(err.to_string())
}

/// Map a [`tars_handle::TarsError`] to a `TarsHandleError`, preserving the
/// structured `role` + `tried` fields (and a `kind` tag) so a caller branches
/// on them rather than parsing the message. Its only variant is the
/// role-resolution failure (the old registry/store/io/workspace-config
/// failures went away with the scope facade).
pub fn handle_to_py(err: tars_handle::TarsError) -> PyErr {
    let message = err.to_string();
    Python::with_gil(|py| match build_handle_exc(py, &err, message) {
        Ok(exc) => exc,
        Err(decorate_err) => decorate_err,
    })
}

/// Build a decorated [`TarsHandleError`]: sets `kind` + the structured `role` /
/// `tried` fields for the role-resolution failure.
fn build_handle_exc(
    py: Python<'_>,
    err: &tars_handle::TarsError,
    message: String,
) -> PyResult<PyErr> {
    use tars_handle::TarsError as E;
    let exc = TarsHandleError::new_err(message);
    let value = exc.value(py);
    value.setattr("kind", "unknown_role")?;
    let E::UnknownRole { role, tried } = err;
    value.setattr("role", role)?;
    value.setattr("tried", tried.as_ref().map(|p| p.to_string()))?;
    Ok(exc)
}

/// Map a [`tars_handle::InitError`] (composition-root failure) to its Python
/// exception: config-load failures are config-shaped; registry-build failures
/// are runtime/wiring.
pub fn init_to_py(err: tars_handle::InitError) -> PyErr {
    use tars_handle::InitError as E;
    match err {
        E::Config(e) => config_to_py(e),
        E::Registry(e) => runtime_to_py("provider registry", e),
        E::AlreadyInitialized => TarsRuntimeError::new_err("tars already initialized"),
    }
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
    let exc = match err {
        ProviderError::NoCompatibleCandidate { .. } => TarsRoutingExhaustedError::new_err(message),
        _ => TarsProviderError::new_err(message),
    };

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
        ProviderError::NoCompatibleCandidate { skipped } => {
            // `skipped_candidates: list[(provider_id: str,
            // reasons: list[CompatibilityReason])]`. Each reason re-uses
            // the existing `CompatibilityReasonPy` class so callers get
            // the same kind/message/detail surface they get from
            // `Pipeline.check_compatibility`.
            let py_skipped = PyList::empty(py);
            for (id, reasons) in skipped {
                let id_str = id.to_string();
                let py_reasons = PyList::empty(py);
                for r in reasons {
                    let kind = r.kind().to_string();
                    let message = r.to_string();
                    let detail_json = compat_reason_detail(r);
                    let item = pyo3::Py::new(
                        py,
                        crate::CompatibilityReasonPy {
                            kind,
                            message,
                            detail_json,
                        },
                    )?;
                    py_reasons.append(item)?;
                }
                // `(str, list)` → `Bound<PyTuple>`; the tuple's
                // `IntoPyObject` is fallible, so `?` covers element
                // conversion without an `unwrap` panic path.
                let tuple = (id_str, py_reasons).into_pyobject(py)?;
                py_skipped.append(tuple)?;
            }
            value.setattr("skipped_candidates", py_skipped)?;
        }
        _ => {}
    }
    Ok(exc)
}

/// Mirror of the detail-extraction logic in `compatibility_to_py` so
/// the same structured fields show up on `TarsRoutingExhaustedError`'s
/// reasons as on `Pipeline.check_compatibility` results. Kept here
/// (and not factored to a shared helper) because lib.rs's
/// `compatibility_to_py` consumes `CompatibilityReason` by value while
/// here we have `&CompatibilityReason` — different ownership shapes.
fn compat_reason_detail(r: &tars_types::CompatibilityReason) -> Option<serde_json::Value> {
    use tars_types::CompatibilityReason as R;
    match r {
        R::ToolUseUnsupported { tool_count } => {
            Some(serde_json::json!({"tool_count": *tool_count}))
        }
        R::ThinkingUnsupported { mode } => Some(serde_json::json!({"mode": format!("{mode:?}")})),
        R::ContextWindowExceeded {
            estimated_prompt_tokens,
            max_context_tokens,
        } => Some(serde_json::json!({
            "estimated_prompt_tokens": *estimated_prompt_tokens,
            "max_context_tokens": *max_context_tokens,
        })),
        R::MaxOutputTokensExceeded { requested, max } => Some(serde_json::json!({
            "requested": *requested,
            "max": *max,
        })),
        R::StructuredOutputUnsupported | R::VisionUnsupported => None,
        _ => None,
    }
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
    m.add("TarsHandleError", py.get_type::<TarsHandleError>())?;
    m.add(
        "TarsRoutingExhaustedError",
        py.get_type::<TarsRoutingExhaustedError>(),
    )?;
    Ok(())
}
