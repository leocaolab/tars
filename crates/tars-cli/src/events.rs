//! Runs `tars events`, which reads the call log: the `llm_call` rows (and the
//! `eval_score` rows under them) that `EventEmitterMiddleware` writes into
//! an event-store directory, and under `show` the `llm_wire` rows a call's
//! transport wrote, one per attempt.
//!
//! Without `--store-dir` it reads `events/` in the tars home
//! ([`tars_config::resolve_home`]). Pipelines write there only when given that directory as
//! their `event_store_dir`; none defaults to it.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use serde::Serialize;

use tars_melt::event::call_log::{
    self, CallBody, CallOutcome, EvalScoreRow, LlmCallRow, LlmCallSummary, TagFilter, call_scope,
};
use tars_melt::event::event_logs::EventLogs;
use tars_melt::event::log::{LogError, LogSource, TenantScope, TimeWindow, WindowError};
use tars_melt::event::wire::{self, Transport, WireOutcome, WireRow};
use tars_types::telemetry::RetryAttempt;
use tars_types::{
    CallId, CompatibilityReason, Degradation, LlmErrorKind, Millis, StopReason, TenantId, Usage,
    ValidationReason,
};
use uuid::Uuid;

use render::{json_lines, render_bodies, render_list, render_wire};

#[derive(Args, Debug)]
pub struct EventsArgs {
    /// Path to the event store directory (the call log's root). Defaults
    /// to `events/` in `$TARS_HOME`, else in `~/.tars`.
    #[arg(long, env = "TARS_EVENT_STORE_DIR", global = true)]
    store_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: EventsCommand,
}

#[derive(Subcommand, Debug)]
enum EventsCommand {
    /// List recent calls. One row per call with key columns.
    List(ListArgs),
    /// Show one call in full, with its scores and the attempts its transport
    /// recorded, optionally with both bodies and every attempt's raw bytes.
    Show(ShowArgs),
}

#[derive(Args, Debug)]
struct ListArgs {
    /// The tenant whose calls to list.
    #[arg(long, value_parser = TenantId::from_str)]
    tenant: TenantId,
    /// Look back this far. Accepts `1d`, `2h`, `30m`, `45s`. Pass
    /// `--since all` to disable the lower bound.
    #[arg(long, default_value = "7d")]
    since: String,
    /// Filter by cohort tag — calls whose `tags` contain this string.
    #[arg(long)]
    tag: Option<String>,
    /// Hard cap on rows.
    #[arg(long, default_value_t = 50)]
    limit: usize,
    /// Output as JSON lines instead of a human-readable table.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ShowArgs {
    /// The tenant the call belongs to.
    #[arg(long, value_parser = TenantId::from_str)]
    tenant: TenantId,
    /// The call's `call_id` (a UUIDv7); list with `tars events list` first.
    id: Uuid,
    /// Print the request and response bodies too, and each wire attempt's
    /// bytes as sent and received.
    #[arg(long)]
    with_bodies: bool,
}

/// Why a `show` finds no single call to print.
#[derive(Debug, thiserror::Error)]
pub enum ShowError {
    #[error("not a call id; a call is looked up by its own id (UUIDv7)")]
    NotACallId(#[from] tars_types::CallIdError),
    #[error("call {call_id} not found for tenant {tenant}")]
    NotFound { tenant: TenantId, call_id: CallId },
    #[error("{rows} rows of tenant {tenant} carry call id {call_id}; a call has one row")]
    Duplicate {
        tenant: TenantId,
        call_id: CallId,
        rows: usize,
    },
}

/// Why `tars events` printed nothing.
#[derive(Debug, thiserror::Error)]
pub enum EventsError {
    #[error("no tars home: set $TARS_HOME or $HOME, or pass --store-dir")]
    NoHome,
    #[error(
        "event store dir is not a directory: {}\n\
         Pipeline writes to it only when constructed with `event_store_dir=...`.",
        dir.display()
    )]
    NotADirectory { dir: PathBuf },
    #[error(transparent)]
    Since(#[from] ParseSinceError),
    #[error(transparent)]
    Window(#[from] WindowError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error(transparent)]
    Show(#[from] ShowError),
    #[error("rendering as JSON")]
    Json(#[from] serde_json::Error),
}

/// Why a `--since` value names no lower bound.
#[derive(Debug, thiserror::Error)]
pub enum ParseSinceError {
    #[error("invalid --since value: {raw} (expected like `1d`, `2h`, `30m`, `45s`, or `all`)")]
    Number {
        raw: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("unknown duration unit `{unit}`; use s/m/h/d")]
    Unit { unit: String },
    #[error("--since {raw}: the lower bound is past the millisecond range")]
    OutOfRange {
        raw: String,
        #[source]
        source: std::num::TryFromIntError,
    },
}

pub async fn execute(args: EventsArgs) -> Result<(), EventsError> {
    print!("{}", run(args).await?);
    Ok(())
}

/// Returns the command's whole stdout.
async fn run(args: EventsArgs) -> Result<String, EventsError> {
    let dir = resolve_store_dir(args.store_dir.as_deref())?;
    let stores = open(&dir)?;
    match args.command {
        EventsCommand::List(a) => list(&stores, a).await,
        EventsCommand::Show(a) => show(&stores, a).await,
    }
}

fn resolve_store_dir(explicit: Option<&Path>) -> Result<PathBuf, EventsError> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let home = tars_config::resolve_home(None).ok_or(EventsError::NoHome)?;
    Ok(home.join("events"))
}

/// Opens the event store at `dir`, refusing a missing directory, which is far likelier a mistyped
/// `--store-dir` than an empty store.
fn open(dir: &Path) -> Result<EventLogs, EventsError> {
    if !dir.is_dir() {
        return Err(EventsError::NotADirectory {
            dir: dir.to_path_buf(),
        });
    }
    Ok(EventLogs::open_in_dir(dir))
}

/// Returns the tenant's scope since `--since`.
fn scope_since(tenant: TenantId, since: &str) -> Result<TenantScope, EventsError> {
    let window = match parse_since(since, SystemTime::now())? {
        Some(at) => TimeWindow::since(at)?,
        None => TimeWindow::all(),
    };
    Ok(TenantScope::new(tenant, window))
}

/// Lists the tenant's calls since `--since` that carry `--tag`, at most `--limit`.
async fn list(stores: &EventLogs, args: ListArgs) -> Result<String, EventsError> {
    let scope = scope_since(args.tenant, &args.since)?;
    let filter = TagFilter::from(args.tag);
    let calls = call_log::summaries(&stores.calls(), &scope, &filter, Some(args.limit)).await?;

    let lines: Vec<CallSummaryView> = calls.into_iter().map(CallSummaryView::from).collect();

    if args.json {
        return Ok(json_lines(&lines)?);
    }
    Ok(render_list(&lines))
}

/// One call as `list` shows it: `--json` prints all of it, the table the
/// columns that fit a line.
#[derive(Debug, Serialize)]
struct CallSummaryView {
    call_id: String,
    at_ms: Millis,
    tenant: String,
    model: String,
    /// See [`LlmCallSummary::actual_model`].
    actual_model: Option<String>,
    outcome: CallEndView,
    usage: Option<Usage>,
    provider_latency_ms: Option<u64>,
    tags: Vec<String>,
}

impl From<LlmCallSummary> for CallSummaryView {
    fn from(call: LlmCallSummary) -> Self {
        Self {
            call_id: call.call_id.to_string(),
            at_ms: call.at_ms(),
            tenant: call.tenant.to_string(),
            model: call.model,
            actual_model: call.actual_model,
            outcome: CallEndView::from(&call.outcome),
            usage: call.usage,
            provider_latency_ms: call.provider_latency_ms,
            tags: call.tags,
        }
    }
}

/// [`CallOutcome`] as the CLI prints it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CallEndView {
    Completed,
    Failed { kind: LlmErrorKind },
    Unterminated,
    Cancelled,
}

impl From<&CallOutcome> for CallEndView {
    fn from(outcome: &CallOutcome) -> Self {
        match outcome {
            CallOutcome::Completed => Self::Completed,
            CallOutcome::Refused(_) => Self::Failed {
                kind: LlmErrorKind::Incompatible,
            },
            CallOutcome::Failed(failure) => Self::Failed {
                kind: failure.kind(),
            },
            CallOutcome::Unterminated => Self::Unterminated,
            CallOutcome::Cancelled => Self::Cancelled,
        }
    }
}

impl std::fmt::Display for CallEndView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => f.write_str("ok"),
            Self::Failed { kind } => write!(f, "err:{kind}"),
            Self::Unterminated => f.write_str("unterminated"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

async fn show(stores: &EventLogs, args: ShowArgs) -> Result<String, EventsError> {
    let id = CallId::try_from(args.id).map_err(ShowError::NotACallId)?;
    let row = one_call(stores, &call_scope(&args.tenant, id), id).await?;
    let scope = call_scope(&args.tenant, row.call_id);
    let scores = stores.scores().scan_key(&scope, &row.call_id).await?;
    let attempts = wire::attempts(&stores.wire(), &args.tenant, row.call_id).await?;

    let detail = CallDetailView::of(&row, &scores, &attempts);
    let mut out = format!("{}\n", serde_json::to_string_pretty(&detail)?);
    if args.with_bodies {
        out.push_str(&render_bodies(&row)?);
        out.push_str(&render_wire(&attempts));
    }
    Ok(out)
}

async fn one_call(
    stores: &EventLogs,
    scope: &TenantScope,
    call_id: CallId,
) -> Result<LlmCallRow, EventsError> {
    let mut rows = stores.calls().scan_key(scope, &call_id).await?;
    let tenant = scope.tenant();
    let found = match rows.len() {
        0 => Err(ShowError::NotFound {
            tenant: tenant.clone(),
            call_id,
        }),
        1 => Ok(rows.remove(0)),
        n => Err(ShowError::Duplicate {
            tenant: tenant.clone(),
            call_id,
            rows: n,
        }),
    };
    Ok(found?)
}

/// One call as `show` prints it, bodies aside: their state is here, their
/// text under `--with-bodies`.
#[derive(Debug, Serialize)]
struct CallDetailView<'a> {
    #[serde(flatten)]
    line: CallSummaryView,
    session_id: Option<tars_types::SessionId>,
    trace_id: Option<&'a str>,
    provider_id: Option<&'a str>,
    request_body: BodyView,
    /// What the request was sent without; `None` when the row does not record it.
    degraded: Option<&'a [Degradation]>,
    /// `None` ⇔ the stream delivered no terminal event; `outcome` says why.
    response: Option<ResponseView>,
    retry_attempts: &'a [RetryAttempt],
    pipeline_total_ms: Option<u64>,
    layers: &'a [String],
    validation_reason: Option<ValidationReason>,
    /// Why the provider could not serve the request; `None` unless the call was refused.
    refusal_reasons: Option<Vec<CompatibilityReason>>,
    scores: Vec<ScoreView<'a>>,
    /// Every attempt the provider's transport recorded, first first.
    wire: WireView<'a>,
}

impl<'a> CallDetailView<'a> {
    fn of(row: &'a LlmCallRow, scores: &'a [EvalScoreRow], attempts: &'a [WireRow]) -> Self {
        let outcome = row.outcome();
        Self {
            line: CallSummaryView::from(LlmCallSummary::of(row)),
            session_id: row.session_id,
            trace_id: Some(row.trace_id.as_str()),
            provider_id: row.provider_id().map(|p| p.as_str()),
            request_body: BodyView::of(&row.request_body),
            degraded: row.degraded(),
            response: row.response().map(|r| ResponseView {
                body: BodyView::of(&r.body),
                stop_reason: r.stop_reason,
            }),
            retry_attempts: &row.retry_attempts,
            pipeline_total_ms: row.pipeline_total_ms,
            layers: &row.layers,
            validation_reason: outcome.validation_reason().cloned(),
            refusal_reasons: outcome
                .refusal_reasons()
                .map(<[CompatibilityReason]>::to_vec),
            scores: scores.iter().map(ScoreView::of).collect(),
            wire: WireView::of(attempts),
        }
    }
}

/// A call's wire attempts, their bytes counted; their bytes themselves under
/// `--with-bodies`.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum WireView<'a> {
    Recorded {
        attempts: Vec<AttemptView<'a>>,
    },
    /// No transport recorded an attempt: the call ran with no wire log, on a
    /// path with no raw bytes (the Claude SDK, a batch, a cache
    /// hit), or never reached a transport.
    Unrecorded,
}

impl<'a> WireView<'a> {
    fn of(attempts: &'a [WireRow]) -> Self {
        if attempts.is_empty() {
            Self::Unrecorded
        } else {
            Self::Recorded {
                attempts: attempts.iter().map(AttemptView::of).collect(),
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct AttemptView<'a> {
    attempt: u32,
    at_ms: Millis,
    provider_id: &'a str,
    model: &'a str,
    transport: &'static str,
    status: Option<u16>,
    request_bytes: usize,
    /// The argv a delegate was spawned with; `None` on HTTP and when no
    /// process was spawned.
    argv: Option<&'a [String]>,
    response_bytes: usize,
    response_truncated: bool,
    /// `None` ⇔ the attempt had no stderr stream (HTTP; a delegate that
    /// never spawned).
    stderr_bytes: Option<usize>,
    outcome: &'static str,
    /// The error as the transport raised it; only on a failed attempt.
    error: Option<&'a str>,
}

impl<'a> AttemptView<'a> {
    fn of(a: &'a WireRow) -> Self {
        let (outcome, error) = wire_outcome(&a.outcome);
        let (status, process) = match &a.transport {
            Transport::HttpSse { status } => (*status, None),
            Transport::Subprocess { process } => (None, process.as_ref()),
        };
        Self {
            attempt: a.attempt,
            at_ms: a.at_ms,
            provider_id: a.provider_id.as_str(),
            model: &a.model,
            transport: a.transport.as_str(),
            status,
            request_bytes: a.request_bytes.len(),
            argv: process.map(|p| p.argv.as_slice()),
            response_bytes: a.response_bytes.len(),
            response_truncated: a.response_truncated,
            stderr_bytes: process.map(|p| p.stderr.len()),
            outcome,
            error,
        }
    }
}

fn wire_outcome(o: &WireOutcome) -> (&'static str, Option<&str>) {
    match o {
        WireOutcome::Completed => ("completed", None),
        WireOutcome::Failed { error } => ("failed", Some(error)),
        WireOutcome::Cancelled => ("cancelled", None),
    }
}

#[derive(Debug, Serialize)]
struct ResponseView {
    body: BodyView,
    stop_reason: Option<StopReason>,
}

/// [`CallBody`] without its text.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum BodyView {
    Present {
        bytes: usize,
    },
    Withheld,
    /// The body did not serialize; `error` is the serializer's message.
    Unserializable {
        error: String,
    },
}

impl BodyView {
    fn of(body: &CallBody) -> Self {
        match body {
            CallBody::Present(text) => Self::Present { bytes: text.len() },
            CallBody::Withheld => Self::Withheld,
            CallBody::Unserializable { error } => Self::Unserializable {
                error: error.clone(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ScoreView<'a> {
    score_id: String,
    at_ms: Millis,
    evaluator: &'a str,
    score: f64,
    explanation: Option<&'a str>,
    tags: &'a [String],
}

impl<'a> ScoreView<'a> {
    fn of(s: &'a EvalScoreRow) -> Self {
        Self {
            score_id: s.score_id.to_string(),
            at_ms: s.at_ms,
            evaluator: &s.evaluator,
            score: s.score,
            explanation: s.explanation.as_deref(),
            tags: &s.tags,
        }
    }
}

/// Returns the lower bound `--since` names, in epoch ms; `None` for `all`.
fn parse_since(s: &str, now: SystemTime) -> Result<Option<Millis>, ParseSinceError> {
    if s == "all" {
        return Ok(None);
    }
    // The unit is split off by character: slicing at `len()-1` would panic on a multibyte
    // suffix like `1д`.
    let split = s.char_indices().last().map_or(0, |(i, _)| i);
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|source| ParseSinceError::Number {
        raw: s.to_string(),
        source,
    })?;
    // Saturates, so an absurd value means "very far back" rather than a tiny wrapped window.
    let secs = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3600),
        "d" => n.saturating_mul(86400),
        other => {
            return Err(ParseSinceError::Unit {
                unit: other.to_string(),
            });
        }
    };
    let lower_bound = now
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or(UNIX_EPOCH);
    let since = lower_bound
        .duration_since(UNIX_EPOCH)
        .map_or(Ok(0), |d| Millis::try_from(d.as_millis()))
        .map_err(|source| ParseSinceError::OutOfRange {
            raw: s.to_string(),
            source,
        })?;
    Ok(Some(since))
}

mod render;

#[cfg(test)]
mod tests;
