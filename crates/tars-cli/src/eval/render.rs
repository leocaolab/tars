//! Turns the reports and progress events of `tars_eval::eval` into the lines `tars eval` shows:
//! progress and summaries for stderr, the diff for stdout as text or as one JSON object.

use serde::Serialize;
use tars_eval::McNemarTest;
use tars_eval::VendorCheck;
use tars_eval::eval::{
    CaseOutcome, CheckRates, CheckSummary, FinishedRun, JudgedRun, JudgingStarted, Quality,
    RunDiff, RunProgress, TrajectoryDiff,
};

use super::moved_pp;

/// Returns the stderr text of one step of a run; a case's start has no line end, so its outcome
/// finishes the line.
pub(super) fn progress(event: RunProgress<'_>) -> String {
    match event {
        RunProgress::Started {
            case_count,
            provider,
            model,
        } => format!("── eval: {case_count} cases, provider={provider}, model={model}\n"),
        RunProgress::CaseStarted { case_id } => format!("── {case_id} ... "),
        RunProgress::CaseFinished { report } => match &report.outcome {
            CaseOutcome::Ok { .. } => format!(
                "ok ({} tokens in / {} out, {} ms)\n",
                report.usage.input_tokens, report.usage.output_tokens, report.wall_clock_ms,
            ),
            CaseOutcome::Error { failure } => {
                format!("ERR ({} ms): {failure}\n", report.wall_clock_ms)
            }
        },
    }
}

/// Returns the stderr lines that close a run.
pub(super) fn run_done(run: &FinishedRun) -> Vec<String> {
    let m = &run.manifest;
    let usage = m.total_usage();
    let totals = format!(
        "── done. {} ok, {} error, total {} in / {} out tokens.",
        m.success_count(),
        m.error_count(),
        usage.input_tokens,
        usage.output_tokens,
    );
    let checks = m.check_summaries().into_iter().map(|c| check_line(&c));
    let path = format!("── manifest: {}", run.manifest_path.display());
    std::iter::once(totals)
        .chain(checks)
        .chain(std::iter::once(path))
        .collect()
}

fn check_line(c: &CheckSummary) -> String {
    let rate = c
        .violation_rate()
        .map_or_else(|| "n/a".to_string(), |r| format!("{:.0}%", r * 100.0));
    let not_judged = if c.not_judged > 0 {
        format!(", {} not judged", c.not_judged)
    } else {
        String::new()
    };
    format!(
        "── check {}: {rate} violation ({}/{} cases{not_judged})",
        c.name, c.violations, c.evaluated,
    )
}

/// Returns the stderr line said before a judge pass.
pub(super) fn judging(started: JudgingStarted<'_>, judge: &str) -> String {
    let vendors = match started.vendors {
        VendorCheck::Distinct { run, judge } => format!("vendor {judge} vs the run's {run}"),
        VendorCheck::UnknownRunVendor { .. } => {
            "UNVERIFIED: the run's model has no known vendor, so the judge may share it".into()
        }
        VendorCheck::UnknownJudgeVendor { run } => {
            format!("UNVERIFIED: the judge's model has no known vendor, so it may be {run}'s")
        }
    };
    format!(
        "── judging {} cases with {judge} (not `{}`; {vendors})",
        started.case_count, started.run_provider,
    )
}

/// Returns the stderr lines that close a judge pass: one per item the judge failed on, then the
/// counts.
pub(super) fn judged(run: &JudgedRun) -> Vec<String> {
    let r = &run.report;
    let prec = r
        .precision()
        .map(|p| format!("{:.1}%", p * 100.0))
        .unwrap_or_else(|| "n/a".into());
    let unjudged = r
        .unjudged
        .iter()
        .map(|u| format!("── not judged {}: {}", u.item_id, u.failure));
    let done = format!(
        "── done. TP={} FP={} Unsure={} not judged={}  precision={}  → {}",
        r.correct(),
        r.wrong(),
        r.unsure(),
        r.unjudged.len(),
        prec,
        run.report_path.display(),
    );
    unjudged.chain(std::iter::once(done)).collect()
}

/// What `eval diff` reports, built once from an [`RunDiff`]: `--json` serializes it and the
/// text is rendered from it, so the two carry the same facts.
#[derive(Debug, Serialize)]
pub(super) struct DiffView<'a> {
    baseline: String,
    candidate: String,
    operational: OperationalView,
    checks: Vec<CheckView<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trajectory: Option<TrajectoryView<'a>>,
    /// `None` when not both runs were judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<QualityView<'a>>,
}

/// Each figure as `[baseline, candidate]`.
#[derive(Debug, Serialize)]
struct OperationalView {
    case_count: [usize; 2],
    error_count: [usize; 2],
    tokens_in: [u64; 2],
    tokens_out: [u64; 2],
    latency_p50_ms: [u64; 2],
}

#[derive(Debug, Serialize)]
struct CheckView<'a> {
    name: &'a str,
    /// `None` when the baseline did not run the check or decided no case with it.
    baseline_violation_rate: Option<f64>,
    /// `None` when the candidate did not run the check or decided no case with it.
    candidate_violation_rate: Option<f64>,
    delta: Option<f64>,
    /// Says the candidate no longer runs a check the baseline ran.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dropped: bool,
    #[serde(skip)]
    rates: CheckRates,
}

#[derive(Debug, Serialize)]
struct TrajectoryView<'a> {
    mode: &'static str,
    paired: u32,
    a_only: u32,
    b_only: u32,
    divergent: u32,
    divergence_rate: f64,
    mean_similarity: f64,
    diverging_ids: &'a [String],
    mcnemar: Vec<McNemarView<'a>>,
}

#[derive(Debug, Serialize)]
struct McNemarView<'a> {
    check: &'a str,
    regressed_b: u32,
    improved_c: u32,
    /// `None` when no pair is discordant.
    test: Option<McNemarTest>,
}

/// How the two runs' judge verdicts compare.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum QualityView<'a> {
    /// One judge graded both runs.
    Compared {
        judge: &'a str,
        /// `None` when the judge decided no item of that run.
        baseline_precision: Option<f64>,
        candidate_precision: Option<f64>,
        regressed: u32,
        improved: u32,
        /// `None` when no pair is discordant.
        mcnemar: Option<McNemarTest>,
    },
    /// Two judges graded the runs, so their verdicts are not compared.
    JudgesDiffer {
        baseline_judge: &'a str,
        candidate_judge: &'a str,
    },
}

impl<'a> DiffView<'a> {
    pub(super) fn of(d: &'a RunDiff) -> Self {
        let (a, b) = (&d.baseline.manifest, &d.candidate.manifest);
        let (a_usage, b_usage) = (a.total_usage(), b.total_usage());
        Self {
            baseline: d.baseline.dir.display().to_string(),
            candidate: d.candidate.dir.display().to_string(),
            operational: OperationalView {
                case_count: [a.case_count(), b.case_count()],
                error_count: [a.error_count(), b.error_count()],
                tokens_in: [a_usage.input_tokens, b_usage.input_tokens],
                tokens_out: [a_usage.output_tokens, b_usage.output_tokens],
                latency_p50_ms: [d.baseline.latency_p50_ms, d.candidate.latency_p50_ms],
            },
            checks: d.checks.iter().map(CheckView::of).collect(),
            trajectory: d.trajectory.as_ref().map(TrajectoryView::of),
            quality: d.quality.as_ref().map(QualityView::of),
        }
    }
}

impl<'a> CheckView<'a> {
    fn of(c: &'a tars_eval::eval::CheckDelta) -> Self {
        let (baseline, candidate, dropped) = match c.rates {
            CheckRates::Both {
                baseline,
                candidate,
            } => (baseline, candidate, false),
            CheckRates::New { candidate } => (None, candidate, false),
            CheckRates::Dropped { baseline } => (baseline, None, true),
        };
        Self {
            name: &c.name,
            baseline_violation_rate: baseline,
            candidate_violation_rate: candidate,
            delta: baseline.zip(candidate).map(|(x, y)| y - x),
            dropped,
            rates: c.rates,
        }
    }
}

impl<'a> TrajectoryView<'a> {
    fn of(td: &'a TrajectoryDiff) -> Self {
        Self {
            mode: td.mode.as_str(),
            paired: td.paired,
            a_only: td.a_only,
            b_only: td.b_only,
            divergent: td.divergent,
            divergence_rate: td.divergence_rate(),
            mean_similarity: td.mean_similarity,
            diverging_ids: &td.diverging_ids,
            mcnemar: td
                .mcnemar
                .iter()
                .map(|m| McNemarView {
                    check: &m.check,
                    regressed_b: m.result.b,
                    improved_c: m.result.c,
                    test: m.result.test(),
                })
                .collect(),
        }
    }
}

impl<'a> QualityView<'a> {
    fn of(q: &'a Quality) -> Self {
        match q {
            Quality::Compared(q) => Self::Compared {
                judge: &q.baseline.judge_id,
                baseline_precision: q.baseline.precision(),
                candidate_precision: q.candidate.precision(),
                regressed: q.paired.b,
                improved: q.paired.c,
                mcnemar: q.paired.test(),
            },
            Quality::JudgesDiffer {
                baseline,
                candidate,
            } => Self::JudgesDiffer {
                baseline_judge: &baseline.judge_id,
                candidate_judge: &candidate.judge_id,
            },
        }
    }
}

fn mcnemar_statistic(test: &McNemarTest) -> String {
    match *test {
        McNemarTest::ExactBinomial { p_value } => format!("exact p={p_value:.3}"),
        McNemarTest::ChiSquared { statistic, p_value } => {
            format!("χ²={statistic:.2} p={p_value:.3}")
        }
    }
}

fn mcnemar_verdict(test: &McNemarTest, not_significant_hint: &str) -> String {
    if test.significant_at(0.01) {
        "significant at α=0.01".to_string()
    } else if test.significant_at(0.05) {
        "significant at α=0.05".to_string()
    } else {
        format!("NOT significant at α=0.05{not_significant_hint}")
    }
}

/// Returns the diff as the stdout lines a person reads.
pub(super) fn diff_text(d: &DiffView<'_>) -> Vec<String> {
    let o = &d.operational;
    let row_i = |label: &str, [a, b]: [usize; 2]| {
        format!("  {label:<14} {a:>10} → {b:>10}   {}", delta_i(a, b))
    };
    let row_tokens = |label: &str, [a, b]: [u64; 2]| {
        format!(
            "  {label:<14} {a:>10} → {b:>10}   {}",
            delta_pct(a as f64, b as f64)
        )
    };
    let [a_lat, b_lat] = o.latency_p50_ms;
    let mut lines = vec![
        "eval diff".to_string(),
        format!("  baseline:  {}", d.baseline),
        format!("  candidate: {}", d.candidate),
        String::new(),
        "operational:".to_string(),
        row_i("cases", o.case_count),
        row_i("errors", o.error_count),
        row_tokens("tokens in", o.tokens_in),
        row_tokens("tokens out", o.tokens_out),
        format!(
            "  {:<14} {:>9}ms → {:>9}ms   {}",
            "latency p50",
            a_lat,
            b_lat,
            delta_pct(a_lat as f64, b_lat as f64)
        ),
    ];

    if !d.checks.is_empty() {
        lines.push(String::new());
        lines.push("checks (violation rate):".to_string());
        lines.extend(d.checks.iter().map(|c| {
            let arrow = match c.rates {
                CheckRates::Both {
                    baseline,
                    candidate,
                } => format!(
                    "{} → {}   {}",
                    rate_cell(baseline),
                    rate_cell(candidate),
                    baseline
                        .zip(candidate)
                        .map_or_else(String::new, |(x, y)| delta_pp(x, y))
                ),
                CheckRates::New { candidate } => {
                    format!("    —   → {}   (new)", rate_cell(candidate))
                }
                CheckRates::Dropped { baseline } => {
                    format!("{} →     —     (dropped)", rate_cell(baseline))
                }
            };
            format!("  {:<24} {arrow}", c.name)
        }));
    }

    if let Some(td) = &d.trajectory {
        lines.push(String::new());
        lines.extend(trajectory_text(td));
    }

    match &d.quality {
        None => {}
        Some(QualityView::JudgesDiffer {
            baseline_judge,
            candidate_judge,
        }) => {
            lines.push(String::new());
            lines.push(format!(
                "quality: not compared, the runs were judged by {baseline_judge} and \
                 {candidate_judge}; judge both with one judge to compare",
            ));
        }
        Some(QualityView::Compared {
            judge,
            baseline_precision,
            candidate_precision,
            regressed,
            improved,
            mcnemar,
        }) => {
            lines.push(String::new());
            lines.push(format!("quality (judge: {judge}):"));
            lines.push(match (baseline_precision, candidate_precision) {
                (Some(x), Some(y)) => format!(
                    "  precision   {:>6.1}% → {:>6.1}%   {}",
                    x * 100.0,
                    y * 100.0,
                    delta_pp_higher_better(*x, *y)
                ),
                _ => "  precision   (no decisive verdicts)".to_string(),
            });
            lines.push(format!(
                "  paired changes: improved (FP→TP)={improved}, regressed (TP→FP)={regressed}"
            ));
            lines.push(match mcnemar {
                None => "  McNemar: no discordant pairs (runs agree on every item)".to_string(),
                Some(test) => format!(
                    "  McNemar: b={regressed} c={improved} {} → {}",
                    mcnemar_statistic(test),
                    mcnemar_verdict(test, " (need more cases or bigger effect)")
                ),
            });
        }
    }
    lines
}

fn trajectory_text(td: &TrajectoryView<'_>) -> Vec<String> {
    let mut lines = vec![
        format!("trajectory (mode={}):", td.mode),
        format!(
            "  paired cases:   {} (a-only {}, b-only {})",
            td.paired, td.a_only, td.b_only
        ),
        format!(
            "  divergence:     {:>5.1}%  ({}/{} cases differ)   mean similarity {:.2}",
            td.divergence_rate * 100.0,
            td.divergent,
            td.paired,
            td.mean_similarity
        ),
    ];
    if !td.diverging_ids.is_empty() {
        const CAP: usize = 12;
        let shown: Vec<&str> = td
            .diverging_ids
            .iter()
            .take(CAP)
            .map(String::as_str)
            .collect();
        let more = td.diverging_ids.len().saturating_sub(CAP);
        let suffix = if more > 0 {
            format!(", +{more} more")
        } else {
            String::new()
        };
        lines.push(format!("  diverging:      {}{}", shown.join(", "), suffix));
    }
    if td.mcnemar.is_empty() {
        lines.push(
            "  McNemar:        (no shared trajectory-match check — run both with \
             --check trajectory-match:<mode> for significance)"
                .to_string(),
        );
    }
    lines.extend(td.mcnemar.iter().map(|m| match &m.test {
        None => format!("  McNemar ({}): no discordant pairs (runs agree)", m.check),
        Some(test) => format!(
            "  McNemar ({}): regressed b={} improved c={} {} → {}",
            m.check,
            m.regressed_b,
            m.improved_c,
            mcnemar_statistic(test),
            mcnemar_verdict(test, "")
        ),
    }));
    lines
}

/// Formats a violation rate, `n/a` for a check that decided no case.
fn rate_cell(rate: Option<f64>) -> String {
    rate.map_or_else(
        || format!("{:>7}", "n/a"),
        |r| format!("{:>6.1}%", r * 100.0),
    )
}

/// Formats `b - a` with its sign.
fn delta_i(a: usize, b: usize) -> String {
    match b.cmp(&a) {
        std::cmp::Ordering::Equal => "(=)".into(),
        std::cmp::Ordering::Greater => format!("(+{})", b - a),
        std::cmp::Ordering::Less => format!("(-{})", a - b),
    }
}

/// Formats the percent change from `a` to `b`.
fn delta_pct(a: f64, b: f64) -> String {
    if a == 0.0 {
        return if b == 0.0 {
            "(=)".into()
        } else {
            "(new)".into()
        };
    }
    let pct = (b - a) / a * 100.0;
    if pct.abs() < 0.05 {
        "(=)".into()
    } else if pct > 0.0 {
        format!("(+{pct:.0}%)")
    } else {
        format!("({pct:.0}%)")
    }
}

/// Formats the percentage-point change of a rate where higher is worse (a violation rate).
fn delta_pp(a: f64, b: f64) -> String {
    match moved_pp(a, b) {
        None => "(=)".into(),
        Some(pp) if pp > 0.0 => format!("(+{pp:.1}pp ⚠)"),
        Some(pp) => format!("({pp:.1}pp ✓)"),
    }
}

/// Formats the percentage-point change of a rate where higher is better (precision).
fn delta_pp_higher_better(a: f64, b: f64) -> String {
    match moved_pp(a, b) {
        None => "(=)".into(),
        Some(pp) if pp > 0.0 => format!("(+{pp:.1}pp ✓)"),
        Some(pp) => format!("({pp:.1}pp ⚠)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_eval::eval::CheckDelta;

    #[test]
    fn a_dropped_check_is_in_the_json_with_no_candidate_rate() {
        let dropped = CheckDelta {
            name: "non-empty".into(),
            rates: CheckRates::Dropped {
                baseline: Some(0.25),
            },
        };
        let json = serde_json::to_value(CheckView::of(&dropped)).unwrap();
        assert_eq!(json["dropped"], true);
        assert_eq!(json["baseline_violation_rate"], 0.25);
        assert!(json["candidate_violation_rate"].is_null());
        assert!(json["delta"].is_null());

        let both = CheckDelta {
            name: "non-empty".into(),
            rates: CheckRates::Both {
                baseline: Some(0.25),
                candidate: Some(0.5),
            },
        };
        let json = serde_json::to_value(CheckView::of(&both)).unwrap();
        assert!(json.get("dropped").is_none());
        assert_eq!(json["delta"], 0.25);
    }

    #[test]
    fn the_quality_section_says_whether_one_judge_graded_both_runs() {
        let differ = QualityView::JudgesDiffer {
            baseline_judge: "a",
            candidate_judge: "b",
        };
        let json = serde_json::to_value(&differ).unwrap();
        assert_eq!(json["state"], "judges_differ");
        assert_eq!(json["candidate_judge"], "b");

        let compared = QualityView::Compared {
            judge: "a",
            baseline_precision: Some(0.9),
            candidate_precision: None,
            regressed: 1,
            improved: 0,
            mcnemar: None,
        };
        let json = serde_json::to_value(&compared).unwrap();
        assert_eq!(json["state"], "compared");
        assert!(json["candidate_precision"].is_null());
    }
}
