//! Runs [`OutputValidator`]s over the whole response once the inner stream has finished.
//!
//! Validators run in registration order, and a `Filter` hands its response to the next one. A
//! `Reject` ends the chain as [`LlmError::ValidationFailed`], which is always `Permanent`: a
//! caller that wants the model to try again catches it itself. The layer drains the stream before
//! validating, so its own consumer sees the events only after the provider has finished. What the
//! validators said goes out as a [`ChatEvent::Validated`] just before `Finished`.

use std::sync::Arc;
use std::time::Instant;

use crate::LlmError;
use crate::LlmEventStream;
use async_trait::async_trait;
use futures::StreamExt;
use tars_types::{
    CallTelemetry, Caller, ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder,
    OutcomeSummary, ValidationOutcome, ValidationSummary,
};

use crate::pipeline::middleware::Middleware;
use crate::pipeline::service::{Next, OpenFailure};

pub mod builtin;

/// Judges a finished response.
///
/// An implementation must not panic on any input; the middleware does not catch a panic, which
/// unwinds the calling task. The chain runs on the blocking pool, so a validator that blocks (a
/// Python callback behind the GIL) stalls only its own request.
pub trait OutputValidator: Send + Sync {
    /// Returns the key of this validator's entry in [`ValidationSummary::outcomes`] and the
    /// `validator` of [`LlmError::ValidationFailed`]. Two validators of one chain with the same
    /// name share one entry, the later one's.
    fn name(&self) -> &str;

    /// Judges `resp`; `req` is there for a validator that needs the prompt.
    fn validate(&self, req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome;
}

/// Runs a chain of [`OutputValidator`]s after the inner stream finishes.
pub struct ValidationMiddleware {
    validators: Arc<[Arc<dyn OutputValidator>]>,
}

impl ValidationMiddleware {
    /// Builds the layer from validators in the order they run.
    pub fn new(validators: Vec<Arc<dyn OutputValidator>>) -> Self {
        Self {
            validators: validators.into(),
        }
    }
}

#[async_trait]
impl Middleware for ValidationMiddleware {
    fn name(&self) -> &'static str {
        "validation"
    }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: Caller,
        next: Next<'_>,
    ) -> Result<LlmEventStream, OpenFailure> {
        // With no validators the stream is not drained, so it keeps streaming.
        if self.validators.is_empty() {
            return next.run(req, ctx).await;
        }

        let started = Instant::now();

        let mut s = next.run(req.clone(), ctx).await?;
        let mut builder = ChatResponseBuilder::new();
        let mut events_held = Vec::new();
        let mut measured = CallTelemetry::default();
        while let Some(ev) = s.next().await {
            match ev {
                Ok(ChatEvent::Measured { telemetry }) => measured = measured.merge(telemetry),
                Ok(ev) => {
                    events_held.push(ev.clone());
                    builder.apply(ev);
                }
                Err(e) => {
                    return Err(OpenFailure::new(e.into(), measured));
                }
            }
        }
        let response = builder.finish();

        // A blocking validator on an async worker would stall every task that worker polls,
        // budget timers included.
        let validators = self.validators.clone();
        let joined =
            tokio::task::spawn_blocking(move || run_chain(&validators, &req, response)).await;
        let ValidatedResponse {
            response,
            mut summary,
            filtered_any,
        } = match chain_outcome(joined) {
            Ok(passed) => passed,
            Err(error) => return Err(OpenFailure::new(error, measured)),
        };
        summary.total_wall_ms = started.elapsed().as_millis() as u64;

        // A filtered response is re-emitted as events; an unfiltered one replays the captured
        // events as they came.
        let body = if filtered_any {
            // The provider's prompt-cache counts from the inner stream's `Started` are kept.
            let cache_hit = response.cache_hit.clone();
            response.into_events(cache_hit)
        } else {
            events_held
        };
        let replayed = with_findings(body, measured, summary);
        Ok(Box::pin(futures::stream::iter(
            replayed.into_iter().map(Ok),
        )))
    }
}

/// Returns `body` with what the layers below measured and the validators' summary handed out just
/// before its `Finished`, or after its last event when it has none.
fn with_findings(
    mut body: Vec<ChatEvent>,
    measured: CallTelemetry,
    summary: ValidationSummary,
) -> Vec<ChatEvent> {
    let at = body
        .iter()
        .position(|ev| matches!(ev, ChatEvent::Finished { .. }))
        .unwrap_or(body.len());
    let findings = [
        ChatEvent::Measured {
            telemetry: measured,
        },
        ChatEvent::Validated { summary },
    ];
    body.splice(at..at, findings);
    body
}

/// A response the whole chain let through, and what each validator said of it.
struct ValidatedResponse {
    response: ChatResponse,
    summary: ValidationSummary,
    filtered_any: bool,
}

/// Runs the validators in registration order; a `Filter` hands its response to the next one,
/// and a `Reject` ends the chain as `ValidationFailed`.
fn run_chain(
    validators: &[Arc<dyn OutputValidator>],
    req: &ChatRequest,
    response: ChatResponse,
) -> Result<ValidatedResponse, LlmError> {
    validators.iter().try_fold(
        ValidatedResponse {
            response,
            summary: ValidationSummary::default(),
            filtered_any: false,
        },
        |mut passed, v| {
            let name = v.name().to_string();
            passed.summary.validators_run.push(name.clone());
            let said = match v.validate(req, &passed.response) {
                ValidationOutcome::Filter {
                    response: filtered,
                    dropped,
                } => {
                    passed.response = filtered;
                    passed.filtered_any = true;
                    OutcomeSummary::Filter { dropped }
                }
                // No summary on Reject: there is no response to attach it to. The error
                // carries the rejected text, which is what the model has to change.
                ValidationOutcome::Reject { reason } => {
                    return Err(LlmError::ValidationFailed {
                        validator: name,
                        reason: Box::new(reason),
                        raw: passed.response.text,
                    });
                }
                ValidationOutcome::Annotate { metrics } => OutcomeSummary::Annotate { metrics },
                // `ValidationOutcome` is `#[non_exhaustive]`; a variant added later counts as Pass.
                _ => OutcomeSummary::Pass,
            };
            passed.summary.outcomes.insert(name, said);
            Ok(passed)
        },
    )
}

/// Returns the chain's own result, or re-raises its panic here: the middleware does not catch a
/// validator's panic, and moving the chain to another thread must not start to.
fn chain_outcome(
    joined: Result<Result<ValidatedResponse, LlmError>, tokio::task::JoinError>,
) -> Result<ValidatedResponse, LlmError> {
    match joined {
        Ok(chain) => chain,
        Err(join) => match join.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_cancelled) => Err(LlmError::ValidationCancelled),
        },
    }
}

#[cfg(test)]
mod tests;
