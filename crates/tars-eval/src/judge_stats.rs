//! Holds what a judge reads and writes, an item and its verdict, and the statistics over a
//! judged run: precision with a Wilson interval, and McNemar's test between two runs. The judge
//! itself is in [`crate::judge`].

use serde::{Deserialize, Serialize};
use tars_types::LlmErrorKind;

/// One output to judge. `item_id` is the caller's and comes back in [`JudgeReport`], so each
/// verdict traces back to its source.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgeItem {
    pub item_id: String,
    pub input: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// Free-form context the judge needs, such as the system prompt the output answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// A judge's verdict on one output: correct, wrong, or undecided. Two decisive verdicts on
/// purpose, not a graded score; `Unsure` is the judge declining to decide. A report an older
/// version wrote names them `true_positive` and `false_positive`, which still read.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum JudgeVerdict {
    #[serde(alias = "true_positive")]
    Correct,
    #[serde(alias = "false_positive")]
    Wrong,
    /// The judge could not decide; what to do next is the caller's call.
    Unsure { reason: String },
}

impl JudgeVerdict {
    /// Returns whether the verdict found the output correct; `None` for `Unsure`, which decides
    /// nothing.
    pub fn decided(&self) -> Option<bool> {
        match self {
            Self::Correct => Some(true),
            Self::Wrong => Some(false),
            Self::Unsure { .. } => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Correct => "TP",
            Self::Wrong => "FP",
            Self::Unsure { .. } => "UNSURE",
        }
    }
}

/// The verdicts of one judge pass, and the items the judge failed on. Counts are computed from
/// them. [`Self::precision`] holds `Unsure` out of its denominator: an unsure verdict says
/// something about the judge, not the output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgeReport {
    pub judge_id: String,
    pub verdicts: Vec<JudgedItem>,
    /// Items the judge failed on, in the order judged; none in a report an older version wrote,
    /// which stopped at the first failure.
    #[serde(default)]
    pub unjudged: Vec<UnjudgedItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgedItem {
    pub item_id: String,
    pub verdict: JudgeVerdict,
}

/// An item the judge gave no verdict on, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnjudgedItem {
    pub item_id: String,
    pub failure: JudgeFailure,
}

/// Why a judge gave no verdict on one item, as a report records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum JudgeFailure {
    /// The judge's call or its stream failed; `message` is cut to 500 chars, `kind` is whole.
    Call { kind: LlmErrorKind, message: String },
    /// The reply's first line was not an answer; `first_line` is that line as written.
    UnrecognizedReply { first_line: String },
    /// The reply ran past `limit` stream events and was abandoned.
    StreamTooLong { limit: usize },
    /// The judge had not answered when the case's deadline passed.
    DeadlinePassed { deadline_ms: u64 },
}

impl std::fmt::Display for JudgeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call { kind, message } => write!(f, "{}: {message}", kind.as_str()),
            Self::UnrecognizedReply { first_line } => {
                write!(f, "the judge's first line is not an answer: {first_line:?}")
            }
            Self::StreamTooLong { limit } => {
                write!(f, "the judge's reply ran past {limit} stream events")
            }
            Self::DeadlinePassed { deadline_ms } => {
                write!(
                    f,
                    "the judge had not answered at the {deadline_ms} ms deadline"
                )
            }
        }
    }
}

impl JudgeReport {
    /// Returns how many items were given to the judge, judged or not.
    pub fn item_count(&self) -> usize {
        self.verdicts.len() + self.unjudged.len()
    }

    pub fn correct(&self) -> usize {
        self.count(|v| matches!(v, JudgeVerdict::Correct))
    }

    pub fn wrong(&self) -> usize {
        self.count(|v| matches!(v, JudgeVerdict::Wrong))
    }

    pub fn unsure(&self) -> usize {
        self.count(|v| matches!(v, JudgeVerdict::Unsure { .. }))
    }

    fn count(&self, is: impl Fn(&JudgeVerdict) -> bool) -> usize {
        self.verdicts.iter().filter(|j| is(&j.verdict)).count()
    }

    /// Returns `correct / (correct + wrong)`, or `None` when there is no decisive verdict.
    pub fn precision(&self) -> Option<f64> {
        let decisive = self.correct() + self.wrong();
        (decisive > 0).then(|| self.correct() as f64 / decisive as f64)
    }

    /// Returns precision with its Wilson score interval as `(p, lo, hi)`; `None` with no decisive
    /// verdict or an untabulated `confidence_level` (0.90, 0.95, 0.99 and 0.999 are tabulated).
    ///
    /// Wilson rather than the normal approximation: the normal interval leaves `[0, 1]` for small
    /// samples, such as (-0.05, 0.30), and eval judges 20 items, not 2 000.
    /// <https://en.wikipedia.org/wiki/Binomial_proportion_confidence_interval#Wilson_score_interval>
    pub fn precision_with_ci(&self, confidence_level: f64) -> Option<(f64, f64, f64)> {
        let n = (self.correct() + self.wrong()) as f64;
        if n == 0.0 {
            return None;
        }
        let p = self.correct() as f64 / n;
        let z = z_for_confidence(confidence_level)?;
        wilson_interval(p, n, z).map(|(lo, hi)| (p, lo, hi))
    }

    /// Returns the share of verdicts that are `Unsure`; a high share means the judge prompt is
    /// weak, the items are ambiguous, or both.
    pub fn unsure_rate(&self) -> Option<f64> {
        let judged = self.verdicts.len();
        (judged > 0).then(|| self.unsure() as f64 / judged as f64)
    }
}

/// Returns the two-sided z-score of a 90, 95, 99 or 99.9% level, and `None` for any other: an
/// arbitrary level would need an inverse erf.
fn z_for_confidence(level: f64) -> Option<f64> {
    if (level - 0.90).abs() < 1e-9 {
        Some(1.6449)
    } else if (level - 0.95).abs() < 1e-9 {
        Some(1.9600)
    } else if (level - 0.99).abs() < 1e-9 {
        Some(2.5758)
    } else if (level - 0.999).abs() < 1e-9 {
        Some(3.2905)
    } else {
        None
    }
}

/// The discordant pairs between two runs judged on the same items, which McNemar's test
/// (`docs/eval-methodology.md` §2) is computed from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiscordantPairs {
    /// Items baseline got right but candidate got wrong (regressions).
    pub b: u32,
    /// Items baseline got wrong but candidate got right (improvements).
    pub c: u32,
}

impl DiscordantPairs {
    /// Returns the test over `b` and `c`; `None` when b+c == 0, the runs agreeing on every item.
    pub fn test(&self) -> Option<McNemarTest> {
        mcnemar_test(self.b, self.c)
    }
}

/// The two-sided test McNemar's statistic was judged by, with its p-value.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum McNemarTest {
    /// Used below 25 discordant pairs, where the χ² approximation calls
    /// noise significant: b=4 c=0 gives χ²=4.0 (p≈0.046) but an exact p of 0.125.
    ExactBinomial { p_value: f64 },
    /// χ² = (b−c)² / (b+c) with 1 degree of freedom, without continuity correction.
    ChiSquared { statistic: f64, p_value: f64 },
}

impl McNemarTest {
    pub fn p_value(&self) -> f64 {
        match *self {
            Self::ExactBinomial { p_value } | Self::ChiSquared { p_value, .. } => p_value,
        }
    }

    /// Returns whether the test rejects "no systematic change" at significance level `alpha`.
    pub fn significant_at(&self, alpha: f64) -> bool {
        self.p_value() < alpha
    }
}

/// The discordant-pair count from which McNemar's χ² approximation is used instead of the exact
/// binomial (`docs/eval-methodology.md` §2).
const EXACT_BINOMIAL_BELOW: u64 = 25;

/// Returns the two-sided McNemar test of `b` regressions against `c` improvements, exact below
/// [`EXACT_BINOMIAL_BELOW`] discordant pairs; `None` when there are none.
fn mcnemar_test(b: u32, c: u32) -> Option<McNemarTest> {
    let n = u64::from(b) + u64::from(c);
    if n == 0 {
        None
    } else if n < EXACT_BINOMIAL_BELOW {
        Some(McNemarTest::ExactBinomial {
            p_value: exact_binomial_two_sided(u64::from(b.min(c)), n),
        })
    } else {
        let diff = f64::from(b) - f64::from(c);
        let statistic = diff * diff / n as f64;
        Some(McNemarTest::ChiSquared {
            statistic,
            p_value: erfc((statistic / 2.0).sqrt()),
        })
    }
}

/// Returns 2·P(X ≤ k) for X ~ Binomial(n, ½), capped at 1. Exact in `u64` for the `n` it is
/// called with (below [`EXACT_BINOMIAL_BELOW`], so 2ⁿ and every C(n, i) fit).
fn exact_binomial_two_sided(k: u64, n: u64) -> f64 {
    let mut coefficient: u64 = 1;
    let mut tail: u64 = 0;
    for i in 0..=k {
        tail += coefficient;
        coefficient = coefficient * (n - i) / (i + 1);
    }
    let total = 1u64 << n;
    (2.0 * tail as f64 / total as f64).min(1.0)
}

/// Returns the complementary error function, with fractional error below 1.2e-7 (Numerical
/// Recipes' `erfcc` Chebyshev fit); the χ² p-value with 1 degree of freedom is `erfc(√(χ²/2))`.
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96
                + t * (0.096_784_18
                    + t * (-0.186_288_06
                        + t * (0.278_868_07
                            + t * (-1.135_203_98
                                + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    let r = t * poly.exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// Counts the discordant pairs between two runs, pairing only items present in both maps.
///
/// Paired data needs a paired test; an unpaired two-proportion test would be wrong here.
pub fn mcnemar(
    baseline: &std::collections::BTreeMap<String, bool>,
    candidate: &std::collections::BTreeMap<String, bool>,
) -> DiscordantPairs {
    let mut b = 0u32;
    let mut c = 0u32;
    for (id, &base_correct) in baseline {
        if let Some(&cand_correct) = candidate.get(id) {
            match (base_correct, cand_correct) {
                (true, false) => b = b.saturating_add(1),
                (false, true) => c = c.saturating_add(1),
                _ => {}
            }
        }
    }
    DiscordantPairs { b, c }
}

fn wilson_interval(p: f64, n: f64, z: f64) -> Option<(f64, f64)> {
    if n <= 0.0 {
        return None;
    }
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = (z * ((p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt())) / denom;
    let lower = (center - margin).clamp(0.0, 1.0);
    let upper = (center + margin).clamp(0.0, 1.0);
    Some((lower, upper))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(item_id: &str, v: JudgeVerdict) -> JudgedItem {
        JudgedItem {
            item_id: item_id.into(),
            verdict: v,
        }
    }

    #[test]
    fn verdict_serde_round_trip() {
        for v in [
            JudgeVerdict::Correct,
            JudgeVerdict::Wrong,
            JudgeVerdict::Unsure {
                reason: "ambiguous".into(),
            },
        ] {
            let s = serde_json::to_value(&v).unwrap();
            let back: JudgeVerdict = serde_json::from_value(s).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn a_verdict_an_older_version_named_by_its_old_tag_reads() {
        for (old, now) in [
            ("true_positive", JudgeVerdict::Correct),
            ("false_positive", JudgeVerdict::Wrong),
        ] {
            let read: JudgeVerdict =
                serde_json::from_value(serde_json::json!({ "verdict": old })).unwrap();
            assert_eq!(read, now);
        }
        assert_eq!(
            serde_json::to_value(JudgeVerdict::Correct).unwrap(),
            serde_json::json!({"verdict": "correct"})
        );
    }

    #[test]
    fn a_report_an_older_version_wrote_reads_its_verdicts_and_recounts_them() {
        let report: JudgeReport = serde_json::from_value(serde_json::json!({
            "judge_id": "j", "item_count": 9, "true_positives": 9, "false_positives": 0, "unsure": 0,
            "verdicts": [
                {"item_id": "a", "verdict": {"verdict": "true_positive"}},
                {"item_id": "b", "verdict": {"verdict": "false_positive"}}
            ]
        }))
        .unwrap();
        assert_eq!(
            (report.item_count(), report.correct(), report.wrong()),
            (2, 1, 1)
        );
        assert!(report.unjudged.is_empty());
    }

    #[test]
    fn verdict_label_stable() {
        assert_eq!(JudgeVerdict::Correct.label(), "TP");
        assert_eq!(JudgeVerdict::Wrong.label(), "FP");
        assert_eq!(
            JudgeVerdict::Unsure { reason: "x".into() }.label(),
            "UNSURE",
        );
    }

    /// Builds a report of `correct`, `wrong` and `unsure` verdicts.
    fn report_of(correct: usize, wrong: usize, unsure: usize) -> JudgeReport {
        let verdicts = std::iter::repeat_n(JudgeVerdict::Correct, correct)
            .chain(std::iter::repeat_n(JudgeVerdict::Wrong, wrong))
            .chain(std::iter::repeat_n(
                JudgeVerdict::Unsure { reason: "?".into() },
                unsure,
            ))
            .enumerate()
            .map(|(i, v)| verdict(&format!("i{i}"), v))
            .collect();
        JudgeReport {
            judge_id: "x".into(),
            verdicts,
            unjudged: Vec::new(),
        }
    }

    #[test]
    fn precision_excludes_unsure() {
        let report = report_of(7, 1, 2);
        // 7 / (7 + 1) = 0.875; Unsure NOT in denominator.
        assert!((report.precision().unwrap() - 0.875).abs() < 1e-9);
        // 2 / 10 = 0.2.
        assert!((report.unsure_rate().unwrap() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn precision_none_when_all_unsure() {
        let report = report_of(0, 0, 3);
        assert!(report.precision().is_none());
    }

    fn report_with(tp: usize, fp: usize) -> JudgeReport {
        report_of(tp, fp, 0)
    }

    #[test]
    fn wilson_ci_matches_closed_form_30_of_50() {
        // 30 successes in 50 trials, 95% (z = 1.96):
        //   z² = 3.8416
        //   denom  = 1 + z²/50 = 1.07683
        //   center = (0.6 + z²/100) / denom = 0.5929
        //   margin = z · √(p(1-p)/n + z²/(4n²)) / denom ≈ 0.1311
        //   → (0.4618, 0.7239)
        // Verified against R `prop.test(30, 50, correct=FALSE)`.
        let r = report_with(30, 20);
        let (p, lo, hi) = r.precision_with_ci(0.95).unwrap();
        assert!((p - 0.6).abs() < 1e-9);
        assert!((lo - 0.4618).abs() < 1e-3, "lower bound off: {lo}");
        assert!((hi - 0.7239).abs() < 1e-3, "upper bound off: {hi}");
    }

    #[test]
    fn wilson_ci_edge_cases_clamp_to_unit() {
        // Perfect precision: TP=10, FP=0. Wilson interval should NOT
        // give upper > 1 even though the closed form's center can.
        let r = report_with(10, 0);
        let (p, lo, hi) = r.precision_with_ci(0.95).unwrap();
        assert!((p - 1.0).abs() < 1e-9);
        assert!(hi <= 1.0 && hi >= lo, "hi={hi} lo={lo}");
        assert!(
            lo > 0.5,
            "tight upper-bound case should still have lo > 0.5, got {lo}"
        );

        // Zero precision: TP=0, FP=10.
        let r = report_with(0, 10);
        let (p, lo, hi) = r.precision_with_ci(0.95).unwrap();
        assert_eq!(p, 0.0);
        assert!(lo >= 0.0 && lo <= hi);
    }

    #[test]
    fn wilson_ci_widens_for_smaller_n() {
        // Same proportion (50%), smaller sample → wider interval.
        let (_, lo_big, hi_big) = report_with(50, 50).precision_with_ci(0.95).unwrap();
        let (_, lo_small, hi_small) = report_with(5, 5).precision_with_ci(0.95).unwrap();
        let width_big = hi_big - lo_big;
        let width_small = hi_small - lo_small;
        assert!(
            width_small > width_big,
            "smaller-n CI should be wider: small={width_small}, big={width_big}"
        );
    }

    #[test]
    fn wilson_ci_widens_for_higher_confidence() {
        // Same data, 99% should be wider than 95%.
        let r = report_with(30, 20);
        let (_, lo95, hi95) = r.precision_with_ci(0.95).unwrap();
        let (_, lo99, hi99) = r.precision_with_ci(0.99).unwrap();
        assert!(lo99 < lo95 && hi99 > hi95);
    }

    #[test]
    fn wilson_ci_returns_none_on_unsupported_level() {
        // We tabulate 0.90 / 0.95 / 0.99 / 0.999 — anything else
        // returns None so callers don't get a silently wrong z.
        assert!(report_with(10, 5).precision_with_ci(0.80).is_none());
        assert!(report_with(10, 5).precision_with_ci(0.50).is_none());
    }

    fn correctness(pairs: &[(&str, bool)]) -> std::collections::BTreeMap<String, bool> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn mcnemar_counts_discordant_pairs() {
        // baseline: a✓ b✓ c✗ d✗ ; candidate: a✓ b✗ c✓ d✓
        // b (base right, cand wrong): {b} → 1
        // c (base wrong, cand right): {c, d} → 2
        let base = correctness(&[("a", true), ("b", true), ("c", false), ("d", false)]);
        let cand = correctness(&[("a", true), ("b", false), ("c", true), ("d", true)]);
        let r = mcnemar(&base, &cand);
        assert_eq!(r.b, 1);
        assert_eq!(r.c, 2);
        // b+c = 3 is exact: 2·P(X ≤ 1 | n=3) = 2·4/8, capped at 1.
        let test = r.test().unwrap();
        assert_eq!(test, McNemarTest::ExactBinomial { p_value: 1.0 });
        assert!(!test.significant_at(0.05));
    }

    #[test]
    fn mcnemar_significant_when_lopsided() {
        // 12 improvements, 1 regression → exact p = 2·(1+13)/2¹³ ≈ 0.0034 < 0.01
        let mut base = std::collections::BTreeMap::new();
        let mut cand = std::collections::BTreeMap::new();
        for i in 0..12 {
            base.insert(format!("imp{i}"), false);
            cand.insert(format!("imp{i}"), true);
        }
        base.insert("reg".into(), true);
        cand.insert("reg".into(), false);
        let r = mcnemar(&base, &cand);
        assert_eq!(r.c, 12);
        assert_eq!(r.b, 1);
        let test = r.test().unwrap();
        assert!(test.significant_at(0.05));
        assert!(test.significant_at(0.01));
    }

    #[test]
    fn mcnemar_no_discordant_pairs_is_none() {
        let base = correctness(&[("a", true), ("b", false)]);
        let cand = correctness(&[("a", true), ("b", false)]);
        let r = mcnemar(&base, &cand);
        assert_eq!((r.b, r.c), (0, 0));
        assert_eq!(r.test(), None);
    }

    #[test]
    fn mcnemar_ignores_unpaired_items() {
        // "only_base" / "only_cand" are dropped — can't pair them.
        let base = correctness(&[("shared", true), ("only_base", false)]);
        let cand = correctness(&[("shared", false), ("only_cand", true)]);
        let r = mcnemar(&base, &cand);
        // only "shared": base right, cand wrong → b=1
        assert_eq!((r.b, r.c), (1, 0));
    }

    #[test]
    fn wilson_ci_returns_none_with_no_decisive_verdicts() {
        let r = report_of(0, 0, 3);
        assert!(r.precision_with_ci(0.95).is_none());
    }

    #[test]
    fn four_regressions_and_no_improvement_is_not_significant() {
        assert_eq!(
            mcnemar_test(4, 0),
            Some(McNemarTest::ExactBinomial { p_value: 0.125 })
        );
        assert!(!mcnemar_test(4, 0).unwrap().significant_at(0.05));
    }

    #[test]
    fn twenty_four_discordant_pairs_use_the_exact_binomial() {
        let test = mcnemar_test(17, 7).unwrap();
        assert!(matches!(test, McNemarTest::ExactBinomial { .. }));
        // 2·Σ_{i≤7} C(24,i) / 2²⁴ = 2·536_155 / 16_777_216
        assert!((test.p_value() - 1_072_310.0 / 16_777_216.0).abs() < 1e-12);
    }

    #[test]
    fn twenty_five_discordant_pairs_use_chi_squared() {
        let test = mcnemar_test(18, 7).unwrap();
        let McNemarTest::ChiSquared { statistic, p_value } = test else {
            panic!("expected chi-squared, got {test:?}");
        };
        // (18−7)²/25 = 4.84; P(χ²₁ > 4.84) ≈ 0.02781
        assert!((statistic - 4.84).abs() < 1e-12);
        assert!((p_value - 0.027_807).abs() < 1e-5);
    }

    #[test]
    fn chi_squared_p_crosses_alpha_at_the_critical_value() {
        // χ² = 121/31 ≈ 3.90 sits just above the 1-dof critical value 3.841, 100/30 ≈ 3.33 below.
        assert!(mcnemar_test(21, 10).unwrap().significant_at(0.05));
        assert!(!mcnemar_test(20, 10).unwrap().significant_at(0.05));
    }

    #[test]
    fn no_discordant_pairs_has_no_test() {
        assert_eq!(mcnemar_test(0, 0), None);
    }

    #[test]
    fn exact_p_is_symmetric_in_b_and_c() {
        assert_eq!(mcnemar_test(3, 9), mcnemar_test(9, 3));
    }
}
