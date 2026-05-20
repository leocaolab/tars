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
    /// `TP / (TP + FP)`. `None` when no decisive verdicts.
    pub fn precision(&self) -> Option<f64> {
        let total = self.true_positives + self.false_positives;
        if total == 0 {
            None
        } else {
            Some(self.true_positives as f64 / total as f64)
        }
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
            JudgeVerdict::Unsure {
                reason: "x".into()
            }
            .label(),
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
                verdict(
                    "c",
                    JudgeVerdict::Unsure {
                        reason: "?".into(),
                    },
                ),
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
}
