//! PyO3 binding for `tars-pipeline::OutputValidator`.
//!
//! Lets Python callers write validators as plain Python callables and
//! attach them to a `Pipeline` via the `validators=` kwarg on
//! `Pipeline.from_default` / `from_config` / `from_str`. Mirrors
//! Stage 3's `PyTool` pattern for the analogous `Tool` trait.
//!
//! ## Caller surface (Python side)
//!
//! ```python
//! import tars
//!
//! def validate_json_shape(req, resp):
//!     try:
//!         json.loads(resp["text"])
//!         return tars.Pass()
//!     except json.JSONDecodeError as e:
//!         return tars.Reject(reason=f"not JSON: {e}")
//!
//! p = tars.Pipeline.from_default(
//!     "qwen_coder_local",
//!     validators=[("json_shape", validate_json_shape)],
//! )
//! ```
//!
//! ## Outcome shape
//!
//! Validators return one of 4 pyclass-based outcome instances:
//!
//! - `tars.Pass()` — empty
//! - `tars.Reject(reason: str)` — always Permanent (no retry)
//! - `tars.FilterText(text: str, dropped: list[str]=[])`
//! - `tars.Annotate(metrics: dict[str, Any]={})`
//!
//! `FilterText` is the v1 Filter shape — text-only (preserves the
//! original `ChatResponse`'s other fields, replacing only `.text`).
//! Full-Response Filter (modify `tool_calls` / `usage` / etc.) is
//! deferred to W3+ when actual users need it.
//!
//! ## Pure-function contract
//!
//! Same as the underlying [`OutputValidator`] trait. Python callbacks
//! MUST be deterministic (same `(req, resp)` → same outcome) and
//! side-effect-free. Cache×Validator design relies on this. Callers
//! who need IO go to the evaluator framework (Doc 16) where async +
//! non-determinism are first-class.

use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use serde_json::Value as JsonValue;

use tars_pipeline::OutputValidator;
use tars_types::{ChatRequest, ChatResponse, ValidationOutcome};

// ── Outcome pyclasses ────────────────────────────────────────────────

/// "Response is acceptable as-is, no metrics." Validators return this
/// for the happy path.
#[pyclass(frozen, name = "Pass")]
#[derive(Debug)]
pub(crate) struct PyPass;

#[pymethods]
impl PyPass {
    #[new]
    fn new() -> Self {
        Self
    }
    fn __repr__(&self) -> &'static str {
        "Pass()"
    }
}

/// "Response is unacceptable; surface as ValidationFailed."
/// `ValidationFailed` is always Permanent — `RetryMiddleware` does
/// not retry on it. Callers who genuinely need a model resample on
/// validation failure should catch `TarsProviderError(kind=
/// "validation_failed")` at their own layer with explicit prompt
/// variation.
#[pyclass(frozen, get_all, name = "Reject")]
#[derive(Debug, Clone)]
pub(crate) struct PyReject {
    pub(crate) reason: String,
}

#[pymethods]
impl PyReject {
    #[new]
    fn new(reason: String) -> Self {
        Self { reason }
    }
    fn __repr__(&self) -> String {
        format!("Reject(reason={:?})", self.reason)
    }
}

/// "Replace the response text with this filtered version." Subsequent
/// validators in the chain see the filtered text.
///
/// `dropped` is a free-form audit trail (what got removed/changed) —
/// recorded in `Response.validation_summary` for downstream metrics
/// but not used for control flow.
///
/// **v1 limitation**: only `.text` is filterable. Full-Response Filter
/// (modifying `tool_calls`, `usage`, etc.) waits for a real consumer
/// — most validation patterns affect text only.
#[pyclass(frozen, get_all, name = "FilterText")]
#[derive(Debug, Clone)]
pub(crate) struct PyFilterText {
    pub(crate) text: String,
    pub(crate) dropped: Vec<String>,
}

#[pymethods]
impl PyFilterText {
    #[new]
    #[pyo3(signature = (text, *, dropped = None))]
    fn new(text: String, dropped: Option<Vec<String>>) -> Self {
        Self {
            text,
            dropped: dropped.unwrap_or_default(),
        }
    }
    fn __repr__(&self) -> String {
        format!(
            "FilterText(text={:?}, dropped_n={})",
            crate::truncate_for_repr_pub(&self.text, 40),
            self.dropped.len()
        )
    }
}

/// "Response unchanged but record per-call metrics." Lands in
/// `validation_summary.outcomes[name]` as `OutcomeSummary::Annotate`.
#[pyclass(frozen, name = "Annotate")]
#[derive(Debug)]
pub(crate) struct PyAnnotate {
    /// Stored as JSON internally so the type is `Clone` for frozen
    /// pyclass; expose as Python dict via getter.
    pub(crate) metrics_json: JsonValue,
}

#[pymethods]
impl PyAnnotate {
    #[new]
    #[pyo3(signature = (metrics = None))]
    fn new(metrics: Option<Bound<'_, PyDict>>) -> PyResult<Self> {
        let metrics_json = match metrics {
            None => JsonValue::Object(serde_json::Map::new()),
            Some(d) => {
                let py = d.py();
                let json_mod = py.import("json")?;
                let s: String = json_mod.call_method1("dumps", (d,))?.extract()?;
                serde_json::from_str(&s).unwrap_or(JsonValue::Object(serde_json::Map::new()))
            }
        };
        Ok(Self { metrics_json })
    }

    #[getter]
    fn metrics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let json_mod = py.import("json")?;
        let s = serde_json::to_string(&self.metrics_json).unwrap_or_else(|_| "{}".into());
        let obj = json_mod.call_method1("loads", (s,))?;
        obj.downcast_into::<PyDict>().map_err(PyErr::from)
    }

    fn __repr__(&self) -> String {
        let n = match &self.metrics_json {
            JsonValue::Object(m) => m.len(),
            _ => 0,
        };
        format!("Annotate(n_metrics={n})")
    }
}

// ── Adapter wrapping Python callable into `OutputValidator` ──────────

/// Adapter — wraps a Python callable into the Rust `OutputValidator`
/// trait. Callable signature: `(req: dict, resp: dict) -> Outcome`
/// where Outcome is one of the 4 pyclass instances above.
///
/// **GIL semantics**: validator is sync (matches trait contract).
/// `validate()` acquires the GIL inside the call, runs the Python
/// callback synchronously, drops GIL on return. Cost is ~1 GIL
/// acquisition + Python function call per validator per request —
/// for cheap validators (regex, set membership) this is microseconds;
/// for heavy Python ones it scales with the user's Python code.
pub(crate) struct PyValidatorAdapter {
    name: String,
    callback: Py<PyAny>,
}

impl PyValidatorAdapter {
    pub(crate) fn new(name: String, callback: Py<PyAny>) -> Self {
        Self { name, callback }
    }
}

impl OutputValidator for PyValidatorAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn validate(&self, req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome {
        // Build read-only Python views of (req, resp) and call the
        // user's validator. Returned object is one of our 4 outcome
        // pyclasses; downcast and translate to the Rust enum.
        //
        // On callback failure (panic / exception / wrong return
        // type), we fall back to Reject{retriable:false} carrying
        // the error message — caller sees a clear "validator X
        // crashed" rather than a silent-Pass false-positive.
        Python::with_gil(|py| {
            let req_dict = match build_req_dict(py, req) {
                Ok(d) => d,
                Err(e) => return reject_internal(&self.name, &format!("req serialize: {e}")),
            };
            let resp_dict = match build_resp_dict(py, resp) {
                Ok(d) => d,
                Err(e) => return reject_internal(&self.name, &format!("resp serialize: {e}")),
            };
            let cb = self.callback.bind(py);
            let outcome = match cb.call1((req_dict, resp_dict)) {
                Ok(o) => o,
                Err(e) => {
                    return reject_internal(&self.name, &format!("python callback raised: {e}"));
                }
            };
            match parse_outcome(py, outcome.unbind(), resp) {
                Ok(o) => o,
                Err(e) => reject_internal(
                    &self.name,
                    &format!("could not parse validator outcome: {e}"),
                ),
            }
        })
    }
}

fn reject_internal(validator: &str, reason: &str) -> ValidationOutcome {
    tracing::warn!(
        validator = %validator,
        reason = %reason,
        "python validator failed; surfacing as permanent Reject"
    );
    ValidationOutcome::Reject {
        reason: reason.to_string(),
    }
}

/// Build a Python dict view of `ChatRequest`. Keys: model (str),
/// system (str | None), messages (list[dict{role, text}]), tools
/// (list[dict{name, description}]). Sufficient for the common
/// validator use cases (read prompt content, count tools); richer
/// fields (tool_calls in messages, structured_output schema) added
/// on demand.
fn build_req_dict<'py>(py: Python<'py>, req: &ChatRequest) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("model", req.model.label())?;
    if let Some(s) = &req.system {
        d.set_item("system", s)?;
    } else {
        d.set_item("system", py.None())?;
    }
    // messages — flatten to {role, text}
    let messages = PyList::empty(py);
    for m in &req.messages {
        let md = PyDict::new(py);
        let role = match m {
            tars_types::Message::User { .. } => "user",
            tars_types::Message::Assistant { .. } => "assistant",
            tars_types::Message::Tool { .. } => "tool",
            tars_types::Message::System { .. } => "system",
        };
        md.set_item("role", role)?;
        let text: String = m
            .content()
            .iter()
            .filter_map(|c| c.as_text().map(String::from))
            .collect::<Vec<_>>()
            .join("");
        md.set_item("text", text)?;
        messages.append(md)?;
    }
    d.set_item("messages", messages)?;
    // tools — flatten name + description
    let tools = PyList::empty(py);
    for t in &req.tools {
        let td = PyDict::new(py);
        td.set_item("name", &t.name)?;
        td.set_item("description", &t.description)?;
        tools.append(td)?;
    }
    d.set_item("tools", tools)?;
    Ok(d)
}

/// Build a Python dict view of `ChatResponse`. Keys: text (str),
/// thinking (str), tool_calls (list[dict{id, name, arguments_json}]),
/// stop_reason (str | None).
fn build_resp_dict<'py>(py: Python<'py>, resp: &ChatResponse) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("text", &resp.text)?;
    d.set_item("thinking", &resp.thinking)?;
    let tcs = PyList::empty(py);
    for tc in &resp.tool_calls {
        let td = PyDict::new(py);
        td.set_item("id", &tc.id)?;
        td.set_item("name", &tc.name)?;
        td.set_item(
            "arguments_json",
            serde_json::to_string(&tc.arguments).unwrap_or_default(),
        )?;
        tcs.append(td)?;
    }
    d.set_item("tool_calls", tcs)?;
    match &resp.stop_reason {
        Some(r) => d.set_item("stop_reason", crate::stop_reason_str_pub(r))?,
        None => d.set_item("stop_reason", py.None())?,
    }
    Ok(d)
}

/// Parse a Python outcome (one of `Pass` / `Reject` / `FilterText` /
/// `Annotate`) into the Rust `ValidationOutcome` enum. `original_resp`
/// is needed to construct the post-Filter `ChatResponse` (we replace
/// only `.text`, copying other fields).
fn parse_outcome(
    py: Python<'_>,
    outcome: Py<PyAny>,
    original_resp: &ChatResponse,
) -> PyResult<ValidationOutcome> {
    let bound = outcome.bind(py);

    if bound.is_instance_of::<PyPass>() {
        return Ok(ValidationOutcome::Pass);
    }
    if let Ok(rej) = bound.extract::<PyRef<'_, PyReject>>() {
        return Ok(ValidationOutcome::Reject {
            reason: rej.reason.clone(),
        });
    }
    if let Ok(filt) = bound.extract::<PyRef<'_, PyFilterText>>() {
        let mut new_resp = original_resp.clone();
        new_resp.text = filt.text.clone();
        return Ok(ValidationOutcome::Filter {
            response: new_resp,
            dropped: filt.dropped.clone(),
        });
    }
    if let Ok(ann) = bound.extract::<PyRef<'_, PyAnnotate>>() {
        let metrics: HashMap<String, JsonValue> = match &ann.metrics_json {
            JsonValue::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => HashMap::new(),
        };
        return Ok(ValidationOutcome::Annotate { metrics });
    }
    // Anything else — treat as user error, fail noisily.
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "validator must return tars.Pass / Reject / FilterText / Annotate; got {}",
        bound.get_type().name()?
    )))
}

/// Public entry point used by tars-py's Pipeline constructors:
/// translate a Python `validators` kwarg (a list of `(name: str,
/// callable)` tuples) into a `Vec<Box<dyn OutputValidator>>`
/// suitable for `ValidationMiddleware::new`.
pub(crate) fn build_validator_list(
    list: Option<Bound<'_, PyList>>,
) -> PyResult<Vec<Box<dyn OutputValidator>>> {
    let Some(list) = list else {
        return Ok(Vec::new());
    };
    let mut out: Vec<Box<dyn OutputValidator>> = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        // Expect (name: str, callable). Reject anything else with a
        // pinpointed error.
        let tuple = item.downcast::<pyo3::types::PyTuple>().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err(format!(
                "validators[{i}] must be a (name, callable) tuple"
            ))
        })?;
        if tuple.len() != 2 {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "validators[{i}] must be exactly (name, callable); got {} elements",
                tuple.len()
            )));
        }
        let name: String = tuple.get_item(0)?.extract()?;
        let callback = tuple.get_item(1)?.unbind();
        out.push(Box::new(PyValidatorAdapter::new(name, callback)));
    }
    Ok(out)
}
