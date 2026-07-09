//! The typed, result-side view of the always-on durable store.
//!
//! The persistence + SQL live in [`tars_storage::SqliteDurableStore`],
//! which deals in an **opaque JSON payload** ([`RawAnswer`]). This module
//! owns the TYPED domain: [`StepAnswer`] (message as a concrete
//! [`AgentMessage`]), the [`Plan`] a job carries, and the serialize /
//! decode at the boundary — the "result-side JSON decode." A decode
//! failure surfaces as a typed [`DurableError::Serde`], never buried in a
//! rusqlite conversion error.
//!
//! [`AnswerStore`] is the typed adapter the driver + tests hold; it wraps
//! the payload-agnostic [`tars_storage::DurableStore`] trait and never
//! touches sqlite/rusqlite itself.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tars_storage::{
    DurableStore, RawAnswer, ResultEventKind, ResultEventRecord, STATUS_COMPLETED, STATUS_SKIPPED,
    SqliteDurableStore,
};
use tars_types::Usage;

use crate::durable::error::DurableError;
use crate::message::AgentMessage;
use crate::orchestrator::Plan;

/// One step's checkpointed result — the AnswerStore value. Its own type
/// (NOT the cache's `CachedResponse`, which is welded to `ChatResponse`):
/// a durable step result is an [`AgentMessage::PartialResult`] + usage.
#[derive(Clone, Debug)]
pub struct StepAnswer {
    pub job_id: String,
    pub step_id: String,
    /// The worker's output — always [`AgentMessage::PartialResult`] for a
    /// completed step; a synthetic skip marker for a skipped step.
    pub message: AgentMessage,
    /// Token usage the worker reported (zeros for non-LLM / skipped).
    pub usage: Usage,
    /// `ChatResponse::created` (unix seconds) the worker carried up; `0`
    /// for non-LLM / skipped steps.
    pub created: i64,
    /// Projected from the timeline by the blackboard (`completed` /
    /// `skipped`).
    pub status: String,
}

impl StepAnswer {
    /// A completed step: the worker's real output.
    pub fn completed(
        job_id: &str,
        step_id: &str,
        message: AgentMessage,
        usage: Usage,
        created: i64,
    ) -> Self {
        Self {
            job_id: job_id.to_string(),
            step_id: step_id.to_string(),
            message,
            usage,
            created,
            status: STATUS_COMPLETED.to_string(),
        }
    }

    /// A skipped step: presence in the store so it is never re-run and its
    /// dependents cascade, carrying the human-readable reason.
    pub fn skipped(job_id: &str, step_id: &str, reason: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            step_id: step_id.to_string(),
            message: AgentMessage::PartialResult {
                from_agent: tars_types::AgentId::new("durable-skip"),
                step_id: Some(step_id.to_string()),
                summary: format!("(skipped: {reason})"),
                confidence: 0.0,
            },
            usage: Usage::default(),
            created: 0,
            status: STATUS_SKIPPED.to_string(),
        }
    }

    pub fn is_skipped(&self) -> bool {
        self.status == STATUS_SKIPPED
    }

    /// Serialize into the storage layer's opaque payload (message + usage
    /// → their JSON columns).
    fn to_raw(&self) -> Result<RawAnswer, DurableError> {
        Ok(RawAnswer {
            job_id: self.job_id.clone(),
            step_id: self.step_id.clone(),
            message_json: serde_json::to_string(&self.message)?,
            usage_json: serde_json::to_string(&self.usage)?,
            created: self.created,
            status: self.status.clone(),
        })
    }

    /// Decode a stored opaque payload back into the typed answer — the
    /// result-side decode. A malformed column surfaces as
    /// [`DurableError::Serde`] carrying the typed serde error.
    fn from_raw(raw: RawAnswer) -> Result<Self, DurableError> {
        Ok(Self {
            job_id: raw.job_id,
            step_id: raw.step_id,
            message: serde_json::from_str(&raw.message_json)?,
            usage: serde_json::from_str(&raw.usage_json)?,
            created: raw.created,
            status: raw.status,
        })
    }
}

/// The typed always-on checkpoint store: the persistent, no-TTL,
/// step-identity-keyed result store that IS the checkpoint (§14). Wraps the
/// payload-agnostic [`DurableStore`] contract and owns the typed
/// serialize/decode of [`StepAnswer`] and [`Plan`]. Cheap to clone.
#[derive(Clone)]
pub struct AnswerStore {
    store: Arc<dyn DurableStore>,
}

impl AnswerStore {
    /// Open (creating if needed) the durable store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DurableError> {
        Ok(Self { store: Arc::new(SqliteDurableStore::open(path)?) })
    }

    /// Private in-memory store for tests / ephemeral use.
    pub fn in_memory() -> Result<Self, DurableError> {
        Ok(Self { store: Arc::new(SqliteDurableStore::in_memory()?) })
    }

    /// Wrap any [`DurableStore`] backend (e.g. a shared handle).
    pub fn from_store(store: Arc<dyn DurableStore>) -> Self {
        Self { store }
    }

    /// Persist a fresh durable job (the status of record). Idempotent:
    /// re-submitting the same id leaves the existing row untouched.
    pub fn create_job(&self, job_id: &str, plan: &Plan) -> Result<(), DurableError> {
        let plan_json = serde_json::to_string(plan)?;
        self.store.create_job(job_id, &plan_json)?;
        Ok(())
    }

    /// Atomically checkpoint one step: `{answer + result event + job
    /// updated_at}` in ONE transaction.
    pub fn commit_step(
        &self,
        answer: &StepAnswer,
        kind: ResultEventKind,
        reason: Option<&str>,
    ) -> Result<(), DurableError> {
        let raw = answer.to_raw()?;
        self.store.commit_step(&raw, kind, reason)?;
        Ok(())
    }

    /// The AnswerStore, scoped to one job: `step_id → StepAnswer` for every
    /// present (completed or skipped) step. The scheduler's readiness/skip
    /// test derives entirely from this — the frontier is DERIVED, never
    /// stored.
    pub fn answers(&self, job_id: &str) -> Result<HashMap<String, StepAnswer>, DurableError> {
        let mut out = HashMap::new();
        for (step_id, raw) in self.store.answers(job_id)? {
            out.insert(step_id, StepAnswer::from_raw(raw)?);
        }
        Ok(out)
    }

    /// One step's checkpoint, if present.
    pub fn answer(&self, job_id: &str, step_id: &str) -> Result<Option<StepAnswer>, DurableError> {
        self.store.answer(job_id, step_id)?.map(StepAnswer::from_raw).transpose()
    }

    /// Read a job's `result_events`, `seq > since`, in order.
    pub fn result_events_since(
        &self,
        job_id: &str,
        since: u64,
    ) -> Result<Vec<ResultEventRecord>, DurableError> {
        Ok(self.store.result_events_since(job_id, since)?)
    }

    /// All of a job's result events (`read_since(0)`).
    pub fn result_events(&self, job_id: &str) -> Result<Vec<ResultEventRecord>, DurableError> {
        Ok(self.store.result_events(job_id)?)
    }

    /// The persisted plan for a job (the status of record — how resume
    /// re-drives without the caller re-supplying it).
    pub fn load_plan(&self, job_id: &str) -> Result<Plan, DurableError> {
        let plan_json = self
            .store
            .load_plan_json(job_id)?
            .ok_or_else(|| DurableError::JobNotFound(job_id.to_string()))?;
        Ok(serde_json::from_str(&plan_json)?)
    }

    /// A job's current lifecycle status, if the row exists.
    pub fn job_status(&self, job_id: &str) -> Result<Option<String>, DurableError> {
        Ok(self.store.job_status(job_id)?)
    }

    /// Set a job's lifecycle status (e.g. mark terminal when every step
    /// resolved).
    pub fn set_job_status(&self, job_id: &str, status: &str) -> Result<(), DurableError> {
        Ok(self.store.set_job_status(job_id, status)?)
    }
}
