//! PyO3 wrapper for `tars_runtime::Session`.
//!
//! Exposed Python surface (after `from tars import Session`):
//!
//! ```python
//! sess = tars.Session(pipeline, system="...", budget_chars=400_000,
//!                     default_max_output_tokens=16_384, model="qwen/...")
//! resp = sess.send("hello")                     # returns Response
//! txt  = sess.send_text("hello")                # convenience -> str
//! resp = sess.send_tool_results([(id, result)]) # parallel result batch
//! sess.register_tool(name, schema, callback)    # python callable
//! sess.reset()
//! sess2 = sess.fork()
//! ```
//!
//! Tool callbacks are arbitrary Python callables: `(arguments_dict)
//! -> result_json`. Session bridges them to the Rust `Tool` trait via
//! `PyTool` below — it acquires the GIL on each call so the callback
//! runs in the Python interpreter's thread, then drops back into
//! tokio land to await the next model call.

use std::sync::Arc;

use futures::future::FutureExt;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use serde_json::Value as JsonValue;

use tars_runtime::{
    Budget as RsBudget, Session as RsSession, SessionError, SessionOptions, Tool, ToolRegistry,
};
use tars_types::{JsonSchema, ModelHint, ToolSpec};

use crate::errors::{provider_to_py, runtime_to_py};
use crate::{Pipeline as PyPipeline, Response, Usage, TOKIO};

/// `Session` wraps `tars_runtime::Session`. Mutable internally — every
/// method takes `&mut self`. PyO3's per-instance lock serializes
/// concurrent calls from Python threads (we deliberately do NOT mark
/// the class `frozen` for this reason).
#[pyclass]
pub struct Session {
    inner: RsSession,
}

#[pymethods]
impl Session {
    /// Construct a Session backed by the given `Pipeline`.
    ///
    /// - `system`: system prompt; never enters history, attached every call.
    /// - `model`: model id the conversation targets.
    /// - `budget_chars`: history cost limit in chars (default 400_000).
    ///   Pass 0 to disable trimming entirely (use only with short
    ///   conversations or test fixtures).
    /// - `default_max_output_tokens`: applied when `send()` is called
    ///   without an explicit cap. `None` defers to provider default.
    #[new]
    #[pyo3(signature = (
        pipeline,
        *,
        system,
        model,
        budget_chars = 400_000,
        default_max_output_tokens = None,
    ))]
    fn new(
        pipeline: &PyPipeline,
        system: String,
        model: String,
        budget_chars: usize,
        default_max_output_tokens: Option<u32>,
    ) -> PyResult<Self> {
        let svc = pipeline.inner_arc();
        let caps = pipeline.capabilities_owned();
        let budget = if budget_chars == 0 {
            // Encode "disabled" as a very-large char limit instead of
            // a separate variant — keeps the runtime path one-shape
            // and lets test fixtures opt out of trimming.
            RsBudget::Chars(usize::MAX / 2)
        } else {
            RsBudget::Chars(budget_chars)
        };
        let opts = SessionOptions {
            system,
            budget,
            tools: Some(ToolRegistry::new()),
            default_max_output_tokens,
            model: ModelHint::Explicit(model),
        };
        Ok(Self {
            inner: RsSession::new(svc, caps, opts),
        })
    }

    /// Stable session id. Useful for log correlation.
    #[getter]
    fn id(&self) -> &str {
        self.inner.id()
    }

    /// Frozen snapshot of all messages across all turns.
    fn history<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let msgs = self.inner.history();
        let out = PyList::empty(py);
        for m in msgs {
            let d = PyDict::new(py);
            let role = match &m {
                tars_types::Message::User { .. } => "user",
                tars_types::Message::Assistant { .. } => "assistant",
                tars_types::Message::Tool { .. } => "tool",
                tars_types::Message::System { .. } => "system",
            };
            d.set_item("role", role)?;
            // Flatten content text for the simple Python view; tool
            // calls / multimodal stay as JSON.
            let text: String = m
                .content()
                .iter()
                .filter_map(|c| c.as_text().map(|s| s.to_string()))
                .collect::<Vec<_>>()
                .join("");
            d.set_item("content", text)?;
            if let tars_types::Message::Assistant { tool_calls, .. } = &m {
                if !tool_calls.is_empty() {
                    let tcs = PyList::empty(py);
                    for tc in tool_calls {
                        let td = PyDict::new(py);
                        td.set_item("id", &tc.id)?;
                        td.set_item("name", &tc.name)?;
                        td.set_item(
                            "arguments",
                            serde_json::to_string(&tc.arguments).unwrap_or_default(),
                        )?;
                        tcs.append(td)?;
                    }
                    d.set_item("tool_calls", tcs)?;
                }
            }
            out.append(d)?;
        }
        Ok(out)
    }

    /// Number of turns (logical exchanges) accumulated so far.
    #[getter]
    fn turn_count(&self) -> usize {
        self.inner.turns().len()
    }

    /// Monotone version stamp bumped on each visible history mutation
    /// (successful send, reset). NOT bumped on rollback. NOT bumped
    /// during the in-flight tool loop. Use as a cheap cache-key
    /// invalidator: a (session.id, session.history_version) pair
    /// uniquely identifies "which history snapshot produced this
    /// downstream artifact". `fork()` preserves the value so the
    /// cache can detect shared prefixes.
    #[getter]
    fn history_version(&self) -> u64 {
        self.inner.history_version()
    }

    /// Drop conversation state but keep system / budget / tools / model.
    fn reset(&mut self) {
        self.inner.reset();
    }

    /// Cheap-clone of conversation state. The returned Session shares
    /// the underlying Pipeline + tools but has independent history.
    fn fork(&self) -> Self {
        Self { inner: self.inner.fork() }
    }

    /// Register a Python callable as a tool the model can call.
    ///
    /// - `name`: tool name as the model sees it.
    /// - `description`: short doc for the model's tool selection.
    /// - `parameters_schema`: JSON-Schema-shaped dict describing the
    ///   `arguments` object the model emits.
    /// - `callback`: `Callable[[dict], dict | str]` — invoked with the
    ///   model-supplied arguments. Return value is JSON-serialized
    ///   into the next turn's tool_result.
    #[pyo3(signature = (name, description, parameters_schema, callback))]
    fn register_tool(
        &mut self,
        py: Python<'_>,
        name: String,
        description: String,
        parameters_schema: Bound<'_, PyDict>,
        callback: Py<PyAny>,
    ) -> PyResult<()> {
        // Convert the schema dict to serde_json::Value via JSON round
        // trip; pyo3 doesn't have a direct `PyDict -> serde_json::Value`
        // path that handles every nested case.
        let schema_str: String = py
            .import("json")?
            .call_method1("dumps", (parameters_schema,))?
            .extract()?;
        let schema: JsonValue = serde_json::from_str(&schema_str)
            .map_err(|e| runtime_to_py("parsing tool schema", e))?;

        let spec = ToolSpec {
            name: name.clone(),
            description,
            input_schema: JsonSchema::loose(schema),
        };
        let tool = Arc::new(PyTool {
            name,
            spec,
            callback,
        }) as Arc<dyn Tool>;

        // The Session holds a ToolRegistry inside its internal struct;
        // we need a setter to push tools post-construction. Use the
        // session helper added in tars_runtime::Session::register_tool.
        self.inner.register_tool(tool);
        Ok(())
    }

    /// Send one user message and return the final assistant reply.
    /// If tools are registered, runs the auto-loop transparently.
    #[pyo3(signature = (user, *, max_output_tokens = None))]
    fn send(
        &mut self,
        py: Python<'_>,
        user: &str,
        max_output_tokens: Option<u32>,
    ) -> PyResult<Response> {
        // Move the &mut self.inner into a future. We can't capture it
        // across the GIL-release boundary directly because PyO3 owns
        // self; instead, we run the future on the tokio runtime under
        // `allow_threads`, with a tiny block_in_place to satisfy the
        // borrow checker — `inner` is only touched on this thread.
        let result = py.allow_threads(|| {
            TOKIO.block_on(self.inner.send(user, max_output_tokens))
        });
        let (resp, telemetry) = result.map_err(session_err_to_py)?;
        Ok(chat_response_to_py(resp, telemetry))
    }

    /// Like `send` but returns the final assistant text directly.
    #[pyo3(signature = (user, *, max_output_tokens = None))]
    fn send_text(
        &mut self,
        py: Python<'_>,
        user: &str,
        max_output_tokens: Option<u32>,
    ) -> PyResult<String> {
        let result: Result<String, SessionError> = py.allow_threads(|| {
            TOKIO.block_on(self.inner.send_text(user, max_output_tokens))
        });
        result.map_err(session_err_to_py)
    }

    fn __repr__(&self) -> String {
        format!(
            "Session(id={:?}, turns={})",
            self.inner.id(),
            self.inner.turns().len()
        )
    }
}

/// Bridges a Python callable into Rust `Tool`. The callback runs under
/// the GIL on each call; the `BoxFuture` returned by `Tool::call`
/// resolves immediately because Python tools are synchronous from the
/// LLM-runtime's perspective.
struct PyTool {
    name: String,
    spec: ToolSpec,
    callback: Py<PyAny>,
}

impl Tool for PyTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn call(
        &self,
        arguments: JsonValue,
    ) -> futures::future::BoxFuture<'_, Result<JsonValue, SessionError>> {
        let name = self.name.clone();
        // Async block borrows `&self` for the future's lifetime. The
        // body does no real `await` — the GIL block is synchronous —
        // but we wrap as a future so it satisfies the `Tool::call`
        // signature. Avoid `assume_gil_acquired` (forbidden by
        // workspace lints): bind the callback inside `with_gil` instead
        // of cloning the handle out.
        async move {
            Python::with_gil(|py| {
                let json_mod = py
                    .import("json")
                    .map_err(|e| SessionError::Internal(e.to_string()))?;
                let arg_str = serde_json::to_string(&arguments)
                    .map_err(|e| SessionError::Internal(e.to_string()))?;
                let arg_obj = json_mod.call_method1("loads", (arg_str,)).map_err(|e| {
                    SessionError::ToolFailed {
                        name: name.clone(),
                        message: format!("arg deserialize: {e}"),
                    }
                })?;
                let cb_bound = self.callback.bind(py);
                let result = cb_bound.call1((arg_obj,)).map_err(|e| {
                    SessionError::ToolFailed {
                        name: name.clone(),
                        message: e.to_string(),
                    }
                })?;
                let dumped: String = json_mod
                    .call_method1("dumps", (result,))
                    .and_then(|s| s.extract())
                    .map_err(|e| SessionError::ToolFailed {
                        name: name.clone(),
                        message: format!("result serialize: {e}"),
                    })?;
                serde_json::from_str(&dumped).map_err(|e| SessionError::Internal(e.to_string()))
            })
        }
        .boxed()
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn chat_response_to_py(
    resp: tars_types::ChatResponse,
    telemetry_acc: tars_types::TelemetryAccumulator,
) -> Response {
    Response {
        text: resp.text,
        thinking: resp.thinking,
        usage: Usage {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            cached_input_tokens: resp.usage.cached_input_tokens,
            cache_creation_tokens: resp.usage.cache_creation_tokens,
            thinking_tokens: resp.usage.thinking_tokens,
        },
        stop_reason: resp
            .stop_reason
            .map(|r| stop_reason_str(&r).to_string())
            .unwrap_or_else(|| "none".to_string()),
        telemetry: crate::Telemetry {
            cache_hit: telemetry_acc.cache_hit,
            retry_count: telemetry_acc.retry_count,
            retry_attempts: telemetry_acc
                .retry_attempts
                .into_iter()
                .map(|r| crate::RetryAttemptPy {
                    kind: r.error_kind,
                    retry_after_ms: r.retry_after_ms,
                })
                .collect(),
            provider_latency_ms: telemetry_acc.provider_latency_ms,
            pipeline_total_ms: telemetry_acc.pipeline_total_ms,
            layers: telemetry_acc.layers,
        },
    }
}

fn stop_reason_str(r: &tars_types::StopReason) -> &'static str {
    use tars_types::StopReason::*;
    match r {
        EndTurn => "end_turn",
        MaxTokens => "max_tokens",
        StopSequence => "stop_sequence",
        ToolUse => "tool_use",
        ContentFilter => "content_filter",
        Cancelled => "cancelled",
        Other => "other",
    }
}

fn session_err_to_py(e: SessionError) -> PyErr {
    match e {
        SessionError::Provider(p) => provider_to_py(p),
        SessionError::ToolFailed { name, message } => crate::errors::TarsRuntimeError::new_err(
            format!("tool {name:?} failed: {message}"),
        ),
        SessionError::Internal(m) => crate::errors::TarsRuntimeError::new_err(m),
    }
}

// Suppress unused warning for PyTuple — kept around for future
// `send_tool_results` manual mode.
#[allow(dead_code)]
fn _kept_for_send_tool_results(_t: &PyTuple) {}
