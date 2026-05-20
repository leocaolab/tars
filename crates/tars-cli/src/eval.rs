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

use tars_pipeline::{Pipeline, PipelineOpts};
use tars_types::{ChatRequest, ChatResponseBuilder, ModelHint, RequestContext, Usage};

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
    /// Per-case summary, in run order. Each entry mirrors the
    /// `report.json` written inside that case's directory.
    pub cases: Vec<EvalCaseReport>,
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
        let report =
            run_one_case(pipeline.clone(), case, args.model.as_deref(), args.max_output_tokens)
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

    // 6. Manifest.
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
        cases: reports,
    };
    let manifest_path = output_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;

    eprintln!(
        "── done. {} ok, {} error, total {} in / {} out tokens. manifest: {}",
        manifest.success_count,
        manifest.error_count,
        manifest.total_usage.input_tokens,
        manifest.total_usage.output_tokens,
        manifest_path.display(),
    );
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

    let response = acc.finish();
    let output_text = response.text.clone();
    let status = if error.is_some() {
        EvalCaseStatus::Error
    } else {
        EvalCaseStatus::Ok
    };
    CaseOutcome {
        summary: EvalCaseReport {
            case_id: case.id.clone(),
            status,
            wall_clock_ms: started.elapsed().as_millis() as u64,
            usage: response.usage,
            output_chars: output_text.chars().count() as u64,
            error,
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
            cases: vec![EvalCaseReport {
                case_id: "case_001".into(),
                status: EvalCaseStatus::Ok,
                wall_clock_ms: 2500,
                usage: Usage::default(),
                output_chars: 80,
                error: None,
            }],
        };
        let v = serde_json::to_value(&m).unwrap();
        let back: EvalRunManifest = serde_json::from_value(v).unwrap();
        assert_eq!(back.case_count, 2);
        assert_eq!(back.success_count, 2);
        assert_eq!(back.cases[0].status, EvalCaseStatus::Ok);
    }

    #[test]
    fn civil_from_days_anchor_2026_05_20() {
        // 56 years × 365 + 14 leap years + (31+28+31+30+19) day-of-year
        // = 20454 + 139 = 20593 days from 1970-01-01 to 2026-05-20.
        let (y, mo, d) = civil_from_days(20_593);
        assert_eq!((y, mo, d), (2026, 5, 20));
    }
}
