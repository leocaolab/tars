//! Defines what `tars eval run` records and `judge` / `diff` read back: the run's manifest, each
//! case's report and each check's outcome on a case. Every record holds only facts observed when
//! the run ran; counts, totals and rates are computed from them on reading.
//!
//! A file an older version wrote still reads. A fact it did not record reads as unknown: an error
//! recorded as bare text has no kind ([`CaseFailure::Unclassified`]), and a check summary it kept
//! beside the cases is read for its name only, the rest being recomputed from the cases.

use std::ffi::OsStr;
use std::path::{Component, Path};

use serde::{Deserialize, Deserializer, Serialize};
use tars_config::Vendor;
use tars_types::{LlmErrorKind, ProviderId, Usage};

use crate::{JudgeFailure, VendorCheck};

/// The run summary written to `<output>/manifest.json`, which `judge` and `diff` read back.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// The corpus directory as an absolute path, symlinks resolved, taken before the first call.
    pub corpus_path: String,
    pub provider_id: ProviderId,
    /// The concrete model the run's calls were bound to: the given spec resolved against the
    /// provider's catalog, else the provider's default.
    pub model: String,
    /// The vendor the model catalog names for `model`, recorded when the run ran; `None` for a
    /// model the catalog does not know and for a run written before vendors were recorded.
    #[serde(default)]
    pub vendor: Option<Vendor>,
    /// What was known of the `trajectory-match:args-judge` judge's vendor against `vendor`;
    /// `None` when no check used that judge, or the run predates recording it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_judge_vendors: Option<VendorCheck>,
    /// UTC millis when the first case started.
    pub started_at_ms: u64,
    /// UTC millis when the last case finished.
    pub ended_at_ms: u64,
    /// The names of the checks the run ran, in the order given, each rolled up by
    /// [`Self::check_summaries`].
    #[serde(default, deserialize_with = "check_names")]
    pub checks: Vec<String>,
    /// Per-case summary in run order, the same as each case's `report.json`.
    pub cases: Vec<CaseReport>,
}

impl Manifest {
    pub fn case_count(&self) -> usize {
        self.cases.len()
    }

    /// Returns how many cases' calls completed.
    pub fn success_count(&self) -> usize {
        self.cases.iter().filter(|c| c.succeeded()).count()
    }

    pub fn error_count(&self) -> usize {
        self.case_count() - self.success_count()
    }

    /// Returns the sum of `Usage` across the cases whose call completed.
    pub fn total_usage(&self) -> Usage {
        self.cases
            .iter()
            .filter(|c| c.succeeded())
            .fold(Usage::default(), |total, c| total.merge(c.usage))
    }

    /// Rolls each of [`Self::checks`] up over the cases, in that order.
    pub fn check_summaries(&self) -> Vec<CheckSummary> {
        self.checks
            .iter()
            .map(|name| summarize(name, &self.cases))
            .collect()
    }
}

/// Reads a manifest's `checks`: names, or, as an older version wrote them, summaries whose name
/// is kept and whose counts are recomputed from the cases.
fn check_names<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Name(String),
        Summary { name: String },
    }
    Ok(Vec::<Entry>::deserialize(d)?
        .into_iter()
        .map(|e| match e {
            Entry::Name(name) | Entry::Summary { name } => name,
        })
        .collect())
}

/// One check rolled up over a run's cases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckSummary {
    pub name: String,
    /// Cases the check decided on: successful cases, less any a trajectory check skipped and any
    /// its judge could not decide.
    pub evaluated: u32,
    pub violations: u32,
    /// Cases an `args-judge` check's judge failed on.
    pub not_judged: u32,
}

impl CheckSummary {
    /// Returns `violations / evaluated`; `None` when the check decided on no case, which says
    /// nothing about the model.
    pub fn violation_rate(&self) -> Option<f64> {
        (self.evaluated > 0).then(|| f64::from(self.violations) / f64::from(self.evaluated))
    }
}

/// Rolls the check `name` up over `cases`.
pub(crate) fn summarize(name: &str, cases: &[CaseReport]) -> CheckSummary {
    let mut summary = CheckSummary {
        name: name.to_string(),
        evaluated: 0,
        violations: 0,
        not_judged: 0,
    };
    for check in cases
        .iter()
        .flat_map(|c| c.checks())
        .filter(|c| c.name() == name)
    {
        match check {
            CaseCheckResult::Passed { .. } => summary.evaluated += 1,
            CaseCheckResult::Failed { .. } => {
                summary.evaluated += 1;
                summary.violations += 1;
            }
            CaseCheckResult::NotJudged { .. } => summary.not_judged += 1,
        }
    }
    summary
}

/// The name of a case: its directory in the corpus and in the run. A single path segment, so a
/// case's files are always inside the directory they are joined to, whoever wrote the manifest.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CaseId(String);

/// A case name that is not one path segment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("case id {name:?} is not a single directory name")]
pub struct ParseCaseIdError {
    pub name: String,
}

impl CaseId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CaseId {
    type Error = ParseCaseIdError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if is_one_segment(&name) {
            Ok(Self(name))
        } else {
            Err(ParseCaseIdError { name })
        }
    }
}

impl std::str::FromStr for CaseId {
    type Err = ParseCaseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.to_string())
    }
}

impl From<CaseId> for String {
    fn from(id: CaseId) -> Self {
        id.0
    }
}

impl std::fmt::Display for CaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Returns whether `name` is exactly one ordinary path segment: not empty, not `.` or `..`, no
/// separator, not a root or a drive.
fn is_one_segment(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(part)), None) if part == OsStr::new(name)
    )
}

/// What one case's call came to, with what was observed of it whatever its outcome.
///
/// On the wire `status` is the outcome's tag, beside the other fields:
///
/// ```text
///   {"case_id": "c", "wall_clock_ms": 9, "usage": {..}, "output_chars": 2, "tool_trajectory": [..], "status": "ok", "checks": [..]}
///   {"case_id": "d", "wall_clock_ms": 9, "usage": {..}, "output_chars": 0, "tool_trajectory": [], "status": "error", "failure": {"cause": "call", "kind": "unfinished", "message": ".."}}
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseReport {
    pub case_id: CaseId,
    pub wall_clock_ms: u64,
    /// What the provider reported spending, on a failed call too.
    pub usage: Usage,
    /// Length of the text received in chars, all of it or what came before the failure; the text
    /// is in `<case>/output.txt`.
    pub output_chars: u64,
    /// Tool names the model selected, in call order, all of them or those before the failure;
    /// recorded whatever checks ran so that `diff --trajectory` can compare runs.
    #[serde(default)]
    pub tool_trajectory: Vec<String>,
    #[serde(flatten)]
    pub outcome: CaseOutcome,
}

impl CaseReport {
    pub fn succeeded(&self) -> bool {
        matches!(self.outcome, CaseOutcome::Ok { .. })
    }

    /// Returns the case's check outcomes; none for a failed call, which has no output to check.
    pub fn checks(&self) -> &[CaseCheckResult] {
        match &self.outcome {
            CaseOutcome::Ok { checks, .. } => checks,
            CaseOutcome::Error { .. } => &[],
        }
    }
}

/// Whether a case's call completed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CaseOutcome {
    Ok {
        #[serde(default)]
        checks: Vec<CaseCheckResult>,
    },
    Error {
        #[serde(alias = "error", deserialize_with = "failure_or_text")]
        failure: CaseFailure,
    },
}

/// Why a case's call did not complete.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum CaseFailure {
    /// The call or its stream failed; `message` is cut to [`MESSAGE_MAX_CHARS`], `kind` is whole.
    Call { kind: LlmErrorKind, message: String },
    /// The case ran past the run's per-case deadline and was abandoned there.
    DeadlinePassed { deadline_ms: u64 },
    /// An older version recorded the failure as text alone, so its kind is not known.
    Unclassified { message: String },
}

/// The most chars of an error message a report keeps.
pub const MESSAGE_MAX_CHARS: usize = 500;

impl CaseFailure {
    /// Records a failed call or stream by its kind and its message cut to
    /// [`MESSAGE_MAX_CHARS`].
    pub(crate) fn call(kind: LlmErrorKind, message: &str) -> Self {
        Self::Call {
            kind,
            message: crate::truncate(message, MESSAGE_MAX_CHARS),
        }
    }
}

impl std::fmt::Display for CaseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call { kind, message } => write!(f, "{}: {message}", kind.as_str()),
            Self::DeadlinePassed { deadline_ms } => {
                write!(f, "the case ran past its {deadline_ms} ms deadline")
            }
            Self::Unclassified { message } => f.write_str(message),
        }
    }
}

fn failure_or_text<'de, D: Deserializer<'de>>(d: D) -> Result<CaseFailure, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Recorded {
        Failure(CaseFailure),
        Text(String),
    }
    Ok(match Recorded::deserialize(d)? {
        Recorded::Failure(f) => f,
        Recorded::Text(message) => CaseFailure::Unclassified { message },
    })
}

/// One check's outcome for one case. A failure always carries a reason; a pass may carry a note.
///
/// On the wire `outcome` is the tag and the other fields sit beside it:
///
/// ```text
///   {"outcome": "passed", "name": "x"}
///   {"outcome": "failed", "name": "y", "reason": "bad"}
///   {"outcome": "not_judged", "name": "z", "failure": {"cause": "call", ..}}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CaseCheckResult {
    Passed {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Failed {
        name: String,
        reason: String,
    },
    /// The check's judge failed, so the case says nothing about the model on it.
    NotJudged {
        name: String,
        failure: JudgeFailure,
    },
}

impl CaseCheckResult {
    pub fn name(&self) -> &str {
        match self {
            Self::Passed { name, .. }
            | Self::Failed { name, .. }
            | Self::NotJudged { name, .. } => name,
        }
    }

    pub fn passed(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }

    /// Returns whether the case passed the check; `None` when its judge could not decide.
    pub fn decided(&self) -> Option<bool> {
        match self {
            Self::Passed { .. } => Some(true),
            Self::Failed { .. } => Some(false),
            Self::NotJudged { .. } => None,
        }
    }

    /// Returns the pass note or the failure reason.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Passed { note, .. } => note.as_deref(),
            Self::Failed { reason, .. } => Some(reason.as_str()),
            Self::NotJudged { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: &str, checks: Vec<CaseCheckResult>) -> CaseReport {
        CaseReport {
            case_id: id.parse().unwrap(),
            wall_clock_ms: 0,
            usage: Usage::default(),
            output_chars: 0,
            tool_trajectory: Vec::new(),
            outcome: CaseOutcome::Ok { checks },
        }
    }

    fn passed(name: &str) -> CaseCheckResult {
        CaseCheckResult::Passed {
            name: name.into(),
            note: None,
        }
    }

    fn failed(name: &str) -> CaseCheckResult {
        CaseCheckResult::Failed {
            name: name.into(),
            reason: "r".into(),
        }
    }

    #[test]
    fn a_case_id_is_one_directory_name() {
        for ok in ["c", "case_001", "a.b", "..x"] {
            assert!(ok.parse::<CaseId>().is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "../outside", "a/b", "/etc", "a/"] {
            assert!(bad.parse::<CaseId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_manifest_naming_a_case_outside_its_run_does_not_read() {
        let err = serde_json::from_value::<CaseReport>(serde_json::json!({
            "case_id": "../outside", "status": "ok", "wall_clock_ms": 1,
            "usage": Usage::default(), "output_chars": 0
        }))
        .unwrap_err();
        assert!(err.to_string().contains("../outside"), "{err}");
    }

    #[test]
    fn a_check_that_decided_no_case_has_no_violation_rate() {
        let summary = summarize("x", &[case("c", Vec::new())]);
        assert_eq!(summary.evaluated, 0);
        assert_eq!(summary.violation_rate(), None);
    }

    #[test]
    fn a_check_that_decided_one_case_has_a_rate() {
        assert_eq!(
            summarize("x", &[case("c", vec![failed("x")])]).violation_rate(),
            Some(1.0)
        );
        assert_eq!(
            summarize("x", &[case("c", vec![passed("x")])]).violation_rate(),
            Some(0.0)
        );
    }

    #[test]
    fn a_case_its_judge_could_not_decide_is_counted_apart_from_the_rate() {
        let not_judged = CaseCheckResult::NotJudged {
            name: "x".into(),
            failure: JudgeFailure::StreamTooLong { limit: 1 },
        };
        let summary = summarize(
            "x",
            &[case("a", vec![not_judged]), case("b", vec![passed("x")])],
        );
        assert_eq!(
            (summary.evaluated, summary.violations, summary.not_judged),
            (1, 0, 1)
        );
        assert_eq!(summary.violation_rate(), Some(0.0));
    }

    #[test]
    fn a_case_report_an_older_version_wrote_reads_with_its_outcome() {
        let ok: CaseReport = serde_json::from_value(serde_json::json!({
            "case_id": "c", "status": "ok", "wall_clock_ms": 1, "usage": Usage::default(),
            "output_chars": 2, "error": null,
            "checks": [{"outcome": "passed", "name": "non-empty"}], "tool_trajectory": ["t"]
        }))
        .unwrap();
        assert_eq!(ok.checks(), [passed("non-empty")]);
        assert_eq!(ok.tool_trajectory, ["t"]);

        let failed: CaseReport = serde_json::from_value(serde_json::json!({
            "case_id": "d", "status": "error", "wall_clock_ms": 1, "usage": Usage::default(),
            "output_chars": 0, "error": "connection reset", "checks": [], "tool_trajectory": []
        }))
        .unwrap();
        assert_eq!(
            failed.outcome,
            CaseOutcome::Error {
                failure: CaseFailure::Unclassified {
                    message: "connection reset".into()
                }
            }
        );
    }

    #[test]
    fn a_failed_case_round_trips_with_its_kind() {
        let report = CaseReport {
            case_id: "d".parse().unwrap(),
            wall_clock_ms: 1,
            usage: Usage::default(),
            output_chars: 0,
            tool_trajectory: Vec::new(),
            outcome: CaseOutcome::Error {
                failure: CaseFailure::Call {
                    kind: LlmErrorKind::RateLimited,
                    message: "slow down".into(),
                },
            },
        };
        let wire = serde_json::to_value(&report).unwrap();
        assert_eq!(wire["status"], "error");
        assert_eq!(wire["failure"]["kind"], "rate_limited");
        let back: CaseReport = serde_json::from_value(wire).unwrap();
        assert_eq!(back.outcome, report.outcome);
    }

    #[test]
    fn a_manifest_an_older_version_wrote_keeps_its_check_names_and_recounts_them() {
        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "corpus_path": "c", "provider_id": "p", "model": "m",
            "started_at_ms": 0, "ended_at_ms": 0,
            "case_count": 9, "success_count": 9, "error_count": 0, "total_usage": Usage::default(),
            "checks": [{"name": "non-empty", "evaluated": 9, "violations": 9, "violation_rate": 1.0}],
            "cases": [{
                "case_id": "c", "status": "ok", "wall_clock_ms": 1, "usage": Usage::default(),
                "output_chars": 2, "error": null, "checks": [{"outcome": "passed", "name": "non-empty"}]
            }]
        }))
        .unwrap();
        assert_eq!(manifest.case_count(), 1);
        assert_eq!(
            manifest.check_summaries(),
            [CheckSummary {
                name: "non-empty".into(),
                evaluated: 1,
                violations: 0,
                not_judged: 0
            }]
        );
    }
}
