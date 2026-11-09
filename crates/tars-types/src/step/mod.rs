//! Defines [`Step`], the one trait every executor implements: `outcome = step(goal, ctx)`.
//! A function, an LLM call, an agent loop, a DAG or a fiber is each one opaque step; the
//! runtime drives steps, not what happens inside them.
use async_trait::async_trait;

use crate::StepContext;
use crate::goal::Goal;
use crate::outcome::{Outcome, Reason};
use crate::requires::Requires;

pub mod cap;
pub mod context;
pub mod fn_step;
pub mod goal;
pub mod intent;
pub mod outcome;
pub mod permission;
pub mod requires;
pub mod schedule;
pub mod typed_cap;

/// One opaque unit of work; a multi-turn tool loop lives inside `exec`.
///
/// `&dyn Goal` rather than a generic and `#[async_trait]` both keep `Arc<dyn Step>`
/// object-safe, because the registry stores it.
///
/// A step keeps no state of its own: it changes the world through its caps, and the
/// runtime builds a fresh `StepContext` for every step.
#[async_trait]
pub trait Step: Send + Sync {
    /// Returns the caps this step needs; the default needs none.
    ///
    /// The runtime checks it before `exec`, so a missing cap fails at claim time
    /// instead of after paid LLM turns ([`StepContext::provide`](crate::StepContext::provide)).
    fn requires(&self) -> Requires {
        Requires::none()
    }

    async fn exec(&self, goal: &dyn Goal, ctx: &StepContext) -> Outcome;

    /// Releases what a terminally ended step still holds and returns what it could not
    /// release; the default holds nothing.
    ///
    /// # Errors
    ///
    /// The reason it released nothing: it could not find out what the step still holds. An
    /// implementation may also refuse a task that has not ended.
    ///
    /// A step killed on a budget expiry or a panic never returns from `exec`, so its own
    /// cleanup never ran; this settles it, typically from the journal. The runtime calls
    /// it only once a task has ended for good, because a task that will be retried must find
    /// its resources as it left them, and only from the task's own run or once the task's
    /// lease is gone, because a task ended while it ran is still using what it holds.
    async fn abandon(&self, _ctx: &StepContext) -> Result<Vec<FailedRelease>, Reason> {
        Ok(Vec::new())
    }
}

/// A release that failed, under the name its resource was acquired as, with the reason it
/// failed as its producer classified it.
#[derive(Debug)]
pub struct FailedRelease {
    pub resource: String,
    pub reason: Reason,
}

impl std::fmt::Display for FailedRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "releasing `{}`: {}", self.resource, self.reason)
    }
}

/// A resource a finaliser kept in place for a human, under the name it was acquired as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedResource {
    pub resource: String,
    pub what: String,
}

// Fails to build if `Step` stops being object-safe.
const _: Option<&dyn Step> = None;
