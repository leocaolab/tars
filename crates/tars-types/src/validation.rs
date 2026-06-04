//! Output-validation primitives shared across crates.
//!
//! `OutputValidator` itself lives in `tars-pipeline` (it depends on
//! Pipeline-internal types). The data types here are what flow
//! between caller code and the validator chain — they need to be
//! constructible / inspectable without taking a `tars-pipeline`
//! dependency. See [Doc 15 — Output Validation](../../../docs/architecture/15-output-validation.md).
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
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::response::ChatResponse;

/// Typed, machine-matchable reason a validator rejected a response.
///
/// Mirrors the `CompatibilityReason { kind, message, detail }` shape
/// (B-31 v4) so a caller's fix-stage can match on a discriminant +
/// structured detail instead of grepping the message string — the
/// brittle contract B-31 v1 already retired for the routing path.
/// Built-in validators emit the typed variants; caller-supplied
/// validators (Python user callbacks, the internal adapter's
/// crash-fallback) go through [`ValidationReason::Custom`].
///
/// `#[non_exhaustive]` so a future built-in validator can add a variant
/// without breaking caller `match` arms — the `Custom` catch-all plus
/// [`ValidationReason::kind`] keep older callers functional.
///
/// Derives serde (externally tagged, snake_case) because — unlike its
/// sibling [`crate::CompatibilityReason`], which is matched only at the
/// call site — a reject reason is persisted into the pipeline event log
/// (`LlmCallFinished::validation_reason`) so evaluators can facet on
/// *why* a validator rejected, not just that one did. External tagging
/// avoids colliding with `Custom`'s own `kind` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ValidationReason {
    /// `response.text` was expected to parse as JSON but didn't.
    /// `parse_error` is the underlying `serde_json` message.
    JsonShape { parse_error: String },

    /// A required response field (`text` / `thinking`) was empty or
    /// whitespace-only. `field` is the field label.
    NotEmpty { field: String },

    /// A response field exceeded its configured character budget.
    MaxLength {
        field: String,
        length: usize,
        max: usize,
    },

    /// Caller-supplied rejection — Python user validators and the
    /// adapter's crash-fallback land here. `kind` is a free-form
    /// caller-chosen discriminant (e.g. `"user"`, `"internal"`),
    /// `message` is human-readable, `detail` an optional structured
    /// payload the caller can branch on.
    Custom {
        kind: String,
        message: String,
        detail: Option<serde_json::Value>,
    },
}

impl ValidationReason {
    /// Stable machine-matchable discriminant string. For built-in
    /// variants it's a fixed snake_case tag; for [`Self::Custom`] it's
    /// the caller-chosen `kind`. Use this in caller fix-stages instead
    /// of substring-matching [`fmt::Display`] output.
    pub fn kind(&self) -> &str {
        match self {
            Self::JsonShape { .. } => "json_shape",
            Self::NotEmpty { .. } => "not_empty",
            Self::MaxLength { .. } => "max_length",
            Self::Custom { kind, .. } => kind,
        }
    }
}

impl fmt::Display for ValidationReason {
    /// Human-readable message. Built-in variants reproduce the exact
    /// strings the W1 string-only `reason` carried, so log scrapers /
    /// error messages are unchanged across the v2 migration.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JsonShape { parse_error } => {
                write!(f, "response.text is not valid JSON: {parse_error}")
            }
            Self::NotEmpty { field } => write!(f, "response.{field} is empty"),
            Self::MaxLength { field, length, max } => {
                write!(
                    f,
                    "response.{field} length={length} exceeds max_chars={max}"
                )
            }
            Self::Custom { message, .. } => write!(f, "{message}"),
        }
    }
}

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
    /// Real consumers (2026-05-08 dogfood feedback) all wire validators
    /// as Filter (drop bad findings, keep batch) and never use the
    /// retry path — same prompt → same model → same output, model
    /// retry on validation failure is a near-pure gamble. Cutting
    /// the field shrinks the surface and removes the temptation.
    /// Callers that genuinely need to re-ask the model should do so
    /// at their own layer with explicit prompt variation.
    ///
    /// `reason` is a typed [`ValidationReason`] (B-20.v2) — callers
    /// match on `reason.kind()` + structured detail rather than parsing
    /// a message string.
    Reject { reason: ValidationReason },

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
    Filter {
        dropped: Vec<String>,
    },
    /// Per-call metrics emitted by the validator. Format is
    /// validator-specific; document per-validator.
    Annotate {
        metrics: HashMap<String, serde_json::Value>,
    },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_reason_kind_and_display_match_builtins() {
        let j = ValidationReason::JsonShape {
            parse_error: "expected `,`".into(),
        };
        assert_eq!(j.kind(), "json_shape");
        assert!(j.to_string().contains("not valid JSON"));

        let m = ValidationReason::MaxLength {
            field: "text".into(),
            length: 12,
            max: 5,
        };
        assert_eq!(m.kind(), "max_length");
        assert!(m.to_string().contains("max_chars=5"));

        // Custom's kind is the caller's own discriminant, not a fixed tag.
        let c = ValidationReason::Custom {
            kind: "snippet".into(),
            message: "no snippet tag".into(),
            detail: Some(serde_json::json!({"rule": "R1"})),
        };
        assert_eq!(c.kind(), "snippet");
        assert_eq!(c.to_string(), "no snippet tag");
    }

    #[test]
    fn validation_reason_serde_round_trips_every_variant() {
        // Externally tagged, snake_case. Round-trip must be lossless so
        // the event store preserves the reason for later faceting.
        let cases = vec![
            ValidationReason::JsonShape {
                parse_error: "bad".into(),
            },
            ValidationReason::NotEmpty {
                field: "thinking".into(),
            },
            ValidationReason::MaxLength {
                field: "text".into(),
                length: 9,
                max: 5,
            },
            ValidationReason::Custom {
                kind: "user".into(),
                message: "nope".into(),
                detail: Some(serde_json::json!({"k": 1})),
            },
        ];
        for r in cases {
            let json = serde_json::to_string(&r).expect("ser");
            let back: ValidationReason = serde_json::from_str(&json).expect("de");
            assert_eq!(r, back, "round-trip mismatch for {json}");
        }
    }

    #[test]
    fn validation_reason_custom_kind_does_not_collide_with_serde_tag() {
        // External tagging is deliberate — internal tagging on `kind`
        // would clash with Custom's own `kind` field. Assert the wire
        // form nests the payload under the variant name.
        let c = ValidationReason::Custom {
            kind: "user".into(),
            message: "m".into(),
            detail: None,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("custom").is_some(), "expected external tag, got {v}");
        assert_eq!(v["custom"]["kind"], "user");
    }
}
