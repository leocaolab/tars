//! Defines [`LlmError`], what an [`LlmService`](crate::LlmService) call fails with: a provider's
//! failure, or a refusal or failure of the chain itself. A provider never produces the chain's
//! variants, and the chain reports a provider's failure as [`LlmError::Provider`] unchanged.

use std::time::Duration;

use tars_types::{
    CompatibilityReason, ErrorClass, LlmErrorKind, RECORD_RAW_MAX, Reason, TenantId,
    ValidationReason,
};
use tars_utils::display_chain;
use thiserror::Error;

use crate::ProviderError;
use crate::pipeline::middleware::tenant_budget::BudgetStoreError;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// The provider's profile says it cannot serve the request, so it was not sent. It is
    /// permanent for this provider; another provider may serve it.
    #[error("provider cannot serve the request: {}", join_reasons(reasons))]
    Incompatible {
        reasons: tars_types::NonEmpty<CompatibilityReason>,
    },

    /// A budget layer refused the call before it was sent.
    #[error("budget exceeded")]
    BudgetExceeded,

    /// An output validator rejected the response. It is permanent; see
    /// [`ValidationOutcome::Reject`](tars_types::ValidationOutcome::Reject) for why. `raw` is the
    /// text of the response it rejected, which is what the model has to change.
    #[error("validation failed: {validator}: {reason}")]
    ValidationFailed {
        validator: String,
        reason: Box<ValidationReason>,
        raw: String,
    },

    /// The runtime shut down before the validator chain ran, so the response was not checked.
    #[error("the runtime shut down before the validator chain ran")]
    ValidationCancelled,

    /// The tenant budget store failed, and the call is refused so an outage cannot uncap a tenant.
    #[error("tenant budget store error")]
    BudgetStoreFailed(#[source] BudgetStoreError),

    /// The tenant budget store returned a NaN balance.
    #[error("tenant budget store returned a NaN balance for tenant {tenant}")]
    BudgetBalanceCorrupt { tenant: TenantId },

    /// The caller cancelled the call while Retry was between attempts.
    #[error("cancelled by the caller between retry attempts")]
    Cancelled,
}

impl LlmError {
    /// Returns whether the cause goes away with time.
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Provider(e) => e.class(),
            Self::Incompatible { .. }
            | Self::BudgetExceeded
            | Self::ValidationFailed { .. }
            | Self::ValidationCancelled
            | Self::BudgetStoreFailed(_)
            | Self::BudgetBalanceCorrupt { .. }
            | Self::Cancelled => ErrorClass::Permanent,
        }
    }

    /// Returns how long the provider is unavailable for, when something says; see
    /// [`ProviderError::unavailable_for`].
    pub fn unavailable_for(&self) -> Option<Duration> {
        match self {
            Self::Provider(e) => e.unavailable_for(),
            _ => None,
        }
    }

    /// Returns the error's kind without its payload, which the call log, telemetry, fallback
    /// triggers and the bindings key on.
    pub fn kind(&self) -> LlmErrorKind {
        match self {
            Self::Provider(e) => e.kind(),
            Self::Incompatible { .. } => LlmErrorKind::Incompatible,
            Self::BudgetExceeded => LlmErrorKind::BudgetExceeded,
            Self::ValidationFailed { .. } => LlmErrorKind::ValidationFailed,
            Self::ValidationCancelled => LlmErrorKind::ValidationCancelled,
            Self::BudgetStoreFailed(_) => LlmErrorKind::BudgetStoreFailed,
            Self::BudgetBalanceCorrupt { .. } => LlmErrorKind::BudgetBalanceCorrupt,
            Self::Cancelled => LlmErrorKind::Cancelled,
        }
    }
}

/// Says what a failed service call means to the step that made it: a provider's failure as
/// [`ProviderError`] says, a rejected answer goes back to the model with its text, a refusal of
/// the chain is a failed call, and a failure of the chain's own state never reaches the model.
impl From<LlmError> for Reason {
    fn from(e: LlmError) -> Self {
        match e {
            LlmError::Provider(e) => Reason::from(e),
            LlmError::ValidationFailed { ref raw, .. } => {
                Reason::invalid_input_truncated(raw, display_chain(&e), RECORD_RAW_MAX)
            }
            LlmError::Incompatible { .. } | LlmError::BudgetExceeded => {
                Reason::CallFailed(display_chain(&e))
            }
            LlmError::ValidationCancelled | LlmError::Cancelled => Reason::Cancelled,
            LlmError::BudgetStoreFailed(_) | LlmError::BudgetBalanceCorrupt { .. } => {
                Reason::defect(display_chain(&e))
            }
        }
    }
}

fn join_reasons(reasons: &[CompatibilityReason]) -> String {
    reasons
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each chain variant maps to its own kind, and to the class `ProviderError` gave the same
    /// failure before it moved here.
    #[test]
    fn each_chain_failure_keeps_its_kind_and_class() {
        let cases = [
            (
                LlmError::Incompatible {
                    reasons: tars_types::NonEmpty::new(CompatibilityReason::VisionUnsupported, []),
                },
                LlmErrorKind::Incompatible,
                ErrorClass::Permanent,
            ),
            (
                LlmError::BudgetExceeded,
                LlmErrorKind::BudgetExceeded,
                ErrorClass::Permanent,
            ),
            (
                LlmError::ValidationFailed {
                    validator: "v".into(),
                    reason: Box::new(ValidationReason::NotEmpty {
                        field: "text".into(),
                    }),
                    raw: String::new(),
                },
                LlmErrorKind::ValidationFailed,
                ErrorClass::Permanent,
            ),
            (
                LlmError::ValidationCancelled,
                LlmErrorKind::ValidationCancelled,
                ErrorClass::Permanent,
            ),
            (
                LlmError::BudgetStoreFailed(BudgetStoreError::Backend("down".into())),
                LlmErrorKind::BudgetStoreFailed,
                ErrorClass::Permanent,
            ),
            (
                LlmError::BudgetBalanceCorrupt {
                    tenant: TenantId::new("t"),
                },
                LlmErrorKind::BudgetBalanceCorrupt,
                ErrorClass::Permanent,
            ),
            (
                LlmError::Cancelled,
                LlmErrorKind::Cancelled,
                ErrorClass::Permanent,
            ),
        ];
        for (err, kind, class) in cases {
            assert_eq!(err.kind(), kind, "{err:?}");
            assert_eq!(err.class(), class, "{err:?}");
            assert_eq!(err.unavailable_for(), None, "{err:?}");
        }
    }

    #[test]
    fn a_provider_failure_keeps_its_kind_class_and_message() {
        let e = LlmError::from(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        });
        assert_eq!(e.kind(), LlmErrorKind::RateLimited);
        assert_eq!(e.class(), ErrorClass::Transient);
        assert_eq!(e.unavailable_for(), Some(Duration::from_secs(3)));
        assert_eq!(e.to_string(), "rate limited (retry_after=Some(3s))");
    }

    #[test]
    fn a_rejected_answer_goes_back_to_the_model_with_its_text() {
        let reason = Reason::from(LlmError::ValidationFailed {
            validator: "json".into(),
            reason: Box::new(ValidationReason::JsonShape {
                parse_error: "expected value".into(),
            }),
            raw: "sure! here is the JSON".into(),
        });
        match &reason {
            Reason::InvalidInput { raw, .. } => assert_eq!(raw, "sure! here is the JSON"),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert!(reason.may_reach_model());
    }

    #[test]
    fn a_failure_of_the_chains_own_state_never_reaches_the_model() {
        for e in [
            LlmError::BudgetStoreFailed(BudgetStoreError::Backend("down".into())),
            LlmError::BudgetBalanceCorrupt {
                tenant: TenantId::new("t"),
            },
        ] {
            let reason = Reason::from(e);
            assert!(matches!(reason, Reason::Defect { .. }), "{reason:?}");
            assert!(!reason.may_reach_model());
        }
    }

    #[test]
    fn a_refusal_of_the_chain_is_a_failed_call_the_model_reads() {
        for e in [
            LlmError::BudgetExceeded,
            LlmError::Incompatible {
                reasons: tars_types::NonEmpty::new(CompatibilityReason::VisionUnsupported, []),
            },
        ] {
            let reason = Reason::from(e);
            assert!(matches!(reason, Reason::CallFailed(_)), "{reason:?}");
            assert!(reason.may_reach_model());
            assert!(!reason.is_retryable());
        }
    }

    #[test]
    fn a_cancelled_call_is_a_cancelled_step() {
        for e in [LlmError::Cancelled, LlmError::ValidationCancelled] {
            assert!(matches!(Reason::from(e), Reason::Cancelled));
        }
    }
}
