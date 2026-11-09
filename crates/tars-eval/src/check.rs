//! Checks test-time invariants: postconditions every valid output satisfies, checkable on one
//! `(request, response)` pair with no oracle, no gold standard and no judge
//! (`docs/architecture/18-agent-testing.md` §4.1).
//!
//! An invariant only reports pass or fail for a behavior report and changes nothing; gating
//! responses in production is `tars_llm::OutputValidator`'s job. [`ValidatorInvariant`]
//! reuses any validator as an invariant, so the JSON, non-empty and length checks exist once.

use std::collections::HashSet;
use std::sync::Arc;

use tars_llm::OutputValidator;
use tars_types::{ChatRequest, ChatResponse, ValidationOutcome};

/// Whether one output satisfied a property: it held, perhaps with a note, or it was violated,
/// always for a reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Held { note: Option<String> },
    Violated { reason: String },
}

impl CheckOutcome {
    pub fn pass() -> Self {
        Self::Held { note: None }
    }

    pub fn fail(reason: impl Into<String>) -> Self {
        Self::Violated {
            reason: reason.into(),
        }
    }

    pub fn passed(&self) -> bool {
        matches!(self, Self::Held { .. })
    }

    /// Returns the note of a pass or the reason of a violation.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Held { note } => note.as_deref(),
            Self::Violated { reason } => Some(reason),
        }
    }
}

/// A postcondition every valid output must satisfy. No oracle required.
pub trait Invariant: Send + Sync {
    /// Returns the stable name a behavior report keys this invariant's violation rate by.
    fn name(&self) -> &str;

    fn check(&self, input: &ChatRequest, output: &ChatResponse) -> CheckOutcome;
}

// ─── MembershipInvariant ──────────────────────────────────────────────

type ExtractFn = Box<dyn Fn(&ChatResponse) -> Vec<String> + Send + Sync>;

/// Requires every value extracted from the output to be in an allowed set; a value outside it is
/// a hallucination, caught without a judge (a category in the taxonomy, a recommended tool in the
/// tool DB). What to extract is the caller's, because it depends on the domain.
pub struct MembershipInvariant {
    name: String,
    allowed: HashSet<String>,
    extract: ExtractFn,
}

impl MembershipInvariant {
    pub fn new<I, S>(
        name: impl Into<String>,
        allowed: I,
        extract: impl Fn(&ChatResponse) -> Vec<String> + Send + Sync + 'static,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            name: name.into(),
            allowed: allowed.into_iter().map(Into::into).collect(),
            extract: Box::new(extract),
        }
    }

    /// Builds an invariant that the whole trimmed text is one allowed value.
    pub fn whole_text<I, S>(name: impl Into<String>, allowed: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(name, allowed, |resp| vec![resp.text.trim().to_string()])
    }
}

impl Invariant for MembershipInvariant {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, _input: &ChatRequest, output: &ChatResponse) -> CheckOutcome {
        let extracted = (self.extract)(output);
        let offenders: Vec<String> = extracted
            .into_iter()
            .filter(|v| !self.allowed.contains(v))
            .collect();
        if offenders.is_empty() {
            CheckOutcome::pass()
        } else {
            CheckOutcome::fail(format!(
                "{} value(s) not in allowed set: {}",
                offenders.len(),
                offenders.join(", ")
            ))
        }
    }
}

// ─── CheckRunner — run a set of invariants over one output ────────────

/// Runs a set of [`Invariant`]s against one `(request, response)` pair. Rates across a corpus are
/// the caller's, which owns the corpus loop.
#[derive(Clone, Default)]
pub struct CheckRunner {
    invariants: Vec<Arc<dyn Invariant>>,
}

impl CheckRunner {
    pub fn new(invariants: Vec<Arc<dyn Invariant>>) -> Self {
        Self { invariants }
    }

    pub fn is_empty(&self) -> bool {
        self.invariants.is_empty()
    }

    pub fn run(&self, input: &ChatRequest, output: &ChatResponse) -> Vec<(String, CheckOutcome)> {
        self.invariants
            .iter()
            .map(|inv| (inv.name().to_string(), inv.check(input, output)))
            .collect()
    }

    /// Returns the invariant names in order.
    pub fn names(&self) -> Vec<&str> {
        self.invariants.iter().map(|i| i.name()).collect()
    }
}

// ─── ValidatorInvariant — reuse existing OutputValidators ─────────────

/// Runs an [`OutputValidator`] as an [`Invariant`].
///
/// `Pass` and `Annotate` hold; `Reject` is violated, and so is a `Filter` that dropped something,
/// since the property failed on that part of the output.
pub struct ValidatorInvariant {
    inner: Arc<dyn OutputValidator>,
}

impl ValidatorInvariant {
    pub fn new(validator: Arc<dyn OutputValidator>) -> Self {
        Self { inner: validator }
    }
}

impl Invariant for ValidatorInvariant {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn check(&self, input: &ChatRequest, output: &ChatResponse) -> CheckOutcome {
        match self.inner.validate(input, output) {
            ValidationOutcome::Pass | ValidationOutcome::Annotate { .. } => CheckOutcome::pass(),
            ValidationOutcome::Reject { reason } => CheckOutcome::fail(reason.to_string()),
            ValidationOutcome::Filter { dropped, .. } => {
                if dropped.is_empty() {
                    CheckOutcome::pass()
                } else {
                    CheckOutcome::fail(format!("filtered {} item(s)", dropped.len()))
                }
            }
            // `ValidationOutcome` is non-exhaustive. An outcome this arm does not know fails:
            // passing it could let unvalidated content through.
            _ => {
                tracing::warn!(
                    invariant = self.inner.name(),
                    "ValidatorInvariant: unhandled ValidationOutcome variant treated as FAIL \
                     (fail-closed); add an explicit arm when new variants land",
                );
                CheckOutcome::fail(
                    "the validator returned an outcome this version of tars does not know"
                        .to_string(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{ChatResponse, StopReason, Usage};

    fn req() -> ChatRequest {
        ChatRequest::user("x")
    }

    fn resp_text(text: &str) -> ChatResponse {
        use tars_types::ChatResponseBuilder;
        let mut b = ChatResponseBuilder::new();
        b.apply(tars_types::ChatEvent::started("m"));
        b.apply(tars_types::ChatEvent::Delta { text: text.into() });
        b.apply(tars_types::ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        });
        b.finish()
    }

    #[test]
    fn membership_whole_text_pass_and_fail() {
        let inv = MembershipInvariant::whole_text(
            "category_in_taxonomy",
            ["electronics", "apparel", "home"],
        );
        assert!(inv.check(&req(), &resp_text("apparel")).passed());
        // surrounding whitespace tolerated
        assert!(inv.check(&req(), &resp_text("  home \n")).passed());
        let bad = inv.check(&req(), &resp_text("weapons"));
        assert!(!bad.passed());
        assert!(bad.detail().unwrap().contains("weapons"));
    }

    #[test]
    fn membership_with_extractor_lists_all_offenders() {
        // Extractor: comma-split the text into multiple recommended items.
        let inv = MembershipInvariant::new(
            "tools_in_db",
            ["midjourney", "dall-e", "stable-diffusion"],
            |resp| resp.text.split(',').map(|s| s.trim().to_string()).collect(),
        );
        // all valid
        assert!(inv.check(&req(), &resp_text("midjourney, dall-e")).passed());
        // two hallucinated
        let bad = inv.check(&req(), &resp_text("midjourney, fake-tool, other-fake"));
        assert!(!bad.passed());
        let d = bad.detail().unwrap();
        assert!(d.contains("2 value"));
        assert!(d.contains("fake-tool") && d.contains("other-fake"));
    }

    #[test]
    fn validator_adapter_maps_outcomes() {
        use tars_llm::NotEmptyValidator;

        let inv = ValidatorInvariant::new(Arc::new(NotEmptyValidator::default()));
        // non-empty → Pass → holds
        assert!(inv.check(&req(), &resp_text("hello")).passed());
        // empty → Reject → violated
        assert!(!inv.check(&req(), &resp_text("")).passed());
    }

    #[test]
    fn check_result_constructors() {
        assert!(CheckOutcome::pass().passed());
        assert!(CheckOutcome::pass().detail().is_none());
        let f = CheckOutcome::fail("nope");
        assert!(!f.passed());
        assert_eq!(f.detail(), Some("nope"));
    }
}
