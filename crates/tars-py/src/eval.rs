//! `tars.eval` — minimal evaluation helpers over the pipeline event
//! store (Doc 16, re-scoped M9 plan).
//!
//! The full Doc 16 vision (Evaluator traits, Online/Offline runners,
//! samplers) was re-scoped to a thin surface: callers write evaluator
//! scripts (cron / CI / notebook) that **read** finished calls and
//! **write** scores back as `EvaluationScored` events. Two functions
//! cover that loop:
//!
//! - [`read_calls`] — pull `LlmCallFinished` events as plain dicts so a
//!   script can compute a metric per call.
//! - [`write_score`] — append an `EvaluationScored` event (FK'd to the
//!   call) so the score lands in the same store, queryable alongside
//!   the calls it grades.
//!
//! Both go through the Rust `SqlitePipelineEventLog` so the on-disk
//! schema can't drift from a hand-rolled SQL writer.

use std::time::{Duration, SystemTime};

use pyo3::prelude::*;
use pyo3::types::PyList;
use uuid::Uuid;

use tars_melt::event::{
    PipelineEventLog, PipelineEventQuery, SqlitePipelineEventLog,
    SqlitePipelineEventLogConfig,
};
use tars_types::{EvaluationScored, PipelineEvent, TenantId};

use crate::errors::runtime_to_py;
use crate::tokio_runtime;

/// Open the pipeline event store under `dir` (expects
/// `{dir}/pipeline_events.db`). Shared by both helpers.
fn open_store(dir: &str) -> PyResult<std::sync::Arc<dyn PipelineEventLog>> {
    let path = std::path::Path::new(dir).join("pipeline_events.db");
    if !path.exists() {
        return Err(pyo3::exceptions::PyFileNotFoundError::new_err(format!(
            "pipeline_events.db not found under {dir:?}; run a \
             `Pipeline.complete(..., event_store_dir=...)` (or builder \
             `.event_store(dir)`) first to populate it"
        )));
    }
    let store = SqlitePipelineEventLog::open(SqlitePipelineEventLogConfig::new(path))
        .map_err(|e| runtime_to_py("open pipeline event store", e))?;
    Ok(store as std::sync::Arc<dyn PipelineEventLog>)
}

/// Read finished LLM calls from the event store as plain dicts (one per
/// `LlmCallFinished`), newest-first per the store's ordering.
///
/// Each dict is the event's JSON form — `event_id`, `tenant_id`,
/// `actual_model`, `usage`, `telemetry`, `validation_summary`,
/// `validation_reason` (if a validator rejected), `result`, `tags`,
/// plus `request_ref` / `response_ref` CAS pointers. Metric scripts
/// score off these fields; resolving the full request/response *bodies*
/// from the refs is out of scope for this helper (use the CAS / a
/// future body helper).
///
/// Filters: `since_secs` (lookback window; `None` = no lower bound),
/// `tenant`, `tag` (only calls carrying the tag), `limit`.
#[pyfunction]
#[pyo3(name = "eval_read_calls")]
#[pyo3(signature = (event_store_dir, *, since_secs = None, tenant = None, tag = None, limit = None))]
pub(crate) fn read_calls(
    py: Python<'_>,
    event_store_dir: &str,
    since_secs: Option<u64>,
    tenant: Option<String>,
    tag: Option<String>,
    limit: Option<u32>,
) -> PyResult<Py<PyList>> {
    let store = open_store(event_store_dir)?;
    let since = since_secs
        .map(|s| SystemTime::now().checked_sub(Duration::from_secs(s)))
        .map(|opt| opt.unwrap_or(SystemTime::UNIX_EPOCH));
    let q = PipelineEventQuery {
        tenant_id: tenant.map(TenantId::new),
        since,
        until: None,
        // A tag filter runs in-process, so don't push a row cap that
        // would truncate before filtering (mirrors `tars events list`).
        limit: if tag.is_some() { None } else { limit },
    };

    let events = tokio_runtime()?
        .block_on(async { store.query(&q).await })
        .map_err(|e| runtime_to_py("query event store", e))?;

    let json_mod = py.import("json")?;
    let out = PyList::empty(py);
    let mut kept = 0u32;
    for ev in &events {
        let PipelineEvent::LlmCallFinished(call) = ev else {
            continue;
        };
        if let Some(t) = &tag {
            if !call.tags.iter().any(|x| x == t) {
                continue;
            }
        }
        // Round-trip through JSON so Python gets a native dict without a
        // hand-written field-by-field marshal that could drift from the
        // event schema.
        let s = serde_json::to_string(ev).map_err(|e| runtime_to_py("serialize event", e))?;
        let obj = json_mod.call_method1("loads", (s,))?;
        out.append(obj)?;
        kept += 1;
        if let Some(lim) = limit {
            if kept >= lim {
                break;
            }
        }
    }
    Ok(out.unbind())
}

/// Append an `EvaluationScored` event grading the call identified by
/// `call_event_id`. Returns the new score event's id (UUID string).
///
/// `tenant_id` is optional: when omitted it's looked up from the
/// referenced call (a scan — pass it explicitly, e.g. from the dict
/// `read_calls` returned, to skip the lookup). `score` is free-form
/// (callers pick the scale — 0..1 rate, raw count, etc.).
#[pyfunction]
#[pyo3(name = "eval_write_score")]
#[pyo3(signature = (
    event_store_dir,
    call_event_id,
    evaluator_name,
    score,
    *,
    tenant_id = None,
    explanation = None,
    tags = None,
))]
pub(crate) fn write_score(
    event_store_dir: &str,
    call_event_id: &str,
    evaluator_name: String,
    score: f64,
    tenant_id: Option<String>,
    explanation: Option<String>,
    tags: Option<Vec<String>>,
) -> PyResult<String> {
    let call_uuid = Uuid::parse_str(call_event_id).map_err(|e| {
        pyo3::exceptions::PyValueError::new_err(format!(
            "call_event_id {call_event_id:?} is not a valid UUID: {e}"
        ))
    })?;
    let store = open_store(event_store_dir)?;

    // Resolve the tenant: explicit wins; otherwise find the referenced
    // call and borrow its tenant. Fail loudly if the call doesn't exist
    // — a score FK'd to a missing call is a silent data error.
    let tenant = match tenant_id {
        Some(t) => TenantId::new(t),
        None => {
            let all = tokio_runtime()?
                .block_on(async {
                    store
                        .query(&PipelineEventQuery {
                            limit: Some(10_000),
                            ..Default::default()
                        })
                        .await
                })
                .map_err(|e| runtime_to_py("query event store", e))?;
            let found = all.iter().find_map(|ev| match ev {
                PipelineEvent::LlmCallFinished(c) if c.event_id == call_uuid => {
                    Some(c.tenant_id.clone())
                }
                _ => None,
            });
            found.ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "call_event_id {call_event_id:?} not found in the most recent \
                     10000 events; pass tenant_id= explicitly if the call is older"
                ))
            })?
        }
    };

    let event_id = Uuid::new_v4();
    let scored = EvaluationScored {
        event_id,
        timestamp: SystemTime::now(),
        tenant_id: tenant,
        call_event_id: call_uuid,
        evaluator_name,
        score,
        explanation,
        tags: tags.unwrap_or_default(),
    };
    let ev = PipelineEvent::EvaluationScored(Box::new(scored));
    tokio_runtime()?
        .block_on(async { store.append(std::slice::from_ref(&ev)).await })
        .map_err(|e| runtime_to_py("append score event", e))?;

    Ok(event_id.to_string())
}
