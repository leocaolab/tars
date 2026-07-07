//! The FFI call-context seam (Doc 06 §9 / Doc 12 §6.2).
//!
//! A `tokio::task_local!` ([`tars_types::RUN_CONTEXT`]) does **not** survive a
//! Python `with` block — Python holds no live Rust task between the
//! `__enter__` and the eventual `handle.pipeline(...).complete(...)` call. So
//! the binding keeps the "currently active" [`RequestContext`] in a
//! **thread-local stack** established by [`ContextGuard`] on `__enter__` /
//! torn down on `__exit__`, and each network call re-establishes
//! `RUN_CONTEXT.scope(ctx, …)` from the top of the stack (see
//! `run_complete_tagged` in `lib.rs`). This is the "re-scope per call"
//! mechanism the doc mandates: the task-local is a Rust-internal detail that
//! never crosses FFI; the thread-local is only ever read synchronously on the
//! calling Python thread, then moved into the async block.
//!
//! A stack (not a single slot) so nested `with handle.context(...)` blocks
//! compose: the innermost wins, and exiting it restores the outer context.

use std::cell::RefCell;

use pyo3::prelude::*;

use tars_types::RequestContext;
use tars_types::ids::{SessionId, TenantId, TraceId};

thread_local! {
    /// Stack of active contexts for the calling thread. Empty ⇒ no active
    /// `with handle.context(...)`, so calls fall back to a default context.
    static CTX_STACK: RefCell<Vec<RequestContext>> = const { RefCell::new(Vec::new()) };
}

/// The innermost active context on this thread, if any. Cloned so the caller
/// owns it (contexts are cheap `Arc`-backed clones).
pub(crate) fn current() -> Option<RequestContext> {
    CTX_STACK.with(|s| s.borrow().last().cloned())
}

fn push(ctx: RequestContext) {
    CTX_STACK.with(|s| s.borrow_mut().push(ctx));
}

fn pop() {
    CTX_STACK.with(|s| {
        s.borrow_mut().pop();
    });
}

/// Context-manager returned by `Tars.context(...)`. Establishes a
/// [`RequestContext`] for the duration of a `with` block; every handle call
/// made inside re-scopes `RUN_CONTEXT` from it (Doc 12 §6.2).
///
/// ```python
/// with handle.context(session="sess-abc", tags=["dogfood"]):
///     handle.pipeline("critic").complete(model="…", user="review this")
/// ```
#[pyclass(name = "ContextGuard")]
pub(crate) struct ContextGuard {
    /// `None` once the guard has been entered and its context pushed onto the
    /// stack — so a double `__enter__` is a clean error, not a silent
    /// double-push that would leak a stack entry on the single matching
    /// `__exit__`.
    ctx: Option<RequestContext>,
    entered: bool,
}

impl ContextGuard {
    /// Build a guard from the Python-supplied ids. `trace` defaults to a fresh
    /// UUID (a per-`with` correlation id); `session` / `tenant` default to the
    /// `RequestContext::personal` single-user identities when omitted.
    pub(crate) fn new(
        session: Option<String>,
        tags: Option<Vec<String>>,
        tenant: Option<String>,
        trace: Option<String>,
    ) -> Self {
        // The id constructors reject the empty string (they assert non-empty),
        // so treat an empty override as "not supplied" and keep the
        // `personal` default rather than panicking on caller input.
        let trace = trace.filter(|s| !s.is_empty());
        let trace_id = TraceId::new(trace.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
        let mut ctx = RequestContext::personal(trace_id);
        if let Some(s) = session.filter(|s| !s.is_empty()) {
            ctx.session_id = SessionId::new(s);
        }
        if let Some(t) = tenant.filter(|s| !s.is_empty()) {
            ctx.tenant_id = TenantId::new(t);
        }
        ctx.tags = tags.unwrap_or_default();
        Self {
            ctx: Some(ctx),
            entered: false,
        }
    }
}

#[pymethods]
impl ContextGuard {
    fn __enter__(mut slf: PyRefMut<'_, Self>) -> PyResult<PyRefMut<'_, Self>> {
        if slf.entered {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "context guard already entered; use one `with` block per guard",
            ));
        }
        // `take()` moves the context onto the stack — the guard no longer
        // holds it, so re-entry is refused above.
        let ctx = slf.ctx.take().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("context guard has no context to enter")
        })?;
        push(ctx);
        slf.entered = true;
        Ok(slf)
    }

    /// Pop this guard's context. Signature matches the Python
    /// context-manager protocol; the exception triple is ignored (we always
    /// restore the previous context, propagating any exception unchanged by
    /// returning `False`).
    #[pyo3(signature = (_exc_type = None, _exc_value = None, _traceback = None))]
    fn __exit__(
        &mut self,
        _exc_type: Option<PyObject>,
        _exc_value: Option<PyObject>,
        _traceback: Option<PyObject>,
    ) -> bool {
        if self.entered {
            pop();
            self.entered = false;
        }
        false
    }

    fn __repr__(&self) -> String {
        match &self.ctx {
            Some(c) => format!(
                "ContextGuard(session={:?}, tenant={:?}, trace={:?}, tags={:?}, active=false)",
                c.session_id.as_str(),
                c.tenant_id.as_str(),
                c.trace_id.as_str(),
                c.tags,
            ),
            None => "ContextGuard(active=true)".to_string(),
        }
    }
}
