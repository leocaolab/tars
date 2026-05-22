//! `tars eval` — corpus replay + (later) judge / diff.
//!
//! See `docs/eval-and-arc-llm-roadmap.md §1.3` for the design intent:
//! turn "is prompt A better than prompt B?" into a reproducible run +
//! a saved artifact two future runs can be diffed against.
//!
//! V1 subcommands:
//!   `tars eval run --corpus <dir>` — replay corpus through a pipeline
//!
//! Future (deliberately not in V1):
//!   `tars eval judge` — run a Judge over an eval-run's outputs
//!   `tars eval diff`  — compare two eval-runs
//!
//! ## Corpus directory layout
//!
//! ```text
//! corpus/
//!   case_001/
//!     input.txt       (required)
//!     system.txt      (optional system prompt for this case)
//!     expected.txt    (optional gold standard — read by `eval judge`)
//!   case_002/
//!     ...
//! ```
//!
//! Any subdirectory missing `input.txt` is skipped with a warning;
//! any file is welcome inside a case (only the three above are read).
//!
//! ## Output layout
//!
//! ```text
//! eval-runs/<timestamp>/
//!   manifest.json           — EvalRunManifest (case_count, totals)
//!   case_001/
//!     output.txt            — the agent's response text
//!     report.json           — EvalCaseReport (status, usage, wall clock)
//!   case_002/
//!     ...
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use tars_pipeline::{
    JsonShapeValidator, MaxLengthValidator, NotEmptyValidator, Pipeline, PipelineOpts,
};
use tars_runtime::{CheckRunner, Invariant, ValidatorInvariant};
use tars_types::{ChatRequest, ChatResponse, ChatResponseBuilder, ModelHint, RequestContext, Usage};

use crate::config_loader;
use crate::dispatch::{build_registry_with_breaker, pick_provider};

#[derive(Args, Debug)]
pub struct EvalArgs {
    #[command(subcommand)]
    pub command: EvalCommand,
}

#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// Replay a corpus of cases through a pipeline, writing per-case
    /// outputs + a manifest two runs can be diffed against.
    Run(EvalRunArgs),
    /// Behavioral diff of two eval runs (baseline vs candidate).
    /// Compares operational metrics (errors / tokens / latency) and
    /// per-check violation rates — NOT raw output text. See
    /// `docs/architecture/18-agent-testing.md` §2.
    Diff(EvalDiffArgs),
}

#[derive(Args, Debug)]
pub struct EvalDiffArgs {
    /// Baseline eval-run directory (contains manifest.json).
    pub baseline: PathBuf,
    /// Candidate eval-run directory (contains manifest.json).
    pub candidate: PathBuf,
    /// Emit the diff as a single JSON object on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct EvalRunArgs {
    /// Corpus directory. Each subdirectory is one case; required
    /// `input.txt`, optional `system.txt` and `expected.txt`.
    #[arg(long)]
    pub corpus: PathBuf,

    /// Provider id from your config. Defaults to the first
    /// user-configured provider (same rule as `tars run`).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model hint. If not set, the provider's default is used.
    #[arg(long)]
    pub model: Option<String>,

    /// Output directory. Default: `./eval-runs/<timestamp>/`.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,

    /// Per-case max output token bound.
    #[arg(long)]
    pub max_output_tokens: Option<u32>,

    /// Built-in invariant checks to run against each output (repeatable).
    /// Recognized: `non-empty`, `valid-json`, `max-length:<N>`.
    /// Custom invariants are a Rust-API feature (see Doc 18 §4.1).
    #[arg(long = "check")]
    pub checks: Vec<String>,
}

/// Map a `--check` spec to a built-in invariant. Returns an error for
/// unrecognized specs so a typo fails loudly instead of silently
/// running no check.
fn build_invariant(spec: &str) -> Result<Arc<dyn Invariant>> {
    let inv: Arc<dyn Invariant> = match spec {
        "non-empty" => {
            Arc::new(ValidatorInvariant::new(Arc::new(NotEmptyValidator::new())))
        }
        "valid-json" => {
            Arc::new(ValidatorInvariant::new(Arc::new(JsonShapeValidator::new())))
        }
        other if other.starts_with("max-length:") => {
            let n: usize = other
                .trim_start_matches("max-length:")
                .parse()
                .with_context(|| format!("bad max-length spec: {other}"))?;
            Arc::new(ValidatorInvariant::new(Arc::new(
                MaxLengthValidator::truncate_above(n),
            )))
        }
        other => anyhow::bail!(
            "unknown --check `{other}`. Recognized: non-empty, valid-json, max-length:<N>"
        ),
    };
    Ok(inv)
}

/// Manifest written to `<output>/manifest.json`. Stable schema so
/// `tars eval diff` (future) can pattern-match without ad-hoc parsing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalRunManifest {
    /// Path the user gave for `--corpus` (absolute when possible).
    pub corpus_path: String,
    /// Provider id used.
    pub provider_id: String,
    /// Model hint label sent on each request.
    pub model: String,
    /// UTC millis when the first case started.
    pub started_at_ms: u64,
    /// UTC millis when the last case finished.
    pub ended_at_ms: u64,
    pub case_count: u32,
    pub success_count: u32,
    pub error_count: u32,
    /// Sum of `Usage` across successful cases. Failed cases contribute
    /// nothing (no usage reported on a failed call).
    pub total_usage: Usage,
    /// Invariant check rollups, one per `--check`. Empty if no checks
    /// were requested. This is the "behavioral" part of the report —
    /// `tars eval diff` reads these to show violation-rate deltas.
    #[serde(default)]
    pub checks: Vec<CheckSummary>,
    /// Per-case summary, in run order. Each entry mirrors the
    /// `report.json` written inside that case's directory.
    pub cases: Vec<EvalCaseReport>,
}

/// Aggregate of one invariant across all cases in a run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckSummary {
    pub name: String,
    /// Cases the check ran on (= successful cases; we don't check
    /// errored cases since there's no real output).
    pub evaluated: u32,
    pub violations: u32,
    /// `violations / evaluated`, 0.0 when `evaluated == 0`.
    pub violation_rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalCaseReport {
    pub case_id: String,
    pub status: EvalCaseStatus,
    pub wall_clock_ms: u64,
    pub usage: Usage,
    /// Length of the response in chars. The full text lives at
    /// `<case>/output.txt`; keeping the count here avoids re-reading
    /// the file for summaries.
    pub output_chars: u64,
    /// Truncated error message when `status == "error"`. Empty when OK.
    pub error: Option<String>,
    /// Per-invariant results for this case. Empty if no checks ran.
    #[serde(default)]
    pub checks: Vec<CaseCheckResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseCheckResult {
    pub name: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalCaseStatus {
    Ok,
    Error,
}

pub async fn execute(args: EvalArgs, config_path: Option<PathBuf>) -> Result<()> {
    match args.command {
        EvalCommand::Run(a) => run_eval(a, config_path).await,
        EvalCommand::Diff(a) => run_diff(a),
    }
}

// ─── eval diff ────────────────────────────────────────────────────────

fn load_manifest(dir: &Path) -> Result<EvalRunManifest> {
    let path = dir.join("manifest.json");
    let body = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))
}

fn p50(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

fn run_diff(args: EvalDiffArgs) -> Result<()> {
    let a = load_manifest(&args.baseline)?;
    let b = load_manifest(&args.candidate)?;

    // Operational metrics.
    let a_lat = p50(a.cases.iter().map(|c| c.wall_clock_ms).collect());
    let b_lat = p50(b.cases.iter().map(|c| c.wall_clock_ms).collect());

    if args.json {
        // Machine-readable: emit aligned check deltas + operational deltas.
        let check_deltas: Vec<serde_json::Value> = b
            .checks
            .iter()
            .map(|bc| {
                let base = a.checks.iter().find(|ac| ac.name == bc.name);
                serde_json::json!({
                    "name": bc.name,
                    "baseline_violation_rate": base.map(|x| x.violation_rate),
                    "candidate_violation_rate": bc.violation_rate,
                    "delta": base.map(|x| bc.violation_rate - x.violation_rate),
                })
            })
            .collect();
        let out = serde_json::json!({
            "baseline": args.baseline.display().to_string(),
            "candidate": args.candidate.display().to_string(),
            "operational": {
                "case_count": [a.case_count, b.case_count],
                "error_count": [a.error_count, b.error_count],
                "tokens_in": [a.total_usage.input_tokens, b.total_usage.input_tokens],
                "tokens_out": [a.total_usage.output_tokens, b.total_usage.output_tokens],
                "latency_p50_ms": [a_lat, b_lat],
            },
            "checks": check_deltas,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Human table.
    println!("eval diff");
    println!("  baseline:  {}", args.baseline.display());
    println!("  candidate: {}", args.candidate.display());
    println!();
    println!("operational:");
    println!("  {:<14} {:>10} → {:>10}   {}", "cases", a.case_count, b.case_count, delta_i(a.case_count as i64, b.case_count as i64));
    println!("  {:<14} {:>10} → {:>10}   {}", "errors", a.error_count, b.error_count, delta_i(a.error_count as i64, b.error_count as i64));
    println!("  {:<14} {:>10} → {:>10}   {}", "tokens in", a.total_usage.input_tokens, b.total_usage.input_tokens, delta_pct(a.total_usage.input_tokens as f64, b.total_usage.input_tokens as f64));
    println!("  {:<14} {:>10} → {:>10}   {}", "tokens out", a.total_usage.output_tokens, b.total_usage.output_tokens, delta_pct(a.total_usage.output_tokens as f64, b.total_usage.output_tokens as f64));
    println!("  {:<14} {:>9}ms → {:>9}ms   {}", "latency p50", a_lat, b_lat, delta_pct(a_lat as f64, b_lat as f64));

    if !a.checks.is_empty() || !b.checks.is_empty() {
        println!();
        println!("checks (violation rate):");
        // Union of check names, candidate order first.
        let mut names: Vec<String> = b.checks.iter().map(|c| c.name.clone()).collect();
        for ac in &a.checks {
            if !names.contains(&ac.name) {
                names.push(ac.name.clone());
            }
        }
        for name in &names {
            let av = a.checks.iter().find(|c| &c.name == name).map(|c| c.violation_rate);
            let bv = b.checks.iter().find(|c| &c.name == name).map(|c| c.violation_rate);
            let arrow = match (av, bv) {
                (Some(x), Some(y)) => format!(
                    "{:>6.1}% → {:>6.1}%   {}",
                    x * 100.0,
                    y * 100.0,
                    delta_pp(x, y)
                ),
                (None, Some(y)) => format!("    —   → {:>6.1}%   (new)", y * 100.0),
                (Some(x), None) => format!("{:>6.1}% →     —     (dropped)", x * 100.0),
                (None, None) => "—".into(),
            };
            println!("  {name:<24} {arrow}");
        }
    }
    Ok(())
}

/// Integer delta with sign + direction marker.
fn delta_i(a: i64, b: i64) -> String {
    let d = b - a;
    match d.cmp(&0) {
        std::cmp::Ordering::Equal => "(=)".into(),
        std::cmp::Ordering::Greater => format!("(+{d})"),
        std::cmp::Ordering::Less => format!("({d})"),
    }
}

/// Percent change a→b.
fn delta_pct(a: f64, b: f64) -> String {
    if a == 0.0 {
        return if b == 0.0 { "(=)".into() } else { "(new)".into() };
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

/// Percentage-point delta for rates (already in [0,1]).
fn delta_pp(a: f64, b: f64) -> String {
    let pp = (b - a) * 100.0;
    if pp.abs() < 0.05 {
        "(=)".into()
    } else if pp > 0.0 {
        format!("(+{pp:.1}pp ⚠)") // violation rate UP = worse
    } else {
        format!("({pp:.1}pp ✓)") // violation rate DOWN = better
    }
}

async fn run_eval(args: EvalRunArgs, config_path: Option<PathBuf>) -> Result<()> {
    // 1. Config + registry + provider selection — same path tars run uses.
    let cfg = config_loader::load(config_path)?;
    let registry = build_registry_with_breaker(&cfg, /* breaker */ true)?;
    let provider_id = pick_provider(&cfg, args.provider.as_deref())?;
    let provider = registry
        .get(&provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider {provider_id} missing from registry"))?;

    // 2. Build pipeline using the canonical default chain. Cache
    //    namespace is unique-per-run so cases don't unexpectedly hit
    //    each other across runs (and we still get intra-run cache
    //    benefits if cases share prompts — rare but free).
    let pipeline = Pipeline::default_chain(provider, PipelineOpts::new(provider_id.clone()));
    let pipeline = Arc::new(pipeline);

    // 2b. Build the invariant check runner from --check specs.
    let mut invariants: Vec<Arc<dyn Invariant>> = Vec::with_capacity(args.checks.len());
    for spec in &args.checks {
        invariants.push(build_invariant(spec)?);
    }
    let check_runner = CheckRunner::new(invariants);
    let check_names: Vec<String> = check_runner.names().iter().map(|s| s.to_string()).collect();

    // 3. Discover cases.
    let cases = load_corpus(&args.corpus)
        .with_context(|| format!("loading corpus from {}", args.corpus.display()))?;
    if cases.is_empty() {
        anyhow::bail!(
            "corpus at {} contains no readable cases (need subdirectories with input.txt)",
            args.corpus.display()
        );
    }
    eprintln!(
        "── eval: {} cases, provider={}, model={}",
        cases.len(),
        provider_id,
        args.model.as_deref().unwrap_or("(provider default)"),
    );

    // 4. Output directory.
    let output_dir = match args.output.clone() {
        Some(p) => p,
        None => PathBuf::from(format!("eval-runs/{}", utc_now_stamp())),
    };
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating output dir {}", output_dir.display()))?;

    // 5. Per-case loop. Failures are recorded into the report, not
    //    propagated — the value of an eval is seeing the distribution.
    let started_at_ms = utc_now_millis();
    let mut reports: Vec<EvalCaseReport> = Vec::with_capacity(cases.len());
    let mut total_usage = Usage::default();
    let model_label = args
        .model
        .clone()
        .unwrap_or_else(|| "(provider default)".into());

    for case in &cases {
        eprint!("── {} ... ", case.id);
        let case_out_dir = output_dir.join(&case.id);
        fs::create_dir_all(&case_out_dir)
            .with_context(|| format!("creating case dir {}", case_out_dir.display()))?;
        let report = run_one_case(
            pipeline.clone(),
            case,
            args.model.as_deref(),
            args.max_output_tokens,
            &check_runner,
        )
        .await;
        // Persist response text + per-case report regardless of outcome.
        let output_path = case_out_dir.join("output.txt");
        let output_text = report.output_text.clone();
        fs::write(&output_path, output_text)
            .with_context(|| format!("writing {}", output_path.display()))?;
        let report_path = case_out_dir.join("report.json");
        fs::write(&report_path, serde_json::to_string_pretty(&report.summary)?)
            .with_context(|| format!("writing {}", report_path.display()))?;

        match &report.summary.status {
            EvalCaseStatus::Ok => {
                eprintln!(
                    "ok ({} tokens in / {} out, {} ms)",
                    report.summary.usage.input_tokens,
                    report.summary.usage.output_tokens,
                    report.summary.wall_clock_ms,
                );
                total_usage = merge_usage(total_usage, &report.summary.usage);
            }
            EvalCaseStatus::Error => {
                eprintln!(
                    "ERR ({} ms): {}",
                    report.summary.wall_clock_ms,
                    report.summary.error.as_deref().unwrap_or("(no detail)"),
                );
            }
        }
        reports.push(report.summary);
    }

    // 6. Aggregate per-check violation rates across all cases.
    let check_summaries: Vec<CheckSummary> = check_names
        .iter()
        .map(|name| {
            let mut evaluated = 0u32;
            let mut violations = 0u32;
            for case in &reports {
                if let Some(c) = case.checks.iter().find(|c| &c.name == name) {
                    evaluated += 1;
                    if !c.passed {
                        violations += 1;
                    }
                }
            }
            CheckSummary {
                name: name.clone(),
                evaluated,
                violations,
                violation_rate: if evaluated == 0 {
                    0.0
                } else {
                    violations as f64 / evaluated as f64
                },
            }
        })
        .collect();

    // 7. Manifest.
    let ended_at_ms = utc_now_millis();
    let manifest = EvalRunManifest {
        corpus_path: args
            .corpus
            .canonicalize()
            .unwrap_or(args.corpus.clone())
            .display()
            .to_string(),
        provider_id: provider_id.as_str().to_string(),
        model: model_label,
        started_at_ms,
        ended_at_ms,
        case_count: reports.len() as u32,
        success_count: reports
            .iter()
            .filter(|r| r.status == EvalCaseStatus::Ok)
            .count() as u32,
        error_count: reports
            .iter()
            .filter(|r| r.status == EvalCaseStatus::Error)
            .count() as u32,
        total_usage,
        checks: check_summaries,
        cases: reports,
    };
    let manifest_path = output_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;

    eprintln!(
        "── done. {} ok, {} error, total {} in / {} out tokens.",
        manifest.success_count,
        manifest.error_count,
        manifest.total_usage.input_tokens,
        manifest.total_usage.output_tokens,
    );
    for c in &manifest.checks {
        eprintln!(
            "── check {}: {:.0}% violation ({}/{} cases)",
            c.name,
            c.violation_rate * 100.0,
            c.violations,
            c.evaluated,
        );
    }
    eprintln!("── manifest: {}", manifest_path.display());
    Ok(())
}

// ─── one case ─────────────────────────────────────────────────────────

/// One case loaded from disk. `expected` is preserved but not used by
/// `eval run` itself — read by `eval judge` later.
struct Case {
    id: String,
    input: String,
    system: Option<String>,
    #[allow(dead_code)]
    expected: Option<String>,
}

struct CaseOutcome {
    summary: EvalCaseReport,
    output_text: String,
}

async fn run_one_case(
    pipeline: Arc<Pipeline>,
    case: &Case,
    model: Option<&str>,
    max_output_tokens: Option<u32>,
    checks: &CheckRunner,
) -> CaseOutcome {
    let started = Instant::now();
    let mut req = ChatRequest::user(
        match model {
            Some(m) => ModelHint::Explicit(m.into()),
            None => ModelHint::Explicit("".into()), // forces provider to use its capability's
                                                    // default; explicit-empty is fine for many
                                                    // backends and avoids the "model required"
                                                    // path the CLI providers hit
        },
        case.input.clone(),
    );
    if let Some(sys) = &case.system {
        req = req.with_system(sys.clone());
    }
    if let Some(cap) = max_output_tokens {
        req.max_output_tokens = Some(cap);
    }

    // Keep a copy of the request — invariants need it (and `call`
    // consumes the original).
    let req_for_checks = req.clone();
    let ctx = RequestContext::test_default();
    let stream_result = pipeline.clone().call(req, ctx).await;
    let mut acc = ChatResponseBuilder::new();
    let mut error: Option<String> = None;

    match stream_result {
        Ok(mut stream) => {
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ev) => acc.apply(ev),
                    Err(e) => {
                        error = Some(truncate(&format!("{e}"), 500));
                        break;
                    }
                }
            }
        }
        Err(e) => error = Some(truncate(&format!("{e}"), 500)),
    }

    let response: ChatResponse = acc.finish();
    let output_text = response.text.clone();
    let status = if error.is_some() {
        EvalCaseStatus::Error
    } else {
        EvalCaseStatus::Ok
    };

    // Run invariants only on successful cases — an errored call has no
    // real output to check.
    let case_checks: Vec<CaseCheckResult> = if status == EvalCaseStatus::Ok && !checks.is_empty() {
        checks
            .run(&req_for_checks, &response)
            .into_iter()
            .map(|(name, r)| CaseCheckResult {
                name,
                passed: r.passed,
                detail: r.detail,
            })
            .collect()
    } else {
        Vec::new()
    };

    CaseOutcome {
        summary: EvalCaseReport {
            case_id: case.id.clone(),
            status,
            wall_clock_ms: started.elapsed().as_millis() as u64,
            usage: response.usage,
            output_chars: output_text.chars().count() as u64,
            error,
            checks: case_checks,
        },
        output_text,
    }
}

// ─── corpus loader ────────────────────────────────────────────────────

fn load_corpus(root: &Path) -> Result<Vec<Case>> {
    if !root.is_dir() {
        anyhow::bail!("corpus path is not a directory: {}", root.display());
    }
    let mut cases: Vec<Case> = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() || id.starts_with('.') {
            continue;
        }
        let input_path = path.join("input.txt");
        if !input_path.exists() {
            tracing::warn!(case = %id, "skipping case: no input.txt");
            continue;
        }
        let input = fs::read_to_string(&input_path)
            .with_context(|| format!("reading {}", input_path.display()))?;
        let system = read_optional(&path.join("system.txt"))?;
        let expected = read_optional(&path.join("expected.txt"))?;
        cases.push(Case {
            id,
            input,
            system,
            expected,
        });
    }
    // Stable order — caller asks for case_001 < case_002 < case_010 to
    // sort lexically with zero-padding; we don't otherwise re-order.
    cases.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(cases)
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

// ─── helpers ──────────────────────────────────────────────────────────

fn merge_usage(a: Usage, b: &Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        cached_input_tokens: a
            .cached_input_tokens
            .saturating_add(b.cached_input_tokens),
        cache_creation_tokens: a
            .cache_creation_tokens
            .saturating_add(b.cache_creation_tokens),
        thinking_tokens: a.thinking_tokens.saturating_add(b.thinking_tokens),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

fn utc_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn utc_now_stamp() -> String {
    // Format that file systems and humans both tolerate: 2026-05-20T15-30-00.
    // Doesn't depend on `chrono` (tars-cli avoids the dep).
    let secs = utc_now_millis() / 1000;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days_since_epoch = secs / 86400;
    let (y, mo, d) = civil_from_days(days_since_epoch as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{m:02}-{s:02}")
}

/// Days since 1970-01-01 → (year, month, day). Pure function, no
/// timezone, no leap-second handling — good enough for filenames.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    // Howard Hinnant's date algorithm, simplified for >=1970.
    let z = z + 719_468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn load_corpus_reads_cases_in_lex_order() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("case_002").join("input.txt"), "second");
        write(&dir.path().join("case_001").join("input.txt"), "first");
        write(&dir.path().join("case_001").join("expected.txt"), "exp");
        write(&dir.path().join("case_001").join("system.txt"), "sys");
        let cases = load_corpus(dir.path()).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].id, "case_001");
        assert_eq!(cases[0].input, "first");
        assert_eq!(cases[0].system.as_deref(), Some("sys"));
        assert_eq!(cases[0].expected.as_deref(), Some("exp"));
        assert_eq!(cases[1].id, "case_002");
        assert!(cases[1].system.is_none());
    }

    #[test]
    fn load_corpus_skips_subdirs_without_input() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("good").join("input.txt"), "ok");
        // empty subdir
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        // dot-prefixed (e.g. .DS_Store dir)
        fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        write(&dir.path().join(".hidden").join("input.txt"), "should-skip");
        let cases = load_corpus(dir.path()).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "good");
    }

    #[test]
    fn load_corpus_errors_on_non_dir_path() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("not-a-dir.txt");
        write(&file, "x");
        let err = load_corpus(&file).err().expect("must error");
        assert!(err.to_string().contains("not a directory"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), "abc");
        // truncate at 3 → "abc…" (the "…" is appended, not counted)
        assert_eq!(truncate("abcdef", 3), "abc…");
        // multi-byte safe
        let s = "あいうえお";
        let t = truncate(s, 2);
        assert!(t.starts_with("あい"));
        assert!(t.ends_with('…'));
    }

    #[test]
    fn manifest_serde_round_trip() {
        let m = EvalRunManifest {
            corpus_path: "/tmp/corpus".into(),
            provider_id: "anthropic".into(),
            model: "claude-opus-4-7".into(),
            started_at_ms: 1_000_000,
            ended_at_ms: 1_005_000,
            case_count: 2,
            success_count: 2,
            error_count: 0,
            total_usage: Usage {
                input_tokens: 100,
                output_tokens: 200,
                ..Usage::default()
            },
            checks: vec![CheckSummary {
                name: "non-empty".into(),
                evaluated: 2,
                violations: 0,
                violation_rate: 0.0,
            }],
            cases: vec![EvalCaseReport {
                case_id: "case_001".into(),
                status: EvalCaseStatus::Ok,
                wall_clock_ms: 2500,
                usage: Usage::default(),
                output_chars: 80,
                error: None,
                checks: vec![CaseCheckResult {
                    name: "non-empty".into(),
                    passed: true,
                    detail: None,
                }],
            }],
        };
        let v = serde_json::to_value(&m).unwrap();
        let back: EvalRunManifest = serde_json::from_value(v).unwrap();
        assert_eq!(back.case_count, 2);
        assert_eq!(back.success_count, 2);
        assert_eq!(back.cases[0].status, EvalCaseStatus::Ok);
        assert_eq!(back.checks[0].name, "non-empty");
        assert_eq!(back.cases[0].checks[0].passed, true);
    }

    #[test]
    fn civil_from_days_anchor_2026_05_20() {
        // 56 years × 365 + 14 leap years + (31+28+31+30+19) day-of-year
        // = 20454 + 139 = 20593 days from 1970-01-01 to 2026-05-20.
        let (y, mo, d) = civil_from_days(20_593);
        assert_eq!((y, mo, d), (2026, 5, 20));
    }
}
