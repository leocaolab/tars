//! `Judge` framework types — offline binary-classification eval.
//!
//! See `docs/eval-and-arc-llm-roadmap.md §1.2` for the design intent.
//! Per arc's production experience, the high-ROI offline eval phase
//! is: take a corpus of (input, agent_output) pairs, ask a second
//! LLM to verdict each one as TP / FP / Unsure, aggregate into a
//! precision-style report. Different shape from Doc 16 §7.1's
//! per-call deterministic dimension scoring.
//!
//! The trait + reference impl live in `tars-runtime::judge`; this
//! module only defines the data shapes so caller code can be
//! type-stable even if it never imports the runtime.

use serde::{Deserialize, Serialize};

/// One item to be judged — the input the agent saw, the output it
/// produced, optional gold-standard expectation, optional context the
/// judge needs.
///
/// `item_id` is caller-chosen and shows up in [`JudgeReport`] so each
/// verdict can be traced back to its source (e.g. arc finding
/// `"app-shell-runner-14"`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgeItem {
    pub item_id: String,
    pub input: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// Free-form context the judge needs to make its decision
    /// (e.g. arc passes the source file the finding cites).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// The judge's verdict on one item.
///
/// Deliberately binary classification — production agent eval at the
/// "is this output correct" level is a yes/no question; nuanced
/// scoring is its own (heavier) framework and lives elsewhere.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum JudgeVerdict {
    /// Output is correct / valid.
    TruePositive,
    /// Output is wrong / hallucination / over-flag.
    FalsePositive,
    /// Judge can't confidently decide. Caller decides what to do —
    /// retry with different judge, escalate to human, etc.
    Unsure { reason: String },
}

impl JudgeVerdict {
    pub fn label(&self) -> &'static str {
        match self {
            Self::TruePositive => "TP",
            Self::FalsePositive => "FP",
            Self::Unsure { .. } => "UNSURE",
        }
    }
}

/// One judge pass's full verdict log + aggregates.
///
/// `precision()` is the headline number for "was the critic right":
/// `TP / (TP + FP)`. `Unsure` is held out of the denominator — it's
/// a separate signal about judge calibration, not about the critic.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgeReport {
    /// Identifier of the judge that produced the verdicts.
    /// Conventionally `"provider:model"` (e.g. `"anthropic:claude-opus-4-7"`).
    pub judge_id: String,
    pub item_count: u32,
    pub true_positives: u32,
    pub false_positives: u32,
    pub unsure: u32,
    /// Per-item verdicts in the order they were judged.
    pub verdicts: Vec<JudgedItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgedItem {
    pub item_id: String,
    pub verdict: JudgeVerdict,
}

impl JudgeReport {
    /// True iff the tallies are internally consistent:
    /// `true_positives + false_positives + unsure == item_count`.
    ///
    /// The fields are public (serde + ergonomic test fixtures), so a
    /// caller can build a report where the counts don't add up — which
    /// would make [`unsure_rate`](Self::unsure_rate) exceed 1.0 and
    /// [`precision`](Self::precision) lie. Call this before trusting the
    /// ratios; the canonical builder in `tars-runtime::judge` upholds it.
    pub fn counts_consistent(&self) -> bool {
        self.true_positives
            .checked_add(self.false_positives)
            .and_then(|s| s.checked_add(self.unsure))
            == Some(self.item_count)
    }

    /// `TP / (TP + FP)`. `None` when no decisive verdicts.
    pub fn precision(&self) -> Option<f64> {
        // Saturating: counts are u32 and could in principle sum past
        // u32::MAX; a wrapping add in release would yield a bogus
        // denominator. Saturation keeps the ratio sane at the extreme.
        let total = self.true_positives.saturating_add(self.false_positives);
        if total == 0 {
            None
        } else {
            Some(self.true_positives as f64 / total as f64)
        }
    }

    /// Wilson score interval for precision at `confidence_level`
    /// (e.g. `0.95`). Returns `(point, lower, upper)` or `None` when
    /// there are no decisive verdicts.
    ///
    /// Why Wilson and not normal-approximation: Wilson behaves well at
    /// the edges (precision = 0 or 1, small n) and doesn't ever
    /// produce intervals outside [0, 1]. The closed-form normal CI
    /// gives nonsensical intervals like (-0.05, 0.30) for small
    /// samples — common in eval where you're judging 20 items, not
    /// 2,000.
    ///
    /// `confidence_level` must be in (0, 1); 0.95 is the common default.
    /// Reference: <https://en.wikipedia.org/wiki/Binomial_proportion_confidence_interval#Wilson_score_interval>
    pub fn precision_with_ci(&self, confidence_level: f64) -> Option<(f64, f64, f64)> {
        // Add as f64 (after widening) so the denominator can't wrap in
        // release the way a u32 `tp + fp` would. Same fix as `precision`.
        let n = self.true_positives as f64 + self.false_positives as f64;
        if n == 0.0 {
            return None;
        }
        let p = self.true_positives as f64 / n;
        let z = z_for_confidence(confidence_level)?;
        wilson_interval(p, n, z).map(|(lo, hi)| (p, lo, hi))
    }

    /// `Unsure / item_count`. Headline for judge calibration: a high
    /// share of Unsure means the judge prompt is too weak, the items
    /// are genuinely ambiguous, or both.
    pub fn unsure_rate(&self) -> Option<f64> {
        if self.item_count == 0 {
            None
        } else {
            Some(self.unsure as f64 / self.item_count as f64)
        }
    }
}

/// Inverse standard normal at the right tail for common confidence
/// levels. Returns `None` for unsupported levels — keeps the API
/// honest about what's tabulated (no fake "any-level" guarantees that
/// would require a real erf-inverse implementation).
///
/// The tabulated values are the standard z-scores everyone reaches
/// for: 90/95/99/99.9%. Add more if a caller needs them; this list
/// covers the realistic range for eval reports.
fn z_for_confidence(level: f64) -> Option<f64> {
    // Equality comparisons on f64 are intentional — these are the
    // documented exact values callers should pass in.
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

/// Result of a McNemar paired-significance test comparing two judge
/// runs over the **same** items. See `docs/eval-methodology.md §2`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct McNemarResult {
    /// Items baseline got right but candidate got wrong (regressions).
    pub b: u32,
    /// Items baseline got wrong but candidate got right (improvements).
    pub c: u32,
    /// χ² = (b−c)² / (b+c). `None` when b+c == 0 (no discordant pairs —
    /// the two runs agree on every item, nothing to test).
    pub chi_squared: Option<f64>,
    /// True iff χ² exceeds the 1-dof critical value at α=0.05 (3.841).
    pub significant_at_05: bool,
    /// ... at α=0.01 (6.635).
    pub significant_at_01: bool,
}

/// McNemar's test on two sets of per-item correctness, paired by item
/// id. `correct` maps item_id → was-correct (TruePositive). Only items
/// present in **both** maps are paired; the rest are ignored (you
/// can't compare an item one run didn't judge).
///
/// This is the statistically correct test for "did config B change
/// behavior vs A on the same corpus" — the data is paired, so an
/// unpaired two-proportion test would be wrong (see methodology doc).
/// Significance via 1-dof critical-value lookup (no erfc needed):
/// 3.841 @ 0.05, 6.635 @ 0.01.
pub fn mcnemar(
    baseline: &std::collections::BTreeMap<String, bool>,
    candidate: &std::collections::BTreeMap<String, bool>,
) -> McNemarResult {
    let mut b = 0u32; // base right, cand wrong
    let mut c = 0u32; // base wrong, cand right
    for (id, &base_correct) in baseline {
        if let Some(&cand_correct) = candidate.get(id) {
            match (base_correct, cand_correct) {
                // Saturating: corpora are nowhere near u32::MAX, but a
                // wrapping `+= 1` in release would silently corrupt the
                // χ² statistic at the extreme.
                (true, false) => b = b.saturating_add(1),
                (false, true) => c = c.saturating_add(1),
                _ => {} // concordant — carries no information
            }
        }
    }
    let n = b.saturating_add(c);
    let chi_squared = if n == 0 {
        None
    } else {
        let diff = b as f64 - c as f64;
        Some(diff * diff / n as f64)
    };
    McNemarResult {
        b,
        c,
        chi_squared,
        significant_at_05: chi_squared.is_some_and(|x| x > 3.841),
        significant_at_01: chi_squared.is_some_and(|x| x > 6.635),
    }
}

/// Wilson score interval — closed-form binomial confidence interval.
/// `p` is the observed proportion (TP / n); `n` is the trial count;
/// `z` is the standard normal quantile for the desired level.
/// Returns `(lower, upper)`, both clamped to `[0, 1]`.
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
            JudgeVerdict::TruePositive,
            JudgeVerdict::FalsePositive,
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
    fn verdict_label_stable() {
        assert_eq!(JudgeVerdict::TruePositive.label(), "TP");
        assert_eq!(JudgeVerdict::FalsePositive.label(), "FP");
        assert_eq!(
            JudgeVerdict::Unsure { reason: "x".into() }.label(),
            "UNSURE",
        );
    }

    #[test]
    fn precision_excludes_unsure() {
        let report = JudgeReport {
            judge_id: "anthropic:claude-opus-4-7".into(),
            item_count: 10,
            true_positives: 7,
            false_positives: 1,
            unsure: 2,
            verdicts: vec![
                verdict("a", JudgeVerdict::TruePositive),
                verdict("b", JudgeVerdict::FalsePositive),
                verdict("c", JudgeVerdict::Unsure { reason: "?".into() }),
            ],
        };
        // 7 / (7 + 1) = 0.875; Unsure NOT in denominator.
        assert!((report.precision().unwrap() - 0.875).abs() < 1e-9);
        // 2 / 10 = 0.2.
        assert!((report.unsure_rate().unwrap() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn precision_none_when_all_unsure() {
        let report = JudgeReport {
            judge_id: "x".into(),
            item_count: 3,
            true_positives: 0,
            false_positives: 0,
            unsure: 3,
            verdicts: vec![],
        };
        assert!(report.precision().is_none());
    }

    fn report_with(tp: u32, fp: u32) -> JudgeReport {
        JudgeReport {
            judge_id: "x".into(),
            item_count: tp + fp,
            true_positives: tp,
            false_positives: fp,
            unsure: 0,
            verdicts: vec![],
        }
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
        // χ² = (1−2)²/(1+2) = 1/3 ≈ 0.333
        assert!((r.chi_squared.unwrap() - 1.0 / 3.0).abs() < 1e-9);
        assert!(!r.significant_at_05); // tiny n, not significant
    }

    #[test]
    fn mcnemar_significant_when_lopsided() {
        // 12 improvements, 1 regression → χ² = (1−12)²/13 = 121/13 ≈ 9.3 > 6.635
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
        assert!(r.significant_at_05);
        assert!(r.significant_at_01);
    }

    #[test]
    fn mcnemar_no_discordant_pairs_is_none() {
        let base = correctness(&[("a", true), ("b", false)]);
        let cand = correctness(&[("a", true), ("b", false)]);
        let r = mcnemar(&base, &cand);
        assert_eq!((r.b, r.c), (0, 0));
        assert!(r.chi_squared.is_none());
        assert!(!r.significant_at_05);
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
        let r = JudgeReport {
            judge_id: "x".into(),
            item_count: 3,
            true_positives: 0,
            false_positives: 0,
            unsure: 3,
            verdicts: vec![],
        };
        assert!(r.precision_with_ci(0.95).is_none());
    }
}
