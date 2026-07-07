//! The FFI call-context boundary (Doc 06 §9, Doc 12 §7.2).
//!
//! `RUN_CONTEXT` is a Rust-internal `tokio::task_local` — it deliberately does
//! **not** cross the napi hop. So on the JS side the context is **explicit**: a
//! plain `{ session, tenant, trace, tags }` object is turned into a
//! [`RequestContext`] here and carried on the handle, and every call
//! re-establishes `RUN_CONTEXT.scope(ctx, …)` at the boundary (see
//! [`crate::drive_complete`]).
//!
//! This is why the binding exposes `handle.context(ctx)` (a context-bound
//! handle) rather than the doc-sketch's `withContext(ctx, body)` callback: a JS
//! async callback issues its inner `handle.pipeline(…).complete(…)` calls as
//! *separate* napi async tasks, and a `task_local` set around the callback
//! would not reach them. Binding the ctx to a derived handle makes it explicit
//! and correct per the boundary law, and reads naturally in async JS:
//!
//! ```js
//! const scoped = handle.context({ session: 'sess-abc', tags: ['dogfood'] });
//! await scoped.pipeline('critic').complete({ model, user: 'review this' });
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

use napi_derive::napi;

use tars_types::{RequestContext, SessionId, TenantId, TraceId};

/// Monotonic suffix so distinct calls that don't pass an explicit `trace` still
/// get distinct trace ids (log correlation), rather than colliding on a
/// constant.
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_trace() -> TraceId {
    let n = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
    TraceId::new(format!("node-{n}"))
}

/// The explicit per-call context passed from JS. All fields optional — an
/// empty `{}` yields a fresh single-user ("personal") context.
#[napi(object)]
pub struct JsContext {
    /// Session id — the conversation / batch partition. Default `"local"`.
    pub session: Option<String>,
    /// Tenant id — the multi-tenant partition key (Doc 06 §8). Default
    /// `"local"`.
    pub tenant: Option<String>,
    /// Trace id for log correlation. Default: an auto-incremented `node-<n>`.
    pub trace: Option<String>,
    /// Cohort tags surfaced on every emitted pipeline event.
    pub tags: Option<Vec<String>>,
}

/// A default single-user context for a handle opened without an explicit ctx.
pub(crate) fn default_context() -> RequestContext {
    RequestContext::personal(next_trace())
}

/// Build a [`RequestContext`] from the explicit JS `ctx` object.
pub(crate) fn build_context(js: JsContext) -> RequestContext {
    let trace = js.trace.map(TraceId::new).unwrap_or_else(next_trace);
    let mut ctx = RequestContext::personal(trace);
    if let Some(session) = js.session {
        ctx.session_id = SessionId::new(session);
    }
    if let Some(tenant) = js.tenant {
        ctx.tenant_id = TenantId::new(tenant);
    }
    if let Some(tags) = js.tags {
        ctx.tags = tags;
    }
    ctx
}
