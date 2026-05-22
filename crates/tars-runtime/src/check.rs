//! `Invariant` — test-time property checks on a single output.
//!
//! Slice 1 of the agent-testing architecture
//! ([`docs/architecture/18-agent-testing.md`](../../../docs/architecture/18-agent-testing.md)
//! §4.1). An invariant is a postcondition every valid output must
//! satisfy, checkable on one `(request, response)` pair with **no
//! oracle** — no gold standard, no judge.
//!
//! ## Relationship to `OutputValidator`
//!
//! `tars_pipeline::OutputValidator` is a **production-gating** concern:
//! it runs inline on the request hot path and returns
//! Pass/Filter/Reject/Annotate, where Filter/Reject mutate or fail the
//! response. `Invariant` is a **test** concern: it returns pass/fail +
//! detail for aggregation into a behavior report, never mutates
//! anything.
//!
//! They overlap on "does this output satisfy a property," so rather
//! than reimplement JSON / non-empty / length checks we provide
//! [`ValidatorInvariant`] to adapt any existing `OutputValidator` into
//! an `Invariant`. The only net-new built-in here is
//! [`MembershipInvariant`] (closed-set membership = the free
//! hallucination check that recurs in 3 of 5 field case studies and
//! has no validator equivalent).

use std::collections::HashSet;
use std::sync::Arc;

use tars_pipeline::OutputValidator;
use tars_types::{ChatRequest, ChatResponse, ValidationOutcome};

/// Outcome of one invariant check against one output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub passed: bool,
    /// Why it failed (or any note on pass). `None` keeps the common
    /// pass case allocation-free.
    pub detail: Option<String>,
}

impl CheckResult {
    pub fn pass() -> Self {
        Self {
            passed: true,
            detail: None,
        }
    }

    pub fn fail(detail: impl Into<String>) -> Self {
        Self {
            passed: false,
            detail: Some(detail.into()),
        }
    }
}

/// A postcondition every valid output must satisfy. No oracle required.
pub trait Invariant: Send + Sync {
    /// Stable name — the key under which this invariant's violation
    /// rate shows up in a behavior report.
    fn name(&self) -> &str;

    /// Check the property on one `(request, response)` pair.
    fn check(&self, input: &ChatRequest, output: &ChatResponse) -> CheckResult;
}

// ─── MembershipInvariant — the net-new built-in ───────────────────────

/// Closed-set membership: every value extracted from the output must
/// be a member of an allowed set. Violation = the output referenced
/// something not in the set = a hallucination, caught with a `HashSet`
/// lookup and no judge.
///
/// Covers the recurring field pattern: generated category ∈ taxonomy
/// (Sincera), recommended tool ∈ tool DB (RAG), endpoint path ∈ source
/// page (Zapier). The caller supplies the extractor because *what* to
/// pull out of the response (a field, a list, the whole text) is
/// domain-specific; the membership check is generic.
pub struct MembershipInvariant {
    name: String,
    allowed: HashSet<String>,
    extract: Box<dyn Fn(&ChatResponse) -> Vec<String> + Send + Sync>,
}

impl MembershipInvariant {
    /// `extract` pulls the candidate value(s) out of a response (e.g.
    /// recommended tool names). Every extracted value must be in
    /// `allowed`; any that isn't fails the invariant.
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

    /// Convenience: the entire response text (trimmed) must be one of
    /// the allowed values. For single-label classification outputs.
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

    fn check(&self, _input: &ChatRequest, output: &ChatResponse) -> CheckResult {
        let extracted = (self.extract)(output);
        let offenders: Vec<String> = extracted
            .into_iter()
            .filter(|v| !self.allowed.contains(v))
            .collect();
        if offenders.is_empty() {
            CheckResult::pass()
        } else {
            CheckResult::fail(format!(
                "{} value(s) not in allowed set: {}",
                offenders.len(),
                offenders.join(", ")
            ))
        }
    }
}

// ─── ValidatorInvariant — reuse existing OutputValidators ─────────────

/// Adapts any [`OutputValidator`] into an [`Invariant`] so the existing
/// built-ins (`JsonShapeValidator`, `NotEmptyValidator`,
/// `MaxLengthValidator`, …) are reusable as test-time checks without
/// reimplementation.
///
/// Outcome mapping:
/// - `Pass` / `Annotate` → invariant **holds**
/// - `Reject` / `Filter` → invariant **violated** (Filter means the
///   validator found content to drop — i.e. the property failed on
///   part of the output)
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

    fn check(&self, input: &ChatRequest, output: &ChatResponse) -> CheckResult {
        match self.inner.validate(input, output) {
            ValidationOutcome::Pass | ValidationOutcome::Annotate { .. } => CheckResult::pass(),
            ValidationOutcome::Reject { reason } => CheckResult::fail(reason),
            ValidationOutcome::Filter { dropped, .. } => {
                CheckResult::fail(format!("filtered {} item(s)", dropped.len()))
            }
            // ValidationOutcome is #[non_exhaustive]; a future variant
            // defaults to "holds" rather than guessing it's a violation
            // (conservative — a new outcome shouldn't silently fail tests).
            _ => CheckResult::pass(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{ChatResponse, ModelHint, StopReason, Usage};

    fn req() -> ChatRequest {
        ChatRequest::user(ModelHint::Explicit("m".into()), "x")
    }

    fn resp_text(text: &str) -> ChatResponse {
        // Construct a minimal ChatResponse with the given text.
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
        assert!(inv.check(&req(), &resp_text("apparel")).passed);
        // surrounding whitespace tolerated
        assert!(inv.check(&req(), &resp_text("  home \n")).passed);
        let bad = inv.check(&req(), &resp_text("weapons"));
        assert!(!bad.passed);
        assert!(bad.detail.unwrap().contains("weapons"));
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
        assert!(
            inv.check(&req(), &resp_text("midjourney, dall-e"))
                .passed
        );
        // two hallucinated
        let bad = inv.check(&req(), &resp_text("midjourney, fake-tool, other-fake"));
        assert!(!bad.passed);
        let d = bad.detail.unwrap();
        assert!(d.contains("2 value"));
        assert!(d.contains("fake-tool") && d.contains("other-fake"));
    }

    #[test]
    fn validator_adapter_maps_outcomes() {
        use tars_pipeline::NotEmptyValidator;

        let inv = ValidatorInvariant::new(Arc::new(NotEmptyValidator::default()));
        // non-empty → Pass → holds
        assert!(inv.check(&req(), &resp_text("hello")).passed);
        // empty → Reject → violated
        assert!(!inv.check(&req(), &resp_text("")).passed);
    }

    #[test]
    fn check_result_constructors() {
        assert!(CheckResult::pass().passed);
        assert!(CheckResult::pass().detail.is_none());
        let f = CheckResult::fail("nope");
        assert!(!f.passed);
        assert_eq!(f.detail.as_deref(), Some("nope"));
    }
}
