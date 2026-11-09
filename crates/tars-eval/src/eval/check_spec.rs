//! Parses the `--check` syntax into [`CheckSpec`] once, at the edge, and owns each check's name:
//! the spec's canonical writing, so two checks of one meaning have one name and two of different
//! meanings (`max-length:10`, `max-length:1000`) have two. [`CheckSet`] is the checks of one run,
//! which pairs an `args-judge` check with its judge.

use std::fmt;
use std::num::{ParseFloatError, ParseIntError};
use std::str::FromStr;

use tars_types::ProviderId;

use crate::trajectory_match::MatchMode;

/// A built-in check `tars eval run` can run on every case.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckSpec {
    NonEmpty,
    ValidJson,
    MaxLength { max_chars: usize },
    Trajectory(TrajectoryCheck),
}

/// A `trajectory-match` check: how the tools a case's model selected are scored against the
/// case's `expected_tools.json`, and the score a case must reach to pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrajectoryCheck {
    pub scoring: TrajectoryScoring,
    pub threshold: Threshold,
}

/// How a `trajectory-match` check scores a case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrajectoryScoring {
    /// Scored by [`crate::trajectory_match::score`] in this mode.
    Match(MatchMode),
    /// Like [`MatchMode::Args`], with byte-different arguments asked of a judge model.
    ArgsJudged,
}

impl TrajectoryScoring {
    /// Returns the mode word the spec spells.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Match(mode) => mode.as_str(),
            Self::ArgsJudged => ARGS_JUDGE,
        }
    }
}

const ARGS_JUDGE: &str = "args-judge";
const TRAJECTORY_MATCH: &str = "trajectory-match";

/// The score in `[0, 1]` a case must reach to pass a trajectory check. A score lies in that
/// range, so a threshold outside it (or NaN) would pass every case or none.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Threshold(f64);

impl Threshold {
    pub const FULL: Self = Self(1.0);

    /// Returns the threshold `value`, or `None` outside `[0, 1]`.
    pub fn new(value: f64) -> Option<Self> {
        (0.0..=1.0).contains(&value).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// A check spec that names no check or whose argument does not parse.
#[derive(Debug, thiserror::Error)]
pub enum ParseCheckError {
    #[error(
        "unknown check `{spec}`. Recognized: non-empty, valid-json, max-length:<N>, \
         trajectory-match[:<{}>[:<threshold>]]",
        trajectory_modes()
    )]
    Unknown { spec: String },
    #[error("bad max-length spec: {spec}")]
    MaxLength {
        spec: String,
        #[source]
        source: ParseIntError,
    },
    #[error(
        "unknown trajectory-match mode `{mode}`. Recognized: {}",
        trajectory_modes()
    )]
    TrajectoryMode { mode: String },
    #[error("bad trajectory-match threshold: {threshold}")]
    Threshold {
        threshold: String,
        #[source]
        source: ParseFloatError,
    },
    #[error("trajectory-match threshold must be in [0, 1], got {threshold}")]
    ThresholdOutOfRange { threshold: String },
}

fn trajectory_modes() -> String {
    MatchMode::ALL
        .iter()
        .map(MatchMode::as_str)
        .chain([ARGS_JUDGE])
        .collect::<Vec<_>>()
        .join("|")
}

impl FromStr for CheckSpec {
    type Err = ParseCheckError;

    /// Parses `non-empty`, `valid-json`, `max-length:<N>`, `trajectory-match`,
    /// `trajectory-match:<mode>` or `trajectory-match:<mode>:<threshold>`; a bare
    /// `trajectory-match` is `ordered` at threshold 1.
    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let unknown = || ParseCheckError::Unknown {
            spec: spec.to_string(),
        };
        let parts: Vec<&str> = spec.split(':').collect();
        match parts.as_slice() {
            ["non-empty"] => Ok(Self::NonEmpty),
            ["valid-json"] => Ok(Self::ValidJson),
            ["max-length", n] => n
                .parse()
                .map(|max_chars| Self::MaxLength { max_chars })
                .map_err(|source| ParseCheckError::MaxLength {
                    spec: spec.to_string(),
                    source,
                }),
            [TRAJECTORY_MATCH] => Ok(Self::Trajectory(TrajectoryCheck {
                scoring: TrajectoryScoring::Match(MatchMode::Ordered),
                threshold: Threshold::FULL,
            })),
            [TRAJECTORY_MATCH, mode] => Ok(Self::Trajectory(TrajectoryCheck {
                scoring: parse_scoring(mode)?,
                threshold: Threshold::FULL,
            })),
            [TRAJECTORY_MATCH, mode, threshold] => Ok(Self::Trajectory(TrajectoryCheck {
                scoring: parse_scoring(mode)?,
                threshold: parse_threshold(threshold)?,
            })),
            _ => Err(unknown()),
        }
    }
}

fn parse_scoring(mode: &str) -> Result<TrajectoryScoring, ParseCheckError> {
    if mode == ARGS_JUDGE {
        return Ok(TrajectoryScoring::ArgsJudged);
    }
    MatchMode::parse(mode)
        .map(TrajectoryScoring::Match)
        .ok_or_else(|| ParseCheckError::TrajectoryMode {
            mode: mode.to_string(),
        })
}

fn parse_threshold(text: &str) -> Result<Threshold, ParseCheckError> {
    let value = text
        .parse::<f64>()
        .map_err(|source| ParseCheckError::Threshold {
            threshold: text.to_string(),
            source,
        })?;
    Threshold::new(value).ok_or_else(|| ParseCheckError::ThresholdOutOfRange {
        threshold: text.to_string(),
    })
}

/// Writes the spec canonically, which is the check's name in a run's reports: `trajectory-match`
/// alone for `ordered` at threshold 1, a threshold only when it is below 1.
impl fmt::Display for CheckSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonEmpty => f.write_str("non-empty"),
            Self::ValidJson => f.write_str("valid-json"),
            Self::MaxLength { max_chars } => write!(f, "max-length:{max_chars}"),
            Self::Trajectory(TrajectoryCheck { scoring, threshold }) => {
                f.write_str(TRAJECTORY_MATCH)?;
                let ordered = *scoring == TrajectoryScoring::Match(MatchMode::Ordered);
                if *threshold == Threshold::FULL {
                    if !ordered {
                        write!(f, ":{}", scoring.as_str())?;
                    }
                    Ok(())
                } else {
                    write!(f, ":{}:{}", scoring.as_str(), threshold.get())
                }
            }
        }
    }
}

/// The judge an `args-judge` check asks: a provider, and the model to bind it to (`None` for the
/// provider's default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgeChoice {
    pub provider: ProviderId,
    pub model: Option<String>,
}

/// The checks of one run, each name once, with the judge exactly when an `args-judge` check is
/// among them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CheckSet {
    checks: Vec<CheckSpec>,
    args_judge: Option<JudgeChoice>,
}

/// Why a run's checks do not make a [`CheckSet`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckSetError {
    #[error("check `{name}` is given twice")]
    Duplicate { name: String },
    #[error("the `trajectory-match:args-judge` check needs a judge provider")]
    JudgeMissing,
    #[error("a judge is given, but no check is `trajectory-match:args-judge`")]
    JudgeUnused,
}

impl CheckSet {
    /// Returns the checks of a run, in the order given.
    ///
    /// # Errors
    ///
    /// Fails on two specs of one canonical name, on an `args-judge` check without `args_judge`,
    /// and on `args_judge` without an `args-judge` check.
    pub fn new(
        checks: Vec<CheckSpec>,
        args_judge: Option<JudgeChoice>,
    ) -> Result<Self, CheckSetError> {
        let names: Vec<String> = checks.iter().map(CheckSpec::to_string).collect();
        if let Some(name) = first_repeat(&names) {
            return Err(CheckSetError::Duplicate {
                name: name.to_string(),
            });
        }
        let judged = checks.iter().any(|c| {
            matches!(
                c,
                CheckSpec::Trajectory(TrajectoryCheck {
                    scoring: TrajectoryScoring::ArgsJudged,
                    ..
                })
            )
        });
        match (judged, args_judge.is_some()) {
            (true, false) => Err(CheckSetError::JudgeMissing),
            (false, true) => Err(CheckSetError::JudgeUnused),
            _ => Ok(Self { checks, args_judge }),
        }
    }

    pub fn checks(&self) -> &[CheckSpec] {
        &self.checks
    }

    /// Returns the judge of the `args-judge` check; `Some` exactly when there is one.
    pub fn args_judge(&self) -> Option<&JudgeChoice> {
        self.args_judge.as_ref()
    }
}

fn first_repeat(names: &[String]) -> Option<&str> {
    names
        .iter()
        .enumerate()
        .find(|(i, name)| names[..*i].contains(name))
        .map(|(_, name)| name.as_str())
}

/// Returns whether a check name from a run's reports names a trajectory check; a name no spec
/// parses (one an older version wrote) names none.
pub(crate) fn names_trajectory_check(name: &str) -> bool {
    matches!(name.parse(), Ok(CheckSpec::Trajectory(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> CheckSpec {
        s.parse().unwrap()
    }

    #[test]
    fn each_spec_is_named_by_its_canonical_writing() {
        for (written, name) in [
            ("non-empty", "non-empty"),
            ("valid-json", "valid-json"),
            ("max-length:10", "max-length:10"),
            ("trajectory-match", "trajectory-match"),
            ("trajectory-match:ordered", "trajectory-match"),
            ("trajectory-match:ordered:1", "trajectory-match"),
            ("trajectory-match:exact", "trajectory-match:exact"),
            ("trajectory-match:set:0.5", "trajectory-match:set:0.5"),
            ("trajectory-match:args-judge", "trajectory-match:args-judge"),
        ] {
            assert_eq!(spec(written).to_string(), name, "{written}");
            assert_eq!(spec(name), spec(written), "{written}");
        }
    }

    #[test]
    fn two_length_limits_are_two_checks() {
        assert_ne!(
            spec("max-length:10").to_string(),
            spec("max-length:1000").to_string()
        );
    }

    #[test]
    fn a_spec_missing_its_separator_or_mode_is_refused() {
        for bad in [
            "trajectory-matchexact",
            "trajectory-match:",
            "trajectory-match::",
            "trajectory-match:exact:0.5:x",
            "nonempty",
            "max-length",
            "max-length:",
            "max-length:-1",
            "",
        ] {
            assert!(bad.parse::<CheckSpec>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_threshold_is_kept_at_both_ends_of_the_score_range_and_refused_past_them() {
        assert!(Threshold::new(0.0).is_some());
        assert!(Threshold::new(1.0).is_some());
        assert!(Threshold::new(-0.0001).is_none());
        assert!(Threshold::new(1.0001).is_none());
        assert!(Threshold::new(f64::NAN).is_none());
    }

    fn judge() -> JudgeChoice {
        JudgeChoice {
            provider: ProviderId::new("judge"),
            model: None,
        }
    }

    #[test]
    fn a_check_given_twice_is_refused_whatever_its_spelling() {
        let err = CheckSet::new(
            vec![spec("trajectory-match"), spec("trajectory-match:ordered")],
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            CheckSetError::Duplicate {
                name: "trajectory-match".into()
            }
        );
    }

    #[test]
    fn an_args_judge_check_comes_with_its_judge_and_only_then() {
        let judged = spec("trajectory-match:args-judge");
        assert_eq!(
            CheckSet::new(vec![judged.clone()], None),
            Err(CheckSetError::JudgeMissing)
        );
        assert_eq!(
            CheckSet::new(vec![spec("non-empty")], Some(judge())),
            Err(CheckSetError::JudgeUnused)
        );
        let set = CheckSet::new(vec![judged], Some(judge())).unwrap();
        assert_eq!(set.args_judge(), Some(&judge()));
    }

    #[test]
    fn only_a_trajectory_spec_names_a_trajectory_check() {
        assert!(names_trajectory_check("trajectory-match:exact"));
        assert!(!names_trajectory_check("non-empty"));
        assert!(!names_trajectory_check("trajectory-match-like"));
        assert!(!names_trajectory_check("not_empty"));
    }
}
