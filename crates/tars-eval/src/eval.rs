//! Runs the `tars eval` commands over a corpus and the run directories it produces: `run`
//! replays each case through a provider pipeline and scores it, `judge` grades a run with an LLM,
//! and `diff` compares two runs.
//!
//! The caller hands it a registry and a resolved provider; where the config lives is not its
//! business. Each command returns what it found as a report and writes nothing to the terminal:
//! how a report or a progress event reads is the caller's. A run directory holds outputs and
//! reports, and a copy of each case's `input.txt`, `system.txt` and `expected.txt`, so `judge`
//! grades a run without the corpus. A run is written only into a new or empty directory.
//!
//! ## Corpus directory layout
//!
//! ```text
//! corpus/
//!   case_001/
//!     input.txt            (required)
//!     system.txt           (optional system prompt for this case)
//!     expected.txt         (optional reference answer for `judge`)
//!     expected_tools.json  (optional reference tools for `--check trajectory-match`)
//!   case_002/
//!     ...
//! ```
//!
//! A subdirectory without `input.txt` is skipped with a warning; other files in a case are
//! ignored.
//!
//! ## Output layout
//!
//! ```text
//! <output>/                (the CLI's default: benchmarks/runs/eval/<timestamp>/, gitignored)
//!   manifest.json           — Manifest (the run's model, checks and cases)
//!   judge_report.json       — written by `judge`
//!   case_001/
//!     output.txt            — the response text
//!     report.json           — CaseReport (outcome, usage, wall clock)
//!     input.txt             — the corpus case's
//!     system.txt            — the corpus case's, when it has one
//!     expected.txt          — the corpus case's, when it has one
//!   case_002/
//!     ...
//! ```

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

use futures::StreamExt;
use serde::Serialize;

mod check_spec;
mod report;
mod run_dir;

pub use report::{
    CaseCheckResult, CaseFailure, CaseId, CaseOutcome, CaseReport, CheckSummary, MESSAGE_MAX_CHARS,
    Manifest, ParseCaseIdError,
};

pub use check_spec::{
    CheckSet, CheckSetError, CheckSpec, JudgeChoice, ParseCheckError, Threshold, TrajectoryCheck,
    TrajectoryScoring,
};

use crate::trajectory_match::{self, MatchMode, ToolStep};
use crate::{
    ArgEquivalenceJudge, CheckOutcome, DiscordantPairs, Invariant, JudgeFailure, JudgeReport,
    SharedProvider, SharedVendor, ValidatorInvariant, VendorCheck, args_match_judged,
    check_providers, check_vendors,
};
use check_spec::names_trajectory_check;
use run_dir::RunDir;
use tars_config::{MODEL_KB, Vendor};
use tars_llm::provider::registry::{ProviderRegistry, RegistryError};
use tars_llm::{
    ChainOptions, JsonShapeValidator, LlmService, MaxLengthValidator, NotEmptyValidator,
};
use tars_sandbox::SandboxPolicy;
use tars_types::{Caller, ChatRequest, ChatResponse, ChatResponseBuilder, ProviderId};
use tars_utils::display_chain;

// ─── errors ───────────────────────────────────────────────────────────

/// What was being done to a file when it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOp {
    Read,
    Write,
    CreateDir,
    List,
    Resolve,
}

impl fmt::Display for FileOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "reading",
            Self::Write => "writing",
            Self::CreateDir => "creating dir",
            Self::List => "listing",
            Self::Resolve => "resolving",
        })
    }
}

/// A corpus or run-directory file that could not be read, written or decoded.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{op} {}", path.display())]
    Io {
        op: FileOp,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("parsing {}", path.display())]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// A corpus directory that does not load as cases.
#[derive(Debug, thiserror::Error)]
pub enum LoadCorpusError {
    #[error("corpus path is not a directory: {}", path.display())]
    NotADirectory { path: PathBuf },
    #[error(
        "parsing {} (expect a JSON array of tool names or {{name,args}})",
        path.display()
    )]
    ExpectedTools {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    File(#[from] FileError),
}

/// Why [`run_eval`] stopped before or while writing the run; a case whose call fails is not one
/// of these, it is recorded in the run.
#[derive(Debug, thiserror::Error)]
pub enum RunEvalError {
    #[error("provider `{provider}`")]
    ProviderUnavailable {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    #[error("the model of provider `{provider}`")]
    ModelUnresolved {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    #[error(transparent)]
    Judge(#[from] JudgeSetupError),
    #[error("loading corpus from {}", corpus.display())]
    CorpusUnreadable {
        corpus: PathBuf,
        #[source]
        source: LoadCorpusError,
    },
    #[error(
        "corpus at {} contains no readable cases (need subdirectories with input.txt)",
        corpus.display()
    )]
    EmptyCorpus { corpus: PathBuf },
    #[error(
        "the output directory {} is not empty; a run is written only into a new or empty one",
        path.display()
    )]
    OutputNotEmpty { path: PathBuf },
    #[error("case `{case}` is named like a file the run directory keeps beside its cases")]
    CaseNamesARunFile { case: CaseId },
    #[error("the system clock reads before 1970")]
    ClockBeforeEpoch(#[source] SystemTimeError),
    #[error(transparent)]
    File(#[from] FileError),
}

/// Why a judge could not be set up to grade a run: `tars eval judge`'s judge and an `args-judge`
/// check's alike.
#[derive(Debug, thiserror::Error)]
pub enum JudgeSetupError {
    /// The refusal is part of the message rather than its source, so it is said once.
    #[error("{0}; pick a judge on another provider")]
    SharesRunProvider(SharedProvider),
    #[error("judge provider `{provider}`")]
    ProviderUnavailable {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    #[error("the model of judge provider `{provider}`")]
    ModelUnresolved {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    /// The refusal is part of the message rather than its source, so it is said once.
    #[error(
        "{refusal}: the run's `{run_provider}` model `{run_model}` and the judge's \
         `{judge_provider}` model `{judge_model}`; a judge grades its own vendor's output too \
         kindly, so pick a judge whose model is another vendor's"
    )]
    SharesRunVendor {
        refusal: SharedVendor,
        run_provider: ProviderId,
        run_model: String,
        judge_provider: ProviderId,
        judge_model: String,
    },
}

/// Why [`run_judge`] graded nothing or could not keep its report.
#[derive(Debug, thiserror::Error)]
pub enum JudgeRunError {
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Judge(#[from] JudgeSetupError),
    /// A file `tars eval run` writes into every successful case's directory is not there.
    #[error("{} is missing; the run directory is not one a whole run wrote", path.display())]
    CaseFileMissing { path: PathBuf },
    #[error("no successful cases in {} to judge", run.display())]
    NoSuccessfulCase { run: PathBuf },
}

// ─── fs helpers ────────────────────────────────────────────────────────

fn io_error(op: FileOp, path: &Path) -> impl FnOnce(io::Error) -> FileError + '_ {
    move |source| FileError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

fn write_pretty_json<T: Serialize>(path: &Path, value: &T) -> Result<(), FileError> {
    let body = serde_json::to_string_pretty(value).expect(
        "the reports written here are structs, enums, strings and numbers with no map keyed by \
         anything but a string and no hand-written Serialize, which serde_json always encodes",
    );
    write_text(path, body)
}

fn write_text(path: &Path, body: impl AsRef<[u8]>) -> Result<(), FileError> {
    fs::write(path, body).map_err(io_error(FileOp::Write, path))
}

fn ensure_dir(path: &Path) -> Result<(), FileError> {
    fs::create_dir_all(path).map_err(io_error(FileOp::CreateDir, path))
}

fn read_text(path: &Path) -> Result<String, FileError> {
    fs::read_to_string(path).map_err(io_error(FileOp::Read, path))
}

/// Reads a file that may be absent: `Ok(None)` when it does not exist, an error for any other
/// I/O failure.
fn read_optional_text(path: &Path) -> Result<Option<String>, FileError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(FileOp::Read, path)(e)),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, body: &str) -> Result<T, FileError> {
    serde_json::from_str(body).map_err(|source| FileError::Decode {
        path: path.to_path_buf(),
        source,
    })
}

// ─── configs and reports ──────────────────────────────────────────────

/// Says what [`run_eval`] replays and how; the caller has already resolved the provider.
pub struct RunRequest {
    /// Provider registry the run's pipeline (and any judge) is drawn from.
    pub registry: Arc<ProviderRegistry>,
    pub provider_id: ProviderId,
    /// Who the run's model calls (and any judge's) are made for.
    pub caller: Caller,
    /// The model spec: a concrete id or `<series>@latest`, resolved against the provider's
    /// catalog. `None` → the provider's default model.
    pub model: Option<String>,
    pub corpus: PathBuf,
    /// The run directory the outputs and reports are written to: a new or empty directory,
    /// created when missing.
    pub output: PathBuf,
    pub max_output_tokens: Option<u32>,
    /// The checks every case is scored by, with the `args-judge` check's judge, which is refused
    /// when it is `provider_id`; its model is resolved as `model` is.
    pub checks: CheckSet,
    /// How long one case may take, its model call and any arg-judge calls together; a case past
    /// it is recorded as failed there and the run goes on. `None` waits as long as a call takes.
    pub case_deadline: Option<Duration>,
}

/// Says which run [`run_judge`] grades and with which provider.
pub struct JudgeRequest {
    /// Provider registry the judge is drawn from.
    pub registry: Arc<ProviderRegistry>,
    /// Who the judge's model calls are made for.
    pub caller: Caller,
    /// Eval-run directory (contains manifest.json + per-case output.txt).
    pub run: PathBuf,
    /// The provider to judge with, refused when it produced the run.
    pub judge: ProviderId,
    /// The judge's model spec, resolved against its catalog. `None` → the provider's default
    /// model.
    pub judge_model: Option<String>,
}

/// Says which two runs [`run_diff`] compares.
pub struct DiffRequest {
    pub baseline: PathBuf,
    pub candidate: PathBuf,
    /// The similarity mode of the tool-trajectory comparison; `None` leaves it out.
    pub trajectory: Option<MatchMode>,
}

/// One step of [`run_eval`], said as it happens so a caller can show progress.
#[derive(Clone, Copy, Debug)]
pub enum RunProgress<'a> {
    /// The corpus loaded and the run is about to start its first case.
    Started {
        case_count: usize,
        provider: &'a ProviderId,
        model: &'a str,
    },
    /// A case's call is about to be made.
    CaseStarted { case_id: &'a str },
    /// A case's call ended and its files are written.
    CaseFinished { report: &'a CaseReport },
}

/// A finished `tars eval run`: the manifest it wrote and where.
#[derive(Clone, Debug)]
pub struct FinishedRun {
    pub manifest: Manifest,
    pub manifest_path: PathBuf,
}

/// The judge pass of [`run_judge`] is about to start.
#[derive(Clone, Copy, Debug)]
pub struct JudgingStarted<'a> {
    pub case_count: usize,
    /// The provider that produced the run, which the judge was checked not to be.
    pub run_provider: &'a ProviderId,
    pub vendors: &'a VendorCheck,
}

/// A finished `tars eval judge`: the report it wrote and where.
#[derive(Clone, Debug)]
pub struct JudgedRun {
    pub report: JudgeReport,
    pub report_path: PathBuf,
    pub vendors: VendorCheck,
}

/// Two runs compared by [`run_diff`].
#[derive(Clone, Debug)]
pub struct RunDiff {
    pub baseline: ComparedRun,
    pub candidate: ComparedRun,
    /// Every check either run ran: the candidate's in its order, then those only the baseline ran.
    pub checks: Vec<CheckDelta>,
    pub trajectory: Option<TrajectoryDiff>,
    /// Present when both runs were judged.
    pub quality: Option<Quality>,
}

/// One side of an [`RunDiff`].
#[derive(Clone, Debug)]
pub struct ComparedRun {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// The median case wall clock, 0 for a run with no case.
    pub latency_p50_ms: u64,
}

/// One check's violation rate in the two runs of an [`RunDiff`].
#[derive(Clone, Debug, PartialEq)]
pub struct CheckDelta {
    pub name: String,
    pub rates: CheckRates,
}

/// Which runs ran a check, with its violation rate in each; a rate is `None` when the check
/// decided no case of that run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CheckRates {
    Both {
        baseline: Option<f64>,
        candidate: Option<f64>,
    },
    /// Only the candidate ran it.
    New { candidate: Option<f64> },
    /// Only the baseline ran it.
    Dropped { baseline: Option<f64> },
}

/// Head-to-head tool-trajectory comparison of two runs, paired by case id.
#[derive(Clone, Debug)]
pub struct TrajectoryDiff {
    pub mode: MatchMode,
    pub paired: u32,
    pub a_only: u32,
    pub b_only: u32,
    pub divergent: u32,
    /// 1.0 when no case is paired.
    pub mean_similarity: f64,
    pub diverging_ids: Vec<String>,
    /// One per `trajectory-match*` check both runs ran.
    pub mcnemar: Vec<CheckMcNemar>,
}

impl TrajectoryDiff {
    /// Returns the share of paired cases whose trajectories differ, 0.0 when none is paired.
    pub fn divergence_rate(&self) -> f64 {
        if self.paired == 0 {
            0.0
        } else {
            f64::from(self.divergent) / f64::from(self.paired)
        }
    }
}

/// McNemar's test of one check over the cases both runs ran it on.
#[derive(Clone, Debug)]
pub struct CheckMcNemar {
    pub check: String,
    pub result: DiscordantPairs,
}

/// How the judge reports of the two runs of an [`RunDiff`] compare.
#[derive(Clone, Debug)]
pub enum Quality {
    /// One judge graded both runs, so its verdicts pair item by item.
    Compared(QualityDiff),
    /// Two judges graded the runs: a change in their verdicts may be the judges', not the runs',
    /// so the verdicts are not paired.
    JudgesDiffer {
        baseline: JudgeReport,
        candidate: JudgeReport,
    },
}

/// The judge reports of both runs of an [`RunDiff`] and McNemar's test over the items they share.
#[derive(Clone, Debug)]
pub struct QualityDiff {
    pub baseline: JudgeReport,
    pub candidate: JudgeReport,
    /// Pairs the items both reports decided: a correct verdict counts as a pass, and an item
    /// either judge was unsure of or failed on takes no part.
    pub paired: DiscordantPairs,
}

/// Builds the invariant of a check that scores the response alone; `None` for a trajectory check,
/// which needs the case's reference tools as well.
fn invariant_of(spec: &CheckSpec) -> Option<Arc<dyn Invariant>> {
    let validator: Arc<dyn tars_llm::OutputValidator> = match spec {
        CheckSpec::NonEmpty => Arc::new(NotEmptyValidator::new()),
        CheckSpec::ValidJson => Arc::new(JsonShapeValidator::new()),
        CheckSpec::MaxLength { max_chars } => {
            Arc::new(MaxLengthValidator::reject_above(*max_chars))
        }
        CheckSpec::Trajectory(_) => return None,
    };
    Some(Arc::new(ValidatorInvariant::new(validator)))
}

/// How a trajectory check of a run scores a case.
#[derive(Clone, Copy)]
enum Scorer<'a> {
    Match(MatchMode),
    ArgsJudged(&'a ArgEquivalenceJudge),
}

/// A `trajectory-match` check of a run. It needs each case's `expected_tools`, which an
/// [`Invariant`]'s `(req, resp)` signature cannot carry, so it runs beside the invariants.
struct TrajectorySpec<'a> {
    /// The spec's canonical writing, the check's name.
    name: String,
    scorer: Scorer<'a>,
    threshold: f64,
}

impl TrajectorySpec<'_> {
    /// Scores one case; `None` when the case has no `expected_tools`, which leaves it out of the
    /// check's `evaluated` count.
    async fn eval(
        &self,
        actual: &[ToolStep],
        expected: Option<&[ToolStep]>,
    ) -> Option<CaseCheckResult> {
        match self.scorer {
            Scorer::Match(mode) => self.eval_case(mode, actual, expected),
            Scorer::ArgsJudged(judge) => self.eval_case_judged(actual, expected, judge).await,
        }
    }

    fn eval_case(
        &self,
        mode: MatchMode,
        actual: &[ToolStep],
        expected: Option<&[ToolStep]>,
    ) -> Option<CaseCheckResult> {
        let expected = expected?;
        let score = trajectory_match::score(actual, expected, mode);
        Some(self.verdict(score, mode.as_str(), actual, expected))
    }

    /// Scores one case like `args` mode, asking the judge whether byte-different arguments are
    /// equivalent; a case the judge fails on is recorded as not judged, which says nothing about
    /// the model.
    async fn eval_case_judged(
        &self,
        actual: &[ToolStep],
        expected: Option<&[ToolStep]>,
        judge: &ArgEquivalenceJudge,
    ) -> Option<CaseCheckResult> {
        let expected = expected?;
        Some(match args_match_judged(actual, expected, judge).await {
            Ok(score) => self.verdict(score, "args-judge", actual, expected),
            Err(e) => CaseCheckResult::NotJudged {
                name: self.name.clone(),
                failure: e.failure(),
            },
        })
    }

    /// Records a case this check could not score before the case's deadline passed.
    fn past_deadline(&self, passed: DeadlinePassed) -> CaseCheckResult {
        CaseCheckResult::NotJudged {
            name: self.name.clone(),
            failure: JudgeFailure::DeadlinePassed {
                deadline_ms: passed.deadline_ms(),
            },
        }
    }

    /// Passes a case whose `score` reaches the threshold, naming the tools on both sides.
    fn verdict(
        &self,
        score: f64,
        scoring: &str,
        actual: &[ToolStep],
        expected: &[ToolStep],
    ) -> CaseCheckResult {
        let got: Vec<&str> = actual.iter().map(|x| x.name.as_str()).collect();
        if score >= self.threshold {
            CaseCheckResult::Passed {
                name: self.name.clone(),
                note: Some(format!("score={score:.3} ({scoring}) got={got:?}")),
            }
        } else {
            let want: Vec<&str> = expected.iter().map(|x| x.name.as_str()).collect();
            CaseCheckResult::Failed {
                name: self.name.clone(),
                reason: format!(
                    "score={score:.3} < {:.3} ({scoring}); want={want:?} got={got:?}",
                    self.threshold
                ),
            }
        }
    }
}

// ─── eval judge ───────────────────────────────────────────────────────

/// Runs `tars eval judge <run>`: grades each successful case's output with the judge provider
/// and writes `judge_report.json` into the run directory. `on_start` is called once, just before
/// the judge's first call.
///
/// # Errors
///
/// Fails when the run's manifest or a case's files cannot be read, when the judge provider is the
/// one that produced the run or is not in the registry, when the judge's model and the run's are
/// known to be one vendor's, when the judge's model does not resolve
/// (a blank `--judge-model`, an unknown series, a registry not built from config), when the run
/// has no successful case, when the judge pass fails, and when the report cannot be written.
pub async fn run_judge(
    cfg: JudgeRequest,
    on_start: impl FnOnce(JudgingStarted<'_>),
) -> Result<JudgedRun, JudgeRunError> {
    use crate::{JudgeItem, LlmJudge, run_judge_pass};

    let run_dir = RunDir::open(cfg.run);
    let manifest = run_dir.read_manifest()?;
    let judging = set_up_judge(
        &cfg.registry,
        &cfg.judge,
        cfg.judge_model.as_deref(),
        &RunModel {
            provider: &manifest.provider_id,
            model: &manifest.model,
            vendor: manifest.vendor.as_ref(),
        },
    )?;
    let judge = LlmJudge::new(judging.service, judging.id, judge_caller(&cfg.caller));
    let vendors = judging.vendors;

    let items: Vec<JudgeItem> = manifest
        .cases
        .iter()
        .filter(|case| case.succeeded())
        .map(|case| run_dir.read_judge_item(&case.case_id))
        .collect::<Result<_, _>>()?;
    if items.is_empty() {
        return Err(JudgeRunError::NoSuccessfulCase {
            run: run_dir.path().to_path_buf(),
        });
    }

    on_start(JudgingStarted {
        case_count: items.len(),
        run_provider: &manifest.provider_id,
        vendors: &vendors,
    });
    let report = run_judge_pass(items, &judge).await;

    let report_path = run_dir.write_judge_report(&report, &vendors)?;
    Ok(JudgedRun {
        report,
        report_path,
        vendors,
    })
}

/// The model a run's calls were bound to, which a judge of the run must not share a provider or
/// vendor with.
struct RunModel<'a> {
    provider: &'a ProviderId,
    model: &'a str,
    vendor: Option<&'a Vendor>,
}

/// A judge ready to grade a run: the chain its calls go through, its `provider:model` name, and
/// what was known of its vendor against the run's.
struct Judging {
    service: LlmService,
    id: String,
    vendors: VendorCheck,
}

/// Builds the judge `judge` (with the model `model` names, else its default) for a run of `run`,
/// refusing it when it is the run's provider or its model the run's vendor's.
///
/// # Errors
///
/// [`JudgeSetupError`] when the judge is the run's provider, is not in the registry, its model
/// does not resolve, or its model is known to be the run's vendor's.
fn set_up_judge(
    registry: &ProviderRegistry,
    judge: &ProviderId,
    model: Option<&str>,
    run: &RunModel<'_>,
) -> Result<Judging, JudgeSetupError> {
    check_providers(run.provider, judge).map_err(JudgeSetupError::SharesRunProvider)?;
    let provider = registry
        .get(judge)
        .map_err(|source| JudgeSetupError::ProviderUnavailable {
            provider: judge.clone(),
            source,
        })?;
    let judge_model = registry.resolve_model(judge, model).map_err(|source| {
        JudgeSetupError::ModelUnresolved {
            provider: judge.clone(),
            source,
        }
    })?;
    let vendors = check_vendors(
        run.vendor.cloned(),
        MODEL_KB.vendor_of(&judge_model).cloned(),
    )
    .map_err(|refusal| JudgeSetupError::SharesRunVendor {
        refusal,
        run_provider: run.provider.clone(),
        run_model: run.model.to_string(),
        judge_provider: judge.clone(),
        judge_model: judge_model.clone(),
    })?;
    Ok(Judging {
        id: format!("{judge}:{judge_model}"),
        service: LlmService::default_chain(provider, judge_model, ChainOptions::default()),
        vendors,
    })
}

/// Returns the context a judge's calls are made in: the run's caller, confined read-only. A judge
/// reads text the evaluated model wrote, so a judge that is a CLI delegate with tools of its own
/// must not be able to write or run what that text tells it to; the caller's network setting is
/// kept, since a delegate reaches its model over the network.
fn judge_caller(caller: &Caller) -> Caller {
    caller
        .clone()
        .with_sandbox(SandboxPolicy::read_only(caller.sandbox.network()))
}

// ─── eval diff ────────────────────────────────────────────────────────

fn p50(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

/// Maps each case `check_name` decided on to whether it passed; a case its judge could not
/// decide is left out.
fn case_check_passmap(m: &Manifest, check_name: &str) -> std::collections::BTreeMap<String, bool> {
    m.cases
        .iter()
        .filter_map(|c| {
            c.checks()
                .iter()
                .find(|cc| cc.name() == check_name)
                .and_then(CaseCheckResult::decided)
                .map(|passed| (c.case_id.to_string(), passed))
        })
        .collect()
}

fn compute_traj_diff(a: &Manifest, b: &Manifest, mode: MatchMode) -> TrajectoryDiff {
    use std::collections::{BTreeMap, BTreeSet};
    let amap: BTreeMap<&str, &CaseReport> =
        a.cases.iter().map(|c| (c.case_id.as_str(), c)).collect();
    let bmap: BTreeMap<&str, &CaseReport> =
        b.cases.iter().map(|c| (c.case_id.as_str(), c)).collect();

    let (mut paired, mut divergent, mut sum_sim) = (0u32, 0u32, 0.0f64);
    let mut diverging_ids = Vec::new();
    for (id, ca) in &amap {
        let Some(cb) = bmap.get(id) else { continue };
        paired += 1;
        let an: Vec<&str> = ca.tool_trajectory.iter().map(String::as_str).collect();
        let bn: Vec<&str> = cb.tool_trajectory.iter().map(String::as_str).collect();
        let sim = trajectory_match::score_names(&an, &bn, mode);
        sum_sim += sim;
        if sim < 1.0 {
            divergent += 1;
            diverging_ids.push((*id).to_string());
        }
    }
    let a_only = amap.keys().filter(|k| !bmap.contains_key(*k)).count() as u32;
    let b_only = bmap.keys().filter(|k| !amap.contains_key(*k)).count() as u32;
    let mean_similarity = if paired == 0 {
        1.0
    } else {
        sum_sim / paired as f64
    };

    let a_traj: BTreeSet<&str> = a
        .checks
        .iter()
        .map(String::as_str)
        .filter(|n| names_trajectory_check(n))
        .collect();
    let mcnemar = b
        .checks
        .iter()
        .map(String::as_str)
        .filter(|n| names_trajectory_check(n) && a_traj.contains(n))
        .map(|name| CheckMcNemar {
            check: name.to_string(),
            result: crate::mcnemar(&case_check_passmap(a, name), &case_check_passmap(b, name)),
        })
        .collect();

    TrajectoryDiff {
        mode,
        paired,
        a_only,
        b_only,
        divergent,
        mean_similarity,
        diverging_ids,
        mcnemar,
    }
}

/// Lists every check either run ran, the candidate's in its order first, each with the baseline's
/// rate for a check of the same name.
fn check_deltas(a: &Manifest, b: &Manifest) -> Vec<CheckDelta> {
    let (a_checks, b_checks) = (a.check_summaries(), b.check_summaries());
    let rate_in = |checks: &[CheckSummary], name: &str| {
        checks
            .iter()
            .find(|c| c.name == name)
            .map(CheckSummary::violation_rate)
    };
    let candidate = b_checks.iter().map(|bc| CheckDelta {
        name: bc.name.clone(),
        rates: match rate_in(&a_checks, &bc.name) {
            Some(baseline) => CheckRates::Both {
                baseline,
                candidate: bc.violation_rate(),
            },
            None => CheckRates::New {
                candidate: bc.violation_rate(),
            },
        },
    });
    let dropped = a_checks
        .iter()
        .filter(|ac| rate_in(&b_checks, &ac.name).is_none())
        .map(|ac| CheckDelta {
            name: ac.name.clone(),
            rates: CheckRates::Dropped {
                baseline: ac.violation_rate(),
            },
        });
    candidate.chain(dropped).collect()
}

/// Pairs two judge reports by item id when one judge wrote both, over the items it decided.
fn quality(baseline: JudgeReport, candidate: JudgeReport) -> Quality {
    if baseline.judge_id != candidate.judge_id {
        return Quality::JudgesDiffer {
            baseline,
            candidate,
        };
    }
    let decided = |r: &JudgeReport| {
        r.verdicts
            .iter()
            .filter_map(|v| v.verdict.decided().map(|pass| (v.item_id.clone(), pass)))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let paired = crate::mcnemar(&decided(&baseline), &decided(&candidate));
    Quality::Compared(QualityDiff {
        baseline,
        candidate,
        paired,
    })
}

fn compared_run(dir: PathBuf) -> Result<ComparedRun, FileError> {
    let manifest = RunDir::open(dir.clone()).read_manifest()?;
    let latency_p50_ms = p50(manifest.cases.iter().map(|c| c.wall_clock_ms).collect());
    Ok(ComparedRun {
        dir,
        manifest,
        latency_p50_ms,
    })
}

/// Runs `tars eval diff`: compares the operational totals and check violation rates of two runs,
/// optionally their tool trajectories, and their judge reports when both were judged.
///
/// # Errors
///
/// Fails when either run's manifest cannot be read or parsed, and when a run's
/// `judge_report.json` is there but cannot be read or parsed.
pub fn run_diff(cfg: DiffRequest) -> Result<RunDiff, FileError> {
    let baseline = compared_run(cfg.baseline)?;
    let candidate = compared_run(cfg.candidate)?;
    let (a, b) = (&baseline.manifest, &candidate.manifest);

    let checks = check_deltas(a, b);
    let trajectory = cfg.trajectory.map(|mode| compute_traj_diff(a, b, mode));
    let quality = match (
        RunDir::open(baseline.dir.clone()).read_judge_report()?,
        RunDir::open(candidate.dir.clone()).read_judge_report()?,
    ) {
        (Some(ja), Some(jb)) => Some(quality(ja, jb)),
        _ => None,
    };

    Ok(RunDiff {
        baseline,
        candidate,
        checks,
        trajectory,
        quality,
    })
}

/// Runs `tars eval run`: replays every corpus case through the provider's default chain,
/// scores it with the run's checks, and writes per-case outputs and reports plus `manifest.json`.
/// `on_progress` is called as the run starts and around each case.
///
/// A case whose call fails is recorded as an error and the run goes on.
///
/// # Errors
///
/// Fails when the provider is not in the registry, when the `args-judge` check's judge cannot be
/// set up (the run's own provider, a model of the run's vendor, not in the registry), when a model
/// does not resolve (a blank model spec, an unknown series, no model for a provider not built from
/// config), on an empty or unreadable corpus or one with a case named like a run file, when the
/// output directory is not empty, and when an artifact cannot be written. All of these but a
/// failed write come before the first call.
pub async fn run_eval(
    cfg: RunRequest,
    mut on_progress: impl FnMut(RunProgress<'_>),
) -> Result<FinishedRun, RunEvalError> {
    let provider_id = cfg.provider_id.clone();
    let provider =
        cfg.registry
            .get(&provider_id)
            .map_err(|source| RunEvalError::ProviderUnavailable {
                provider: provider_id.clone(),
                source,
            })?;

    let model = cfg
        .registry
        .resolve_model(&provider_id, cfg.model.as_deref())
        .map_err(|source| RunEvalError::ModelUnresolved {
            provider: provider_id.clone(),
            source,
        })?;
    let vendor = MODEL_KB.vendor_of(&model).cloned();
    let pipeline = LlmService::default_chain(provider, model.clone(), ChainOptions::default());

    let (arg_judge, args_judge_vendors) = match cfg.checks.args_judge() {
        Some(choice) => {
            let judging = set_up_judge(
                &cfg.registry,
                &choice.provider,
                choice.model.as_deref(),
                &RunModel {
                    provider: &provider_id,
                    model: &model,
                    vendor: vendor.as_ref(),
                },
            )?;
            let judge =
                ArgEquivalenceJudge::new(judging.service, judging.id, judge_caller(&cfg.caller));
            (Some(judge), Some(judging.vendors))
        }
        None => (None, None),
    };
    let checks = RunChecks::of(&cfg.checks, arg_judge.as_ref());
    let check_names: Vec<String> = cfg
        .checks
        .checks()
        .iter()
        .map(CheckSpec::to_string)
        .collect();

    let cases = load_corpus(&cfg.corpus).map_err(|source| RunEvalError::CorpusUnreadable {
        corpus: cfg.corpus.clone(),
        source,
    })?;
    if cases.is_empty() {
        return Err(RunEvalError::EmptyCorpus { corpus: cfg.corpus });
    }
    let corpus_path =
        cfg.corpus
            .canonicalize()
            .map_err(|source| RunEvalError::CorpusUnreadable {
                corpus: cfg.corpus.clone(),
                source: io_error(FileOp::Resolve, &cfg.corpus)(source).into(),
            })?;
    if let Some(case) = cases.iter().find(|c| run_dir::takes_a_run_file(&c.id)) {
        return Err(RunEvalError::CaseNamesARunFile {
            case: case.id.clone(),
        });
    }
    let run_dir = RunDir::create(cfg.output)?;
    on_progress(RunProgress::Started {
        case_count: cases.len(),
        provider: &provider_id,
        model: &model,
    });

    // A failed case is recorded, not propagated: an eval is worth its whole distribution.
    let started_at_ms = utc_now_millis()?;
    let mut reports: Vec<CaseReport> = Vec::with_capacity(cases.len());

    for case in &cases {
        on_progress(RunProgress::CaseStarted {
            case_id: case.id.as_str(),
        });
        let run = run_one_case(
            &pipeline,
            &cfg.caller,
            case,
            cfg.max_output_tokens,
            &checks,
            cfg.case_deadline,
        )
        .await;
        run_dir.write_case(case, &run)?;

        on_progress(RunProgress::CaseFinished {
            report: &run.summary,
        });
        reports.push(run.summary);
    }

    let ended_at_ms = utc_now_millis()?;
    let manifest = Manifest {
        corpus_path: corpus_path.display().to_string(),
        provider_id,
        model,
        vendor,
        args_judge_vendors,
        started_at_ms,
        ended_at_ms,
        checks: check_names,
        cases: reports,
    };
    let manifest_path = run_dir.write_manifest(&manifest)?;
    Ok(FinishedRun {
        manifest,
        manifest_path,
    })
}

// ─── one case ─────────────────────────────────────────────────────────

const INPUT_FILE: &str = "input.txt";
const SYSTEM_FILE: &str = "system.txt";
const EXPECTED_FILE: &str = "expected.txt";

/// One case loaded from the corpus.
struct Case {
    id: CaseId,
    input: String,
    system: Option<String>,
    expected: Option<String>,
    /// The reference tools for `--check trajectory-match`; `None` when the case has no
    /// `expected_tools.json`, and then the check skips the case rather than passing it.
    expected_tools: Option<Vec<ToolStep>>,
}

/// One entry of `expected_tools.json`: a bare tool name, or a `{name, args}` object whose args
/// the `args` and `args-judge` modes compare.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ExpectedToolEntry {
    Name(String),
    Step {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
}

/// Reads `expected_tools.json` if present.
///
/// # Errors
///
/// Fails, naming the file, when it is not valid JSON; a malformed file is never skipped.
fn read_expected_tools(path: &Path) -> Result<Option<Vec<ToolStep>>, LoadCorpusError> {
    let Some(raw) = read_optional_text(path)? else {
        return Ok(None);
    };
    let entries: Vec<ExpectedToolEntry> =
        serde_json::from_str(&raw).map_err(|source| LoadCorpusError::ExpectedTools {
            path: path.to_path_buf(),
            source,
        })?;
    let steps = entries
        .into_iter()
        .map(|e| match e {
            ExpectedToolEntry::Name(name) => ToolStep {
                name,
                args: serde_json::Value::Null,
            },
            ExpectedToolEntry::Step { name, args } => ToolStep { name, args },
        })
        .collect();
    Ok(Some(steps))
}

/// A case's report and the text its call received.
struct CaseRun {
    summary: CaseReport,
    output_text: String,
}

/// Builds a case's request; the model is bound on the pipeline, not here.
fn build_case_request(case: &Case, max_output_tokens: Option<u32>) -> ChatRequest {
    let mut req = ChatRequest::user(case.input.clone());
    if let Some(sys) = &case.system {
        req = req.with_system(sys.clone());
    }
    if let Some(cap) = max_output_tokens {
        req.max_output_tokens = Some(cap);
    }
    req
}

/// What one case's model call received.
struct CaseCall {
    response: ChatResponse,
    /// The tool calls the model asked for.
    tool_steps: Vec<ToolStep>,
    /// Why the call did not complete, if it did not.
    failure: Option<CaseFailure>,
}

/// Makes one completion and returns what it received.
async fn run_completion_case(
    pipeline: &LlmService,
    req: ChatRequest,
    ctx: Caller,
    deadline: Option<Deadline>,
) -> CaseCall {
    let mut acc = ChatResponseBuilder::new();
    let failure = match within(deadline, pipeline.call(req, ctx))
        .await
        .map(|c| c.result)
    {
        Err(passed) => Some(passed.case_failure()),
        Ok(Err(e)) => Some(CaseFailure::call(e.kind(), &display_chain(&e))),
        Ok(Ok(mut stream)) => loop {
            match within(deadline, stream.next()).await {
                Err(passed) => break Some(passed.case_failure()),
                Ok(None) => break None,
                Ok(Some(Ok(ev))) => acc.apply(ev),
                Ok(Some(Err(e))) => break Some(CaseFailure::call(e.kind(), &display_chain(&e))),
            }
        },
    };
    let response = acc.finish();
    let tool_steps = trajectory_match::from_tool_calls(&response.tool_calls);
    CaseCall {
        response,
        tool_steps,
        failure,
    }
}

/// When a case must be done by, and how long it was given.
#[derive(Clone, Copy)]
struct Deadline {
    at: tokio::time::Instant,
    allowed: Duration,
}

impl Deadline {
    fn after(allowed: Duration) -> Self {
        Self {
            at: tokio::time::Instant::now() + allowed,
            allowed,
        }
    }
}

/// A case's deadline passed before what it waited on finished.
#[derive(Clone, Copy, Debug)]
struct DeadlinePassed {
    allowed: Duration,
}

impl DeadlinePassed {
    fn deadline_ms(self) -> u64 {
        u64::try_from(self.allowed.as_millis()).unwrap_or(u64::MAX)
    }

    fn case_failure(self) -> CaseFailure {
        CaseFailure::DeadlinePassed {
            deadline_ms: self.deadline_ms(),
        }
    }
}

/// Awaits `work` until `deadline`, or for as long as it takes when there is none.
async fn within<F: std::future::Future>(
    deadline: Option<Deadline>,
    work: F,
) -> Result<F::Output, DeadlinePassed> {
    match deadline {
        None => Ok(work.await),
        Some(d) => tokio::time::timeout_at(d.at, work)
            .await
            .map_err(|_elapsed| DeadlinePassed { allowed: d.allowed }),
    }
}

/// The checks of one run, ready to score its cases: each response check with its name, and each
/// trajectory check with its scorer.
struct RunChecks<'a> {
    invariants: Vec<(String, Arc<dyn Invariant>)>,
    trajectory: Vec<TrajectorySpec<'a>>,
}

impl<'a> RunChecks<'a> {
    /// Readies `set`'s checks, scoring an `args-judge` check with `arg_judge`, the judge built
    /// from `set`'s.
    fn of(set: &CheckSet, arg_judge: Option<&'a ArgEquivalenceJudge>) -> Self {
        let mut invariants = Vec::new();
        let mut trajectory = Vec::new();
        for spec in set.checks() {
            let name = spec.to_string();
            match spec {
                CheckSpec::Trajectory(t) => trajectory.push(TrajectorySpec {
                    name,
                    scorer: match t.scoring {
                        TrajectoryScoring::Match(mode) => Scorer::Match(mode),
                        TrajectoryScoring::ArgsJudged => Scorer::ArgsJudged(arg_judge.expect(
                            "a CheckSet holds an args-judge check only with its judge, and \
                             run_eval builds arg_judge from that judge",
                        )),
                    },
                    threshold: t.threshold.get(),
                }),
                _ => {
                    if let Some(invariant) = invariant_of(spec) {
                        invariants.push((name, invariant));
                    }
                }
            }
        }
        Self {
            invariants,
            trajectory,
        }
    }
}

async fn run_one_case(
    pipeline: &LlmService,
    caller: &Caller,
    case: &Case,
    max_output_tokens: Option<u32>,
    checks: &RunChecks<'_>,
    case_deadline: Option<Duration>,
) -> CaseRun {
    let started = Instant::now();
    let deadline = case_deadline.map(Deadline::after);
    let req = build_case_request(case, max_output_tokens);

    let call = run_completion_case(pipeline, req.clone(), caller.for_call(), deadline).await;
    // A call that did not complete has no output to check.
    let outcome = match call.failure {
        Some(failure) => CaseOutcome::Error { failure },
        None => {
            let mut results = response_checks(&checks.invariants, &req, &call.response);
            let expected = case.expected_tools.as_deref();
            for ts in &checks.trajectory {
                let scored = match within(deadline, ts.eval(&call.tool_steps, expected)).await {
                    Ok(scored) => scored,
                    Err(passed) => Some(ts.past_deadline(passed)),
                };
                results.extend(scored);
            }
            CaseOutcome::Ok { checks: results }
        }
    };

    case_run(
        case.id.clone(),
        started.elapsed(),
        call.response,
        call.tool_steps,
        outcome,
    )
}

/// Scores a completed response by each check that reads the request and the response alone.
fn response_checks(
    invariants: &[(String, Arc<dyn Invariant>)],
    req: &ChatRequest,
    response: &ChatResponse,
) -> Vec<CaseCheckResult> {
    invariants
        .iter()
        .map(|(name, invariant)| case_check(name, invariant.check(req, response)))
        .collect()
}

/// Assembles a case's report and the text its call received.
fn case_run(
    case_id: CaseId,
    wall_clock: Duration,
    response: ChatResponse,
    tool_steps: Vec<ToolStep>,
    outcome: CaseOutcome,
) -> CaseRun {
    let output_text = response.text;
    CaseRun {
        summary: CaseReport {
            case_id,
            wall_clock_ms: wall_clock.as_millis() as u64,
            usage: response.usage,
            output_chars: output_text.chars().count() as u64,
            tool_trajectory: tool_steps.into_iter().map(|s| s.name).collect(),
            outcome,
        },
        output_text,
    }
}

/// Records an invariant's outcome on a case under the check's name.
fn case_check(name: &str, outcome: CheckOutcome) -> CaseCheckResult {
    let name = name.to_string();
    match outcome {
        CheckOutcome::Held { note } => CaseCheckResult::Passed { name, note },
        CheckOutcome::Violated { reason } => CaseCheckResult::Failed { name, reason },
    }
}

// ─── corpus loader ────────────────────────────────────────────────────

fn load_corpus(root: &Path) -> Result<Vec<Case>, LoadCorpusError> {
    if !root.is_dir() {
        return Err(LoadCorpusError::NotADirectory {
            path: root.to_path_buf(),
        });
    }
    let mut cases: Vec<Case> = Vec::new();
    for entry in fs::read_dir(root).map_err(io_error(FileOp::List, root))? {
        let entry = entry.map_err(io_error(FileOp::List, root))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let id = CaseId::try_from(name).expect(
            "a directory entry's file name is one path segment, and `.`-led names are skipped above",
        );
        let Some(input) = read_optional_text(&path.join(INPUT_FILE))? else {
            tracing::warn!(case = %id, "skipping case: no input.txt");
            continue;
        };
        let system = read_optional_text(&path.join(SYSTEM_FILE))?;
        let expected = read_optional_text(&path.join(EXPECTED_FILE))?;
        let expected_tools = read_expected_tools(&path.join("expected_tools.json"))?;
        cases.push(Case {
            id,
            input,
            system,
            expected,
            expected_tools,
        });
    }
    // Cases run in lexical id order; zero-padded ids sort numerically.
    cases.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(cases)
}

// ─── helpers ──────────────────────────────────────────────────────────

fn utc_now_millis() -> Result<u64, RunEvalError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(RunEvalError::ClockBeforeEpoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::Usage;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn tsteps(names: &[&str]) -> Vec<ToolStep> {
        names
            .iter()
            .map(|n| ToolStep {
                name: (*n).to_string(),
                args: serde_json::Value::Null,
            })
            .collect()
    }

    // ── trajectory-match: spec parsing ──────────────────

    fn trajectory(spec: &str) -> TrajectoryCheck {
        match spec.parse::<CheckSpec>().unwrap() {
            CheckSpec::Trajectory(t) => t,
            other => panic!("{spec} is not a trajectory check: {other:?}"),
        }
    }

    /// Readies the trajectory check `spec` with a name-and-argument scorer.
    fn traj(spec: &str) -> TrajectorySpec<'static> {
        let t = trajectory(spec);
        let TrajectoryScoring::Match(mode) = t.scoring else {
            panic!("{spec} needs a judge");
        };
        TrajectorySpec {
            name: CheckSpec::Trajectory(t).to_string(),
            scorer: Scorer::Match(mode),
            threshold: t.threshold.get(),
        }
    }

    fn eval_now(
        spec: &TrajectorySpec<'_>,
        actual: &[ToolStep],
        expected: Option<&[ToolStep]>,
    ) -> Option<CaseCheckResult> {
        let Scorer::Match(mode) = spec.scorer else {
            panic!("a judged check is scored asynchronously");
        };
        spec.eval_case(mode, actual, expected)
    }

    #[test]
    fn trajectory_spec_parses_modes_threshold_and_bare() {
        assert!(!matches!(
            "valid-json".parse::<CheckSpec>().unwrap(),
            CheckSpec::Trajectory(_)
        ));

        let bare = trajectory("trajectory-match");
        assert_eq!(bare.scoring, TrajectoryScoring::Match(MatchMode::Ordered));
        assert_eq!(bare.threshold.get(), 1.0);
        assert_eq!(traj("trajectory-match").name, "trajectory-match");

        let exact = trajectory("trajectory-match:exact");
        assert_eq!(exact.scoring, TrajectoryScoring::Match(MatchMode::Exact));

        let thr = trajectory("trajectory-match:ordered:0.8");
        assert_eq!(thr.scoring, TrajectoryScoring::Match(MatchMode::Ordered));
        assert!((thr.threshold.get() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn trajectory_spec_rejects_unknown_mode_and_bad_threshold() {
        assert!("trajectory-match:fuzzy".parse::<CheckSpec>().is_err());
        assert!(
            "trajectory-match:exact:notanum"
                .parse::<CheckSpec>()
                .is_err()
        );
    }

    #[test]
    fn a_threshold_outside_the_score_range_is_refused_and_its_ends_are_kept() {
        assert!("trajectory-match:exact:1.01".parse::<CheckSpec>().is_err());
        assert!("trajectory-match:exact:-0.01".parse::<CheckSpec>().is_err());
        assert!("trajectory-match:exact:NaN".parse::<CheckSpec>().is_err());
        assert!("trajectory-match:exact:1".parse::<CheckSpec>().is_ok());
        assert!("trajectory-match:exact:0".parse::<CheckSpec>().is_ok());
    }

    #[test]
    fn trajectory_spec_args_judge_sets_judge_flag() {
        let s = trajectory("trajectory-match:args-judge");
        assert_eq!(s.scoring, TrajectoryScoring::ArgsJudged);
        assert_eq!(
            CheckSpec::Trajectory(s).to_string(),
            "trajectory-match:args-judge"
        );
        let plain = trajectory("trajectory-match:args");
        assert_eq!(plain.scoring, TrajectoryScoring::Match(MatchMode::Args));
    }

    // ── trajectory-match: per-case scoring ─────────

    #[test]
    fn trajectory_eval_case_passes_on_match() {
        let spec = traj("trajectory-match:exact");
        let expected = tsteps(&["search"]);
        let r = eval_now(&spec, &tsteps(&["search"]), Some(&expected)).unwrap();
        assert!(r.passed());
        assert_eq!(r.name(), "trajectory-match:exact");
    }

    #[test]
    fn trajectory_eval_case_fails_on_mismatch_with_reason() {
        let spec = traj("trajectory-match:exact");
        let expected = tsteps(&["search"]);
        let r = eval_now(&spec, &tsteps(&["fetch"]), Some(&expected)).unwrap();
        assert!(!r.passed());
        let detail = r.detail().unwrap();
        assert!(
            detail.contains("search") && detail.contains("fetch"),
            "detail={detail}"
        );
    }

    #[test]
    fn trajectory_eval_case_args_mode_checks_arguments() {
        let spec = traj("trajectory-match:args");
        let s = |n: &str, a: serde_json::Value| ToolStep {
            name: n.into(),
            args: a,
        };
        let expected = vec![s("search", serde_json::json!({"q": "x"}))];
        // same name + same args → pass
        assert!(
            eval_now(
                &spec,
                &[s("search", serde_json::json!({"q": "x"}))],
                Some(&expected)
            )
            .unwrap()
            .passed()
        );
        // same name, WRONG args → fail (what args mode adds over exact)
        assert!(
            !eval_now(
                &spec,
                &[s("search", serde_json::json!({"q": "y"}))],
                Some(&expected)
            )
            .unwrap()
            .passed()
        );
    }

    #[test]
    fn trajectory_eval_case_skips_when_no_expected_tools() {
        let spec = traj("trajectory-match:ordered");
        assert!(eval_now(&spec, &tsteps(&["search"]), None).is_none());
    }

    #[test]
    fn trajectory_ordered_threshold_allows_partial_credit() {
        // ordered score for [a,x] vs [a,y] is 0.5; threshold 0.5 passes, 0.6 fails
        let pass = traj("trajectory-match:ordered:0.5");
        let fail = traj("trajectory-match:ordered:0.6");
        let expected = tsteps(&["a", "y"]);
        assert!(
            eval_now(&pass, &tsteps(&["a", "x"]), Some(&expected))
                .unwrap()
                .passed()
        );
        assert!(
            !eval_now(&fail, &tsteps(&["a", "x"]), Some(&expected))
                .unwrap()
                .passed()
        );
    }

    // ── corpus expected_tools.json ──────────────────────

    #[test]
    fn read_expected_tools_accepts_names_and_objects_and_missing() {
        let dir = TempDir::new().unwrap();
        // missing file → None
        assert!(
            read_expected_tools(&dir.path().join("nope.json"))
                .unwrap()
                .is_none()
        );

        // bare names
        let names = dir.path().join("names.json");
        write(&names, r#"["search","fetch"]"#);
        let got = read_expected_tools(&names).unwrap().unwrap();
        assert_eq!(
            got.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["search", "fetch"]
        );

        // {name,args} objects
        let objs = dir.path().join("objs.json");
        write(&objs, r#"[{"name":"search","args":{"q":"x"}}]"#);
        let got = read_expected_tools(&objs).unwrap().unwrap();
        assert_eq!(got[0].name, "search");
        assert_eq!(got[0].args, serde_json::json!({"q":"x"}));
    }

    #[test]
    fn read_expected_tools_fails_closed_on_malformed() {
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("bad.json");
        write(&bad, "{ not an array");
        assert!(read_expected_tools(&bad).is_err());
    }

    // ── head-to-head eval diff --trajectory ───────────

    fn ecase(id: &str, traj: &[&str], check: Option<(&str, bool)>) -> CaseReport {
        let checks = match check {
            Some((name, true)) => vec![CaseCheckResult::Passed {
                name: name.into(),
                note: None,
            }],
            Some((name, false)) => {
                vec![CaseCheckResult::Failed {
                    name: name.into(),
                    reason: "x".into(),
                }]
            }
            None => vec![],
        };
        CaseReport {
            case_id: id.parse().unwrap(),
            wall_clock_ms: 0,
            usage: Usage::default(),
            output_chars: 0,
            tool_trajectory: traj.iter().map(|s| s.to_string()).collect(),
            outcome: CaseOutcome::Ok { checks },
        }
    }

    fn emanifest(cases: Vec<CaseReport>, check_names: &[&str]) -> Manifest {
        Manifest {
            corpus_path: String::new(),
            provider_id: ProviderId::new("p"),
            model: String::new(),
            vendor: None,
            args_judge_vendors: None,
            started_at_ms: 0,
            ended_at_ms: 0,
            checks: check_names.iter().map(|n| (*n).to_string()).collect(),
            cases,
        }
    }

    #[test]
    fn compute_traj_diff_counts_divergence_and_mcnemar() {
        let cn = "trajectory-match:exact";
        let a = emanifest(
            vec![
                ecase("c1", &["s"], Some((cn, true))),
                ecase("c2", &["s"], Some((cn, true))),
                ecase("c3", &["s"], Some((cn, false))),
                ecase("c4", &["s"], Some((cn, false))),
            ],
            &[cn],
        );
        let b = emanifest(
            vec![
                ecase("c1", &["s"], Some((cn, true))),  // same traj; A✓ B✓
                ecase("c2", &["x"], Some((cn, false))), // differ; A✓ B✗ → b
                ecase("c3", &["x"], Some((cn, true))),  // differ; A✗ B✓ → c
                ecase("c4", &["s"], Some((cn, false))), // same traj; A✗ B✗
            ],
            &[cn],
        );
        let td = compute_traj_diff(&a, &b, MatchMode::Exact);
        assert_eq!(td.paired, 4);
        assert_eq!(td.divergent, 2);
        assert_eq!(td.diverging_ids, vec!["c2", "c3"]);
        assert_eq!(td.a_only, 0);
        assert_eq!(td.b_only, 0);
        assert_eq!(td.mcnemar.len(), 1);
        let CheckMcNemar {
            check: name,
            result: r,
        } = &td.mcnemar[0];
        assert_eq!(name, cn);
        assert_eq!(r.b, 1); // regressed: c2 (A pass, B fail)
        assert_eq!(r.c, 1); // improved:  c3 (A fail, B pass)
    }

    #[test]
    fn compute_traj_diff_reports_unpaired_cases_and_no_shared_check() {
        let a = emanifest(
            vec![ecase("c1", &["s"], None), ecase("c2", &["s"], None)],
            &[],
        );
        let b = emanifest(
            vec![ecase("c2", &["s"], None), ecase("c3", &["s"], None)],
            &[],
        );
        let td = compute_traj_diff(&a, &b, MatchMode::Ordered);
        assert_eq!(td.paired, 1);
        assert_eq!(td.a_only, 1);
        assert_eq!(td.b_only, 1);
        assert_eq!(td.divergent, 0); // the one paired case (c2) matches
        assert!(td.mcnemar.is_empty());
    }

    #[test]
    fn load_corpus_reads_expected_tools_into_case() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("c1").join("input.txt"), "hi");
        write(
            &dir.path().join("c1").join("expected_tools.json"),
            r#"["search"]"#,
        );
        let cases = load_corpus(dir.path()).unwrap();
        let et = cases[0].expected_tools.as_ref().unwrap();
        assert_eq!(et[0].name, "search");
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
        assert_eq!(cases[0].id.as_str(), "case_001");
        assert_eq!(cases[0].input, "first");
        assert_eq!(cases[0].system.as_deref(), Some("sys"));
        assert_eq!(cases[0].expected.as_deref(), Some("exp"));
        assert_eq!(cases[1].id.as_str(), "case_002");
        assert!(cases[1].system.is_none());
    }

    #[test]
    fn load_corpus_skips_subdirs_without_input() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("good").join("input.txt"), "ok");
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        write(&dir.path().join(".hidden").join("input.txt"), "should-skip");
        let cases = load_corpus(dir.path()).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id.as_str(), "good");
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
        use crate::truncate;
        assert_eq!(truncate("abc", 10), "abc");
        // The "…" is appended, not counted.
        assert_eq!(truncate("abcdef", 3), "abc…");
        let s = "あいうえお";
        let t = truncate(s, 2);
        assert!(t.starts_with("あい"));
        assert!(t.ends_with('…'));
    }

    #[test]
    fn manifest_serde_round_trip() {
        let m = Manifest {
            corpus_path: "/tmp/corpus".into(),
            provider_id: ProviderId::new("anthropic"),
            model: "claude-opus-4-7".into(),
            vendor: None,
            args_judge_vendors: None,
            started_at_ms: 1_000_000,
            ended_at_ms: 1_005_000,
            checks: vec!["non-empty".into()],
            cases: ["case_001", "case_002"]
                .map(|id| CaseReport {
                    case_id: id.parse().unwrap(),
                    wall_clock_ms: 2500,
                    usage: Usage::default(),
                    output_chars: 80,
                    tool_trajectory: Vec::new(),
                    outcome: CaseOutcome::Ok {
                        checks: vec![CaseCheckResult::Passed {
                            name: "non-empty".into(),
                            note: None,
                        }],
                    },
                })
                .into(),
        };
        let v = serde_json::to_value(&m).unwrap();
        let back: Manifest = serde_json::from_value(v).unwrap();
        assert_eq!(back.case_count(), 2);
        assert_eq!(back.success_count(), 2);
        assert!(back.cases[0].succeeded());
        assert_eq!(back.checks[0], "non-empty");
        assert!(back.cases[0].checks()[0].passed());
    }

    #[test]
    fn case_check_result_serialises_as_internally_tagged() {
        let pass = serde_json::to_value(CaseCheckResult::Passed {
            name: "x".into(),
            note: None,
        })
        .unwrap();
        assert_eq!(pass, serde_json::json!({"outcome": "passed", "name": "x"}));

        let pass_with_note = serde_json::to_value(CaseCheckResult::Passed {
            name: "x".into(),
            note: Some("validator skipped".into()),
        })
        .unwrap();
        assert_eq!(
            pass_with_note,
            serde_json::json!({
                "outcome": "passed",
                "name": "x",
                "note": "validator skipped",
            })
        );

        let fail = serde_json::to_value(CaseCheckResult::Failed {
            name: "y".into(),
            reason: "bad".into(),
        })
        .unwrap();
        assert_eq!(
            fail,
            serde_json::json!({"outcome": "failed", "name": "y", "reason": "bad"})
        );
    }

    #[test]
    fn case_check_result_round_trips_through_serde() {
        let cases = vec![
            CaseCheckResult::Passed {
                name: "p".into(),
                note: None,
            },
            CaseCheckResult::Passed {
                name: "p".into(),
                note: Some("n".into()),
            },
            CaseCheckResult::Failed {
                name: "f".into(),
                reason: "because".into(),
            },
        ];
        for c in cases {
            let json = serde_json::to_value(&c).unwrap();
            let back: CaseCheckResult = serde_json::from_value(json).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn case_check_result_rejects_failed_without_reason() {
        let bad = serde_json::json!({"outcome": "failed", "name": "z"});
        let err = serde_json::from_value::<CaseCheckResult>(bad).unwrap_err();
        assert!(err.to_string().contains("reason"));
    }

    #[tokio::test]
    async fn every_case_is_replayed_for_the_tenant_the_caller_names() {
        use tars_types::{TenantId, TraceId};

        let corpus = TempDir::new().unwrap();
        write(
            &corpus.path().join("c1").join("input.txt"),
            "b122 first case",
        );
        write(
            &corpus.path().join("c2").join("input.txt"),
            "b122 second case",
        );
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        let caller = Caller::new(TenantId::new("eval-tenant"), TraceId::new("t"));

        run_eval(
            RunRequest {
                registry: recorded_registry(LOCAL_PROVIDERS, std::slice::from_ref(&recorder)),
                provider_id: ProviderId::new("eval_rec"),
                caller,
                model: Some("m".into()),
                corpus: corpus.path().to_path_buf(),
                output: out.path().to_path_buf(),
                max_output_tokens: None,
                checks: CheckSet::default(),
                case_deadline: None,
            },
            |_| {},
        )
        .await
        .unwrap();

        let tenant = TenantId::new("eval-tenant");
        assert_eq!(recorder.tenants(), vec![tenant.clone(), tenant]);
    }

    #[tokio::test]
    async fn a_cases_expected_text_is_copied_into_the_run_and_a_case_without_one_gets_none() {
        use tars_types::{TenantId, TraceId};

        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("with").join("input.txt"), "q");
        write(
            &corpus.path().join("with").join("expected.txt"),
            "the answer",
        );
        write(&corpus.path().join("without").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");

        run_eval(
            RunRequest {
                registry: recorded_registry(LOCAL_PROVIDERS, &[recorder]),
                provider_id: ProviderId::new("eval_rec"),
                caller: Caller::new(TenantId::new("t"), TraceId::new("t")),
                model: Some("m".into()),
                corpus: corpus.path().to_path_buf(),
                output: out.path().to_path_buf(),
                max_output_tokens: None,
                checks: CheckSet::default(),
                case_deadline: None,
            },
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(out.path().join("with").join("expected.txt")).unwrap(),
            "the answer"
        );
        assert!(!out.path().join("without").join("expected.txt").exists());
    }

    /// Declares `eval_rec` and `judge_rec` as local providers whose default model is `m`.
    const LOCAL_PROVIDERS: &str = r#"
        [providers.eval_rec]
        type = "openai_compat"
        gen_ai_provider_name = "local"
        base_url = "http://127.0.0.1:1/v1"
        default_model = "m"

        [providers.judge_rec]
        type = "openai_compat"
        gen_ai_provider_name = "local"
        base_url = "http://127.0.0.1:1/v1"
        default_model = "m"
    "#;

    /// Builds the registry of the config `toml`, so models resolve against the catalog as they
    /// do in `tars`, with each provider a recorder is named after answered by that recorder.
    fn recorded_registry(
        toml: &str,
        recorders: &[Arc<crate::test_support::CallRecorder>],
    ) -> Arc<ProviderRegistry> {
        use tars_llm::LlmProvider;

        let cfg = tars_config::Config::from_toml(toml).unwrap();
        let built =
            ProviderRegistry::from_config_default(cfg.providers(), tars_config::catalog(), None)
                .unwrap();
        Arc::new(built.map_providers(|id, provider| {
            recorders
                .iter()
                .find(|r| r.id() == id)
                .map_or(provider, |r| r.clone() as Arc<dyn LlmProvider>)
        }))
    }

    fn eval_into(
        corpus: &Path,
        out: &Path,
        provider: Arc<crate::test_support::CallRecorder>,
    ) -> RunRequest {
        use tars_types::{TenantId, TraceId};
        RunRequest {
            registry: recorded_registry(LOCAL_PROVIDERS, &[provider]),
            provider_id: ProviderId::new("eval_rec"),
            caller: Caller::new(TenantId::new("t"), TraceId::new("t")),
            model: Some("m".into()),
            corpus: corpus.to_path_buf(),
            output: out.to_path_buf(),
            max_output_tokens: None,
            checks: CheckSet::default(),
            case_deadline: None,
        }
    }

    #[tokio::test]
    async fn work_past_its_deadline_is_abandoned_and_work_without_one_is_awaited() {
        let passed = within(
            Some(Deadline::after(Duration::ZERO)),
            std::future::pending::<()>(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            passed.case_failure(),
            CaseFailure::DeadlinePassed { deadline_ms: 0 }
        );
        assert_eq!(within(None, async { 7 }).await.unwrap(), 7);
        assert_eq!(
            within(Some(Deadline::after(Duration::from_secs(60))), async { 7 })
                .await
                .unwrap(),
            7
        );
    }

    fn judged(judge_id: &str, verdicts: Vec<(&str, crate::JudgeVerdict)>) -> JudgeReport {
        JudgeReport {
            judge_id: judge_id.into(),
            verdicts: verdicts
                .into_iter()
                .map(|(id, verdict)| crate::JudgedItem {
                    item_id: id.into(),
                    verdict,
                })
                .collect(),
            unjudged: Vec::new(),
        }
    }

    #[test]
    fn verdicts_of_two_judges_are_not_paired() {
        use crate::JudgeVerdict::Correct;
        let q = quality(
            judged("a:m", vec![("x", Correct)]),
            judged("b:m", vec![("x", Correct)]),
        );
        assert!(matches!(q, Quality::JudgesDiffer { .. }));
    }

    #[test]
    fn an_item_either_judge_was_unsure_of_takes_no_part_in_the_pairing() {
        use crate::JudgeVerdict::{Correct, Unsure, Wrong};
        let unsure = || Unsure { reason: "?".into() };
        let q = quality(
            judged("j", vec![("x", Correct), ("y", unsure()), ("z", Correct)]),
            judged("j", vec![("x", unsure()), ("y", Wrong), ("z", Wrong)]),
        );
        let Quality::Compared(diff) = q else {
            panic!("one judge graded both runs");
        };
        assert_eq!((diff.paired.b, diff.paired.c), (1, 0));
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_finishing_is_recorded_as_an_unfinished_case() {
        use tars_llm::CannedResponse;
        use tars_types::ChatEvent;

        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::replying(
            ProviderId::new("eval_rec"),
            CannedResponse::Sequence(vec![
                ChatEvent::started("m"),
                ChatEvent::Delta {
                    text: "half an ans".into(),
                },
            ]),
        );
        let checks = CheckSet::new(vec![CheckSpec::NonEmpty], None).unwrap();

        let run = run_eval(
            RunRequest {
                checks,
                ..eval_into(corpus.path(), out.path(), recorder)
            },
            |_| {},
        )
        .await
        .unwrap();

        let case = &run.manifest.cases[0];
        assert!(
            matches!(
                case.outcome,
                CaseOutcome::Error {
                    failure: CaseFailure::Call {
                        kind: tars_types::LlmErrorKind::Unfinished,
                        ..
                    }
                }
            ),
            "{:?}",
            case.outcome
        );
        assert_eq!(run.manifest.success_count(), 0);
        assert_eq!(run.manifest.check_summaries()[0].violation_rate(), None);
    }

    #[tokio::test]
    async fn a_call_that_fails_is_recorded_with_its_kind() {
        use tars_llm::CannedResponse;

        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::replying(
            ProviderId::new("eval_rec"),
            CannedResponse::Error("boom".into()),
        );

        let run = run_eval(eval_into(corpus.path(), out.path(), recorder), |_| {})
            .await
            .unwrap();

        let CaseOutcome::Error {
            failure: CaseFailure::Call { message, .. },
        } = &run.manifest.cases[0].outcome
        else {
            panic!("{:?}", run.manifest.cases[0].outcome);
        };
        assert!(message.contains("boom"), "{message}");
    }

    #[tokio::test]
    async fn a_reused_output_directory_is_refused_before_any_call_and_left_as_it_was() {
        let first = TempDir::new().unwrap();
        write(&first.path().join("c").join("input.txt"), "q");
        write(&first.path().join("c").join("expected.txt"), "OLD ANSWER");
        let second = TempDir::new().unwrap();
        write(&second.path().join("c").join("input.txt"), "q2");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        run_eval(eval_into(first.path(), out.path(), recorder), |_| {})
            .await
            .unwrap();

        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        let err = run_eval(
            eval_into(second.path(), out.path(), recorder.clone()),
            |_| {},
        )
        .await
        .expect_err("the directory holds the first run");

        assert!(
            matches!(&err, RunEvalError::OutputNotEmpty { path } if path == out.path()),
            "{err:?}"
        );
        assert!(recorder.tenants().is_empty());
        let case_dir = out.path().join("c");
        assert_eq!(fs::read_to_string(case_dir.join(INPUT_FILE)).unwrap(), "q");
        assert_eq!(
            fs::read_to_string(case_dir.join(EXPECTED_FILE)).unwrap(),
            "OLD ANSWER"
        );
    }

    #[tokio::test]
    async fn the_manifest_records_the_corpus_as_its_resolved_absolute_path() {
        let parent = TempDir::new().unwrap();
        write(
            &parent.path().join("corpus").join("c").join("input.txt"),
            "q",
        );
        let given = parent.path().join("corpus").join("..").join("corpus");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");

        let run = run_eval(eval_into(&given, out.path(), recorder), |_| {})
            .await
            .unwrap();

        let resolved = parent.path().join("corpus").canonicalize().unwrap();
        assert_eq!(run.manifest.corpus_path, resolved.display().to_string());
    }

    #[tokio::test]
    async fn a_case_named_like_the_manifest_is_refused_before_any_call() {
        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("manifest.json").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");

        let err = run_eval(
            eval_into(corpus.path(), out.path(), recorder.clone()),
            |_| {},
        )
        .await
        .expect_err("the case would take the manifest's place");

        assert!(
            matches!(&err, RunEvalError::CaseNamesARunFile { case } if case.as_str() == "manifest.json"),
            "{err:?}"
        );
        assert!(recorder.tenants().is_empty());
    }

    #[tokio::test]
    async fn an_eval_without_a_model_on_a_provider_not_built_from_config_is_refused_before_any_call()
     {
        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        let cfg = RunRequest {
            registry: Arc::new(ProviderRegistry::single(
                ProviderId::new("eval_rec"),
                recorder.clone(),
            )),
            model: None,
            ..eval_into(corpus.path(), out.path(), recorder.clone())
        };

        let err = run_eval(cfg, |_| {}).await.expect_err("no model to bind");

        assert!(
            matches!(
                err,
                RunEvalError::ModelUnresolved {
                    source: RegistryError::NoDefaultModel(_),
                    ..
                }
            ),
            "{err:#}"
        );
        assert!(recorder.prompts().is_empty());
    }

    #[tokio::test]
    async fn a_concrete_model_on_a_provider_not_built_from_config_is_bound_as_written() {
        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        run_eval(
            RunRequest {
                registry: Arc::new(ProviderRegistry::single(
                    ProviderId::new("eval_rec"),
                    recorder.clone(),
                )),
                model: Some("m".into()),
                ..eval_into(corpus.path(), out.path(), recorder.clone())
            },
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            RunDir::open(out.path().to_path_buf())
                .read_manifest()
                .unwrap()
                .model,
            "m"
        );
        assert_eq!(recorder.prompts().len(), 1);
    }

    #[tokio::test]
    async fn whitespace_around_a_model_is_not_bound_or_recorded() {
        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        run_eval(
            RunRequest {
                model: Some(" m ".into()),
                ..eval_into(corpus.path(), out.path(), recorder.clone())
            },
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            RunDir::open(out.path().to_path_buf())
                .read_manifest()
                .unwrap()
                .model,
            "m"
        );
    }

    #[tokio::test]
    async fn a_blank_model_is_refused_before_any_call_and_a_named_one_is_bound() {
        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");

        let blank = RunRequest {
            model: Some(" ".into()),
            ..eval_into(corpus.path(), out.path(), recorder.clone())
        };
        let err = run_eval(blank, |_| {})
            .await
            .expect_err("a blank model names no model");
        assert!(
            matches!(err, RunEvalError::ModelUnresolved { .. }),
            "{err:#}"
        );
        assert!(recorder.prompts().is_empty());

        run_eval(
            eval_into(corpus.path(), out.path(), recorder.clone()),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            RunDir::open(out.path().to_path_buf())
                .read_manifest()
                .unwrap()
                .model,
            "m"
        );
        assert_eq!(recorder.prompts().len(), 1);
    }

    /// The catalog is the process-global one and may carry this machine's refreshed model list,
    /// so the test compares against what the registry resolves rather than a fixed id.
    #[tokio::test]
    async fn a_series_model_spec_is_bound_and_recorded_as_the_concrete_model_it_resolves_to() {
        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        let registry = recorded_registry(
            r#"
            [providers.eval_rec]
            type = "gemini"
            auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
            default_model = "flash@latest"
            "#,
            std::slice::from_ref(&recorder),
        );
        let concrete = registry
            .resolve_model(&ProviderId::new("eval_rec"), Some("pro@latest"))
            .unwrap();

        run_eval(
            RunRequest {
                registry,
                model: Some("pro@latest".into()),
                ..eval_into(corpus.path(), out.path(), recorder)
            },
            |_| {},
        )
        .await
        .unwrap();

        let bound = RunDir::open(out.path().to_path_buf())
            .read_manifest()
            .unwrap()
            .model;
        assert_eq!(bound, concrete);
        assert!(!bound.contains('@'), "{bound}");
    }

    #[tokio::test]
    async fn the_judge_is_given_the_cases_input_and_system_prompt() {
        use tars_types::{TenantId, TraceId};

        let corpus = TempDir::new().unwrap();
        write(
            &corpus.path().join("case_7").join("input.txt"),
            "what is two plus two",
        );
        write(
            &corpus.path().join("case_7").join("system.txt"),
            "answer in digits",
        );
        let out = TempDir::new().unwrap();
        let evaluated = crate::test_support::CallRecorder::new("eval_rec", "4");
        run_eval(
            eval_into(corpus.path(), out.path(), evaluated.clone()),
            |_| {},
        )
        .await
        .unwrap();

        let judge = crate::test_support::CallRecorder::new("judge_rec", "TP");
        run_judge(
            JudgeRequest {
                registry: recorded_registry(LOCAL_PROVIDERS, &[evaluated, judge.clone()]),
                caller: Caller::new(TenantId::new("t"), TraceId::new("t")),
                run: out.path().to_path_buf(),
                judge: ProviderId::new("judge_rec"),
                judge_model: Some("m".into()),
            },
            |_| {},
        )
        .await
        .unwrap();

        let prompts = judge.prompts();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains("what is two plus two"),
            "{}",
            prompts[0]
        );
        assert!(prompts[0].contains("answer in digits"), "{}", prompts[0]);
        assert!(
            !prompts[0].contains("--- INPUT ---\ncase_7"),
            "{}",
            prompts[0]
        );
    }

    /// Runs one case on `eval_rec` bound to `run_model`, then judges it with `judge_rec` bound to
    /// `judge_model`; both are `openai_compat`, so only the model ids say whose models they are.
    async fn judge_after_run(
        run_model: &str,
        judge_model: &str,
    ) -> (TempDir, Result<JudgedRun, JudgeRunError>) {
        use tars_types::{TenantId, TraceId};

        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let evaluated = crate::test_support::CallRecorder::new("eval_rec", "a");
        let judge = crate::test_support::CallRecorder::new("judge_rec", "TP");
        let registry = recorded_registry(LOCAL_PROVIDERS, &[evaluated.clone(), judge]);
        run_eval(
            RunRequest {
                registry: registry.clone(),
                model: Some(run_model.into()),
                ..eval_into(corpus.path(), out.path(), evaluated)
            },
            |_| {},
        )
        .await
        .unwrap();
        let judged = run_judge(
            JudgeRequest {
                registry,
                caller: Caller::new(TenantId::new("t"), TraceId::new("t")),
                run: out.path().to_path_buf(),
                judge: ProviderId::new("judge_rec"),
                judge_model: Some(judge_model.into()),
            },
            |_| {},
        )
        .await;
        (out, judged)
    }

    #[tokio::test]
    async fn a_run_records_the_vendor_of_its_model() {
        let (out, _) = judge_after_run("claude-sonnet-5", "gpt-5.4").await;
        let vendor = RunDir::open(out.path().to_path_buf())
            .read_manifest()
            .unwrap()
            .vendor;
        assert_eq!(vendor.as_ref().map(Vendor::as_str), Some("anthropic"));
    }

    #[tokio::test]
    async fn a_judge_whose_model_is_the_runs_vendor_is_refused_through_another_provider() {
        let (_out, judged) = judge_after_run("claude-sonnet-5", "claude-opus-4-7").await;
        let err = judged.unwrap_err();
        assert!(
            matches!(&err, JudgeRunError::Judge(JudgeSetupError::SharesRunVendor { refusal, .. })
                if refusal.vendor.as_str() == "anthropic"),
            "{err}"
        );
        let said = err.to_string();
        assert!(
            said.contains("eval_rec") && said.contains("judge_rec"),
            "{said}"
        );
    }

    #[tokio::test]
    async fn a_judge_of_another_vendor_is_recorded_as_verified() {
        let (out, judged) = judge_after_run("claude-sonnet-5", "gpt-5.4").await;
        assert!(judged.unwrap().vendors.is_verified());
        let written: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(out.path().join("judge_report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["vendors"]["vendors"], "distinct");
        assert_eq!(written["vendors"]["judge"], "openai");
    }

    #[tokio::test]
    async fn a_model_of_unknown_vendor_is_judged_and_recorded_as_unverified() {
        let (out, judged) = judge_after_run("m", "claude-opus-4-7").await;
        assert_eq!(
            judged.unwrap().vendors,
            VendorCheck::UnknownRunVendor {
                judge: MODEL_KB.vendor_of("claude-opus-4-7").cloned()
            }
        );
        let report = RunDir::open(out.path().to_path_buf())
            .read_judge_report()
            .unwrap()
            .unwrap();
        assert_eq!(report.item_count(), 1);
    }

    #[test]
    fn a_judge_calls_read_only_and_keeps_the_runs_network_setting() {
        use tars_types::{TenantId, TraceId};

        for network in [true, false] {
            let run = Caller::new(TenantId::new("t"), TraceId::new("t"))
                .with_sandbox(SandboxPolicy::DangerFullAccess { network });
            assert_eq!(
                judge_caller(&run).sandbox,
                SandboxPolicy::ReadOnly { network }
            );
        }
    }

    #[test]
    fn a_max_length_violation_says_how_long_the_output_was() {
        let check = invariant_of(&"max-length:3".parse().unwrap()).unwrap();
        let mut b = tars_types::ChatResponseBuilder::new();
        b.apply(tars_types::ChatEvent::started("m"));
        b.apply(tars_types::ChatEvent::Delta {
            text: "hello".into(),
        });
        let result = check.check(&ChatRequest::user("q"), &b.finish());
        assert!(!result.passed());
        let detail = result.detail().unwrap();
        assert!(
            detail.contains("length=5") && detail.contains("3"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn a_successful_case_whose_output_file_is_gone_is_refused_not_judged_as_empty() {
        use tars_types::{TenantId, TraceId};

        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let evaluated = crate::test_support::CallRecorder::new("eval_rec", "a");
        run_eval(
            eval_into(corpus.path(), out.path(), evaluated.clone()),
            |_| {},
        )
        .await
        .unwrap();
        fs::remove_file(out.path().join("c").join("output.txt")).unwrap();

        let judge = crate::test_support::CallRecorder::new("judge_rec", "TP");
        let err = run_judge(
            JudgeRequest {
                registry: recorded_registry(LOCAL_PROVIDERS, &[evaluated, judge.clone()]),
                caller: Caller::new(TenantId::new("t"), TraceId::new("t")),
                run: out.path().to_path_buf(),
                judge: ProviderId::new("judge_rec"),
                judge_model: Some("m".into()),
            },
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&err, JudgeRunError::CaseFileMissing { path } if path.ends_with("c/output.txt")),
            "{err}"
        );
        assert!(judge.prompts().is_empty());
    }

    #[tokio::test]
    async fn two_length_limits_are_summarised_as_two_checks() {
        let corpus = TempDir::new().unwrap();
        write(&corpus.path().join("c").join("input.txt"), "q");
        let out = TempDir::new().unwrap();
        let recorder = crate::test_support::CallRecorder::new("eval_rec", "ok");
        let checks = CheckSet::new(
            vec![
                "max-length:1".parse().unwrap(),
                "max-length:1000".parse().unwrap(),
            ],
            None,
        )
        .unwrap();

        let run = run_eval(
            RunRequest {
                checks,
                ..eval_into(corpus.path(), out.path(), recorder)
            },
            |_| {},
        )
        .await
        .unwrap();

        let summaries: Vec<(String, u32, u32)> = run
            .manifest
            .check_summaries()
            .into_iter()
            .map(|c| (c.name, c.evaluated, c.violations))
            .collect();
        assert_eq!(
            summaries,
            [
                ("max-length:1".to_string(), 1, 1),
                ("max-length:1000".to_string(), 1, 0)
            ]
        );
    }

    #[test]
    fn a_manifest_written_before_vendors_reads_with_no_vendor() {
        let mut m = emanifest(Vec::new(), &[]);
        m.vendor = MODEL_KB.vendor_of("claude-opus-4-7").cloned();
        let mut old = serde_json::to_value(&m).unwrap();
        old.as_object_mut().unwrap().remove("vendor");
        let read: Manifest = serde_json::from_value(old).unwrap();
        assert_eq!(read.vendor, None);
        assert_eq!(read.args_judge_vendors, None);
    }
}
