//! Turns the JS `{ tenant, session, trace, tags }` object into the [`Caller`] a handle
//! carries.
//!
//! `RUN_CONTEXT` is a tokio task-local and does not cross the napi hop, so each handle is built
//! with its ctx and every call sets `RUN_CONTEXT` again ([`crate::drive_complete`]). A
//! `withContext(ctx, body)` callback would not work: napi runs each `complete()` inside it as a
//! separate async task, which a task-local set around the callback does not reach.
//!
//! A handle has no tenant of its own, so a ctx without one does not build:
//!
//! ```js
//! const p = pipeline('critic', { tenant: 'acme', session: '01900000-0000-7000-8000-000000000abc', tags: ['dogfood'] });
//! await p.complete({ model, user: 'review this' });
//! ```

use napi_derive::napi;

use serde::de::DeserializeOwned;
use tars_types::{Caller, TraceId};

use crate::errors::{CtxField, JsError, invalid_ctx_id_to_js, no_tenant_to_js};

/// Returns a fresh trace id. It is a random UUID because call-log rows from every process
/// share one log, where a per-process counter would repeat.
fn new_trace() -> TraceId {
    TraceId::from(uuid::Uuid::new_v4())
}

/// The explicit per-call context passed from JS. `tenant` is required — whose
/// calls these are; the rest default.
#[napi(object)]
pub struct JsContext {
    /// Tenant id — the multi-tenant partition key every call is logged under.
    pub tenant: String,
    /// The id a tars session minted for its conversation (a UUIDv7). Absent:
    /// the calls belong to no session, and are logged with none.
    pub session: Option<String>,
    /// Trace id for log correlation. Default: a random UUID.
    pub trace: Option<String>,
    /// Cohort tags surfaced on every emitted pipeline event.
    pub tags: Option<Vec<String>>,
}

/// Build the caller's [`Caller`] from the JS `ctx` argument. The
/// declared type makes `ctx` required, but plain JS can still leave it off —
/// that, and an empty tenant / session / trace, are refused by name rather
/// than turned into an id nobody gave (or a panic across the FFI hop).
pub(crate) fn build_context(js: Option<JsContext>) -> Result<Caller, JsError> {
    let js = js.ok_or_else(no_tenant_to_js)?;
    let tenant = parse_id(CtxField::Tenant, js.tenant)?;
    let trace = parse_optional_id(CtxField::Trace, js.trace)?.unwrap_or_else(new_trace);
    let mut ctx = Caller::new(tenant, trace);
    ctx.session_id = parse_optional_id(CtxField::Session, js.session)?;
    if let Some(tags) = js.tags {
        ctx.tags = tags;
    }
    Ok(ctx)
}

/// An id type's own deserializer is the one place that knows what a valid id
/// is (non-empty); its `new` would panic instead.
fn parse_id<T: DeserializeOwned>(field: CtxField, raw: String) -> Result<T, JsError> {
    serde_json::from_value(serde_json::Value::String(raw))
        .map_err(|err| invalid_ctx_id_to_js(field, err))
}

/// An absent field stays absent; a given one must be a valid id.
fn parse_optional_id<T: DeserializeOwned>(
    field: CtxField,
    raw: Option<String>,
) -> Result<Option<T>, JsError> {
    raw.map(|raw| parse_id(field, raw)).transpose()
}

#[cfg(test)]
mod tests {
    use tars_types::{SessionId, TenantId};

    use super::*;

    const SESSION: &str = "01900000-0000-7000-8000-000000000abc";

    fn js(tenant: &str) -> JsContext {
        JsContext {
            tenant: tenant.to_string(),
            session: None,
            trace: None,
            tags: None,
        }
    }

    #[test]
    fn a_context_carries_the_tenant_the_caller_names() {
        let ctx = build_context(Some(js("acme"))).unwrap();
        assert_eq!(ctx.tenant_id, TenantId::new("acme"));
    }

    /// An empty tenant is refused by name, not turned into a default tenant such as `"local"`.
    #[test]
    fn an_empty_tenant_is_refused_naming_it() {
        let Err(err) = build_context(Some(js(""))) else {
            panic!("an empty tenant built a context");
        };
        assert_eq!(err.status, "TarsInvalidTenant");
        assert!(err.reason.contains("ctx.tenant"), "{}", err.reason);
    }

    #[test]
    fn a_context_without_a_session_has_none() {
        let ctx = build_context(Some(js("acme"))).unwrap();
        assert_eq!(ctx.session_id, None);
    }

    #[test]
    fn a_context_carries_the_session_the_caller_names() {
        let named = JsContext {
            session: Some(SESSION.to_string()),
            ..js("acme")
        };
        let ctx = build_context(Some(named)).unwrap();
        assert_eq!(ctx.session_id, Some(SESSION.parse::<SessionId>().unwrap()));
    }

    #[test]
    fn an_empty_session_is_refused_naming_it() {
        let empty = JsContext {
            session: Some(String::new()),
            ..js("acme")
        };
        let Err(err) = build_context(Some(empty)) else {
            panic!("an empty session built a context");
        };
        assert_eq!(err.status, "TarsInvalidSession");
        assert!(err.reason.contains("ctx.session"), "{}", err.reason);
    }

    /// An empty trace is refused, not replaced by a generated one.
    #[test]
    fn an_empty_trace_is_refused_naming_it() {
        let empty = JsContext {
            trace: Some(String::new()),
            ..js("acme")
        };
        let Err(err) = build_context(Some(empty)) else {
            panic!("an empty trace built a context");
        };
        assert_eq!(err.status, "TarsInvalidTrace");
        assert!(err.reason.contains("ctx.trace"), "{}", err.reason);
    }

    #[test]
    fn a_context_without_a_trace_gets_a_uuid_trace() {
        let ctx = build_context(Some(js("acme"))).unwrap();
        assert!(
            uuid::Uuid::parse_str(ctx.trace_id.as_str()).is_ok(),
            "{:?}",
            ctx.trace_id
        );
    }

    #[test]
    fn a_context_carries_the_trace_the_caller_names() {
        let named = JsContext {
            trace: Some("trace-1".to_string()),
            ..js("acme")
        };
        let ctx = build_context(Some(named)).unwrap();
        assert_eq!(ctx.trace_id, TraceId::new("trace-1"));
    }

    #[test]
    fn a_missing_context_is_refused_as_no_tenant() {
        let Err(err) = build_context(None) else {
            panic!("no ctx built a context");
        };
        assert_eq!(err.status, "TarsNoTenant");
        assert!(err.reason.contains("{ tenant"), "{}", err.reason);
    }
}
