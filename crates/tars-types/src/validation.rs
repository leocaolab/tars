//! Defines what output validators return and what one call's validation leaves behind. They sit
//! below tars-llm, where `OutputValidator` and `ValidationMiddleware` live, because tars-melt,
//! which tars-llm depends on, records the summary. The design is in
//! `docs/architecture/15-output-validation.md`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::providers::response::ChatResponse;

/// Why a validator rejected a response, as a discriminant plus structured detail a caller can
/// match on.
///
/// It is persisted in the call log (the `llm_call` row's `validation_reason` column) so
/// evaluators can facet on why a validator rejected. The serde form is externally tagged
/// because an internal `kind` tag would collide with `Custom`'s own `kind` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ValidationReason {
    JsonShape {
        parse_error: String,
    },

    NotEmpty {
        field: String,
    },

    MaxLength {
        field: String,
        length: usize,
        max: usize,
    },

    Custom {
        kind: String,
        message: String,
        detail: Option<serde_json::Value>,
    },

    /// The validator reached no verdict: it raised, or returned something that is not an
    /// outcome. The call still fails, but this is the validator's bug, not a judgment on the
    /// response.
    ValidatorFailed {
        error: String,
        /// The validator's stack at the failure, when the failure had one.
        traceback: Option<String>,
    },
}

impl ValidationReason {
    /// Returns the reason's discriminant: the built-in variant's name, or a `Custom` reason's own
    /// `kind`. Match on this rather than on the `Display` text.
    pub fn kind(&self) -> &str {
        match self {
            Self::JsonShape { .. } => "json_shape",
            Self::NotEmpty { .. } => "not_empty",
            Self::MaxLength { .. } => "max_length",
            Self::Custom { kind, .. } => kind,
            Self::ValidatorFailed { .. } => "validator_failed",
        }
    }
}

impl fmt::Display for ValidationReason {
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
            Self::ValidatorFailed { error, .. } => write!(f, "the validator failed: {error}"),
        }
    }
}

/// One validator's verdict on a response after the call.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ValidationOutcome {
    Pass,

    /// Replaces the response; later validators in the chain see `response`, not the original.
    Filter {
        response: ChatResponse,
        dropped: Vec<String>,
    },

    /// Refuses the response. The call fails with tars-llm's `LlmError::ValidationFailed`, which is
    /// permanent and never retried: the same prompt to the same model mostly gives the same
    /// output, so a caller that wants to re-ask varies the prompt at its own layer.
    Reject {
        reason: ValidationReason,
    },

    /// Leaves the response unchanged and records per-call metrics in
    /// [`ValidationSummary::outcomes`].
    Annotate {
        metrics: HashMap<String, serde_json::Value>,
    },
}

/// What the validators of one call did, sent as [`crate::ChatEvent::Validated`] and folded into
/// [`crate::ChatResponse::validation_summary`]. It is empty when the pipeline has no
/// `ValidationMiddleware`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationSummary {
    /// One entry per validator that ran, keyed by `OutputValidator::name()`.
    pub outcomes: BTreeMap<String, OutcomeSummary>,

    /// The validators that ran, in registration order, which `outcomes` does not keep.
    pub validators_run: Vec<String>,

    /// Wall time spent in `ValidationMiddleware` for this call, in milliseconds.
    pub total_wall_ms: u64,
}

/// One validator's outcome as recorded in [`ValidationSummary`]. It has no `Reject`: a rejected
/// call returns `Err(LlmError::ValidationFailed)` and has no response to carry a summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutcomeSummary {
    Pass,
    /// What the filter removed, in a format the validator chooses.
    Filter {
        dropped: Vec<String>,
    },
    /// Per-call metrics in a format the validator chooses.
    Annotate {
        metrics: HashMap<String, serde_json::Value>,
    },
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
    fn a_validator_that_failed_is_its_own_kind_and_keeps_its_traceback_through_serde() {
        let r = ValidationReason::ValidatorFailed {
            error: "ValueError: bug".into(),
            traceback: Some("  File \"v.py\", line 3, in check\n".into()),
        };
        assert_eq!(r.kind(), "validator_failed");
        assert_eq!(r.to_string(), "the validator failed: ValueError: bug");
        let back: ValidationReason =
            serde_json::from_str(&serde_json::to_string(&r).expect("ser")).expect("de");
        assert_eq!(r, back);
    }

    #[test]
    fn validation_reason_custom_kind_does_not_collide_with_serde_tag() {
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
