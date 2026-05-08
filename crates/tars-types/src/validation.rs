//! Output-validation primitives shared across crates.
//!
//! `OutputValidator` itself lives in `tars-pipeline` (it depends on
//! Pipeline-internal types). The data types here are what flow
//! between caller code and the validator chain — they need to be
//! constructible / inspectable without taking a `tars-pipeline`
//! dependency. See [Doc 15 — Output Validation](../../../docs/15-output-validation.md).
//!
//! Threaded through:
//!
//! - **`ValidationOutcome`** — a validator's per-call verdict
//!   (Pass / Filter / Reject / Annotate). Returned by validator
//!   implementations; consumed by `ValidationMiddleware`.
//! - **`ValidationSummary`** — final aggregated record of what
//!   validators did during one call, attached to
//!   [`crate::ChatResponse::validation_summary`].
//! - **`OutcomeSummary`** — summary of a single validator's outcome
//!   for the summary record. Reject doesn't appear here because
//!   Reject short-circuits the call into an error path.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::response::ChatResponse;

/// What a validator decides to do with a response. Distinct from
/// [`crate::CompatibilityCheck`] — that one is about routing's
/// pre-flight feature match; this one is about *post-call* output
/// inspection.
///
/// `#[non_exhaustive]` so we can add e.g. `Defer` (re-run validator
/// after a future event) later without breaking match arms.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ValidationOutcome {
    /// Response is fine as-is. No metrics recorded; nothing changes.
    Pass,

    /// Response transformed in-place. The new Response is what
    /// downstream sees (subsequent validators in the chain operate on
    /// `response`, not the pre-Filter version). `dropped` is a
    /// free-form audit list — what got removed/changed — for
    /// telemetry, not control flow.
    Filter {
        response: ChatResponse,
        dropped: Vec<String>,
    },

    /// Validator considers the response unacceptable. Surfaces as
    /// `ProviderError::ValidationFailed` — always classified as
    /// `ErrorClass::Permanent`, never triggers `RetryMiddleware`.
    ///
    /// **Why no `retriable` flag**: the W1 design carried an
    /// `retriable: bool` to let validators ask for a model resample.
    /// Real consumers (arc 2026-05-08 dogfood) all wire validators
    /// as Filter (drop bad findings, keep batch) and never use the
    /// retry path — same prompt → same model → same output, model
    /// retry on validation failure is a near-pure gamble. Cutting
    /// the field shrinks the surface and removes the temptation.
    /// Callers that genuinely need to re-ask the model should do so
    /// at their own layer with explicit prompt variation.
    Reject { reason: String },

    /// Response unchanged, but the validator wants to record per-call
    /// metrics. Propagates into [`ValidationSummary::outcomes`].
    Annotate {
        metrics: HashMap<String, serde_json::Value>,
    },
}

/// Aggregated record of all validators that ran during one call.
/// Attached to [`crate::ChatResponse::validation_summary`].
///
/// Empty when the pipeline didn't include `ValidationMiddleware` — so
/// callers checking `summary.outcomes.is_empty()` can branch on
/// "validation participated at all".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationSummary {
    /// One entry per validator that ran, keyed by
    /// `OutputValidator::name()`. `BTreeMap` for stable ordering
    /// in serialized output (logs / telemetry).
    pub outcomes: BTreeMap<String, OutcomeSummary>,

    /// Validators that participated, in registration order. Captures
    /// the chain shape independent of `outcomes` (which loses the
    /// order via BTreeMap). Used by Python `Response.__repr__` and
    /// debugging.
    pub validators_run: Vec<String>,

    /// Wall time spent in `ValidationMiddleware` for this call.
    pub total_wall_ms: u64,
}

/// A single validator's outcome as recorded in [`ValidationSummary`].
///
/// `Reject` is deliberately absent — when a validator rejects, the
/// call returns `Err(ProviderError::ValidationFailed)` to the
/// caller; there's no Response to attach a summary to. The summary
/// record reflects only outcomes that left the response intact or
/// transformed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutcomeSummary {
    Pass,
    /// What got removed/changed. Caller decides format (e.g.
    /// list of finding IDs that were demoted to ad-hoc).
    Filter { dropped: Vec<String> },
    /// Per-call metrics emitted by the validator. Format is
    /// validator-specific; document per-validator.
    Annotate { metrics: HashMap<String, serde_json::Value> },
}

/// Shared handle for `ValidationMiddleware` to publish the per-call
/// `ValidationSummary` (and the post-Filter `ChatResponse`) back to
/// the outer caller.
///
/// **Why a side channel instead of riding through the event stream**:
/// `ChatEvent` is the streaming-token contract — adding "Validation
/// Summary" or "Filtered Body" event variants pollutes that surface
/// for an end-of-stream concept. The side channel mirrors how
/// `SharedTelemetry` works (Stage 4) — caller pre-creates the handle
/// in [`crate::RequestContext`], ValidationMiddleware writes through
/// the same Arc, caller reads after the stream drains.
///
/// The handle holds:
/// - `summary`: aggregated validator outcomes
/// - `filtered_response`: the post-Filter ChatResponse if Filter ran;
///   `None` when no Filter happened (response unchanged from stream)
pub type SharedValidationOutcome = Arc<Mutex<ValidationOutcomeRecord>>;

#[derive(Debug, Default)]
pub struct ValidationOutcomeRecord {
    pub summary: ValidationSummary,
    /// Set iff a Filter validator ran. The OUTER caller's builder
    /// will receive the unmodified streamed response; this field
    /// supplies the filtered version which the caller substitutes
    /// post-stream-drain.
    pub filtered_response: Option<ChatResponse>,
}

/// Construct a fresh `SharedValidationOutcome` for a new request.
pub fn new_shared_validation_outcome() -> SharedValidationOutcome {
    Arc::new(Mutex::new(ValidationOutcomeRecord::default()))
}
