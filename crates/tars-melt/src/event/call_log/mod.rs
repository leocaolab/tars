//! The call log: one [`LlmCallRow`] per LLM call with the request
//! and response bodies inline, and one [`EvalScoreRow`] per score. Both are
//! [`LogTable`]s — written through a `LogSink`, read through a `LogSource` —
//! partitioned by tenant and by the UTC day of the call's [`CallId`].

#[cfg(feature = "parquet")]
pub(crate) mod columns;

use std::num::NonZeroUsize;
use std::time::Duration;

use tars_types::telemetry::RetryAttempt;
use tars_types::{
    CallId, CompatibilityReason, Degradation, LlmErrorKind, Millis, NonEmpty, OutputLimit,
    ProviderId, SessionId, StopReason, TenantId, TraceId, Usage, ValidationReason,
    ValidationSummary,
};
use uuid::Uuid;

#[cfg(feature = "parquet")]
use super::log::CompactRule;
use super::log::{
    LogError, LogSource, LogTable, PartialSource, Partition, Partitioned, Projected, SealPolicy,
    SinkConfig, TableName, TenantScope, TimeWindow,
};

/// Rows per row group: a point lookup by `call_id` decodes at most this many bodies.
const ROW_GROUP_ROWS: NonZeroUsize = NonZeroUsize::new(16).unwrap();

/// The sealing policy the call log is opened with: a crash loses at most the last second of calls.
pub const CALL_LOG_SINK: SinkConfig = SinkConfig {
    seal: SealPolicy {
        max_rows: 256,
        max_bytes: 16 << 20,
        max_age: Duration::from_secs(1),
        grace: Duration::from_secs(600),
    },
    capacity: NonZeroUsize::new(4096).unwrap(),
};

/// How the call log's partitions merge: at one seal a second, 16 files of a
/// level become one of the next, so a day stays at tens of files, not tens
/// of thousands.
#[cfg(feature = "parquet")]
pub const CALL_LOG_COMPACTION: CompactRule = CompactRule::new(16);

/// The `llm_call` table.
pub struct LlmCallTable;

/// One LLM call, written once when it ends.
#[derive(Debug, Clone)]
pub struct LlmCallRow {
    pub call_id: CallId,
    pub tenant: TenantId,
    pub session_id: Option<SessionId>,
    pub trace_id: TraceId,
    /// The model this service is bound to.
    pub model: String,

    /// The serialized `ChatRequest`.
    pub request_body: CallBody,
    /// sha256 of the serialized request, tenant-agnostic.
    pub request_sha256: [u8; 32],
    pub has_tools: bool,
    pub has_thinking: bool,
    pub has_structured_output: bool,
    pub temperature: Option<f32>,
    /// As sent: the caller's value, or the bound model's ceiling. On a refused call, which sent
    /// nothing, the caller's value.
    pub max_output_tokens: Option<u32>,
    pub output_limit: OutputLimit,
    pub dispatch: CallDispatch,

    pub retry_attempts: Vec<RetryAttempt>,
    pub provider_latency_ms: Option<u64>,
    pub pipeline_total_ms: Option<u64>,
    /// The layers of the chain the call ran into, outermost first, ending in `provider`; empty for
    /// a call refused before any layer ran.
    pub layers: Vec<String>,
    pub validation_summary: ValidationSummary,
    pub tags: Vec<String>,
}

impl LlmCallRow {
    /// Returns when the call started, the time its id was minted with.
    pub fn at_ms(&self) -> Millis {
        started_at(self.call_id)
    }

    /// Returns how the call ended, flat, as a listing shows it.
    pub fn outcome(&self) -> CallOutcome {
        self.dispatch.outcome()
    }

    /// Returns what the drained stream used, when one was drained and its provider reported it.
    pub fn usage(&self) -> Option<Usage> {
        self.dispatch.usage()
    }

    /// Returns what the model answered, when the stream delivered its terminal event.
    pub fn response(&self) -> Option<&CallResponse> {
        self.dispatch.response()
    }

    /// Returns the provider that ran the call; `None` when the call was refused or none ran it
    /// (a cache hit, a failure before provider resolution).
    pub fn provider_id(&self) -> Option<&ProviderId> {
        match &self.dispatch {
            CallDispatch::Refused { .. } => None,
            CallDispatch::Sent { provider_id, .. } => provider_id.as_ref(),
        }
    }

    /// Returns the parts of the request its provider was not sent; `None` on a refused call,
    /// whose check does not list them.
    pub fn degraded(&self) -> Option<&[Degradation]> {
        match &self.dispatch {
            CallDispatch::Refused { .. } => None,
            CallDispatch::Sent { degraded, .. } => Some(degraded),
        }
    }
}

impl LlmCallRow {
    /// Returns the row with every kept body, the request's and the response's, withheld; a body
    /// that did not serialize stays as it is, since nothing of it was kept.
    #[must_use]
    pub fn withhold_bodies(self) -> Self {
        Self {
            request_body: self.request_body.withheld(),
            dispatch: match self.dispatch {
                CallDispatch::Sent {
                    degraded,
                    provider_id,
                    end: SentEnd::Streamed { usage, end },
                } => CallDispatch::Sent {
                    degraded,
                    provider_id,
                    end: SentEnd::Streamed {
                        usage,
                        end: end.withholding_body(),
                    },
                },
                dispatch => dispatch,
            },
            ..self
        }
    }
}

/// Returns the start time a v7 id carries: its first 48 bits, big-endian Unix milliseconds
/// (RFC 9562 section 5.7).
fn started_at(id: CallId) -> Millis {
    let b = *Uuid::from(id).as_bytes();
    Millis::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]])
}

/// Whether the dispatch layer refused the call or sent it, and how a sent call ended.
#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "almost every call is sent; boxing that variant would allocate per call for the rare refusal"
)]
pub enum CallDispatch {
    /// The provider could not serve the request; nothing was sent and no layer ran.
    Refused {
        reasons: NonEmpty<CompatibilityReason>,
    },
    Sent {
        /// The parts of the request its provider was not sent, empty when it went out whole.
        degraded: Vec<Degradation>,
        /// The provider that ran the call; `None` when none did (a cache hit, a failure before
        /// provider resolution).
        provider_id: Option<ProviderId>,
        end: SentEnd,
    },
}

impl CallDispatch {
    fn outcome(&self) -> CallOutcome {
        match self {
            Self::Refused { reasons } => CallOutcome::Refused(reasons.clone()),
            Self::Sent { end, .. } => end.outcome(),
        }
    }

    fn usage(&self) -> Option<Usage> {
        match self {
            Self::Sent {
                end: SentEnd::Streamed { usage, .. },
                ..
            } => *usage,
            _ => None,
        }
    }

    fn response(&self) -> Option<&CallResponse> {
        match self {
            Self::Sent {
                end: SentEnd::Streamed { end, .. },
                ..
            } => end.response(),
            _ => None,
        }
    }
}

/// How a sent call ended. A cancelled call has no usage, whether or not its stream had opened:
/// the drop guard that writes it holds none.
#[derive(Debug, Clone)]
pub enum SentEnd {
    /// The call failed before a stream opened.
    FailedBeforeStream(CallFailure),
    /// The caller dropped the call, before the stream opened or before it ended.
    Cancelled,
    /// A stream was drained; `usage` is `None` when the provider reported none.
    Streamed {
        usage: Option<Usage>,
        end: StreamEnd,
    },
}

impl SentEnd {
    fn outcome(&self) -> CallOutcome {
        match self {
            Self::FailedBeforeStream(failure) => CallOutcome::Failed(failure.clone()),
            Self::Cancelled => CallOutcome::Cancelled,
            Self::Streamed { end, .. } => match end {
                StreamEnd::Completed(_) => CallOutcome::Completed,
                StreamEnd::Failed { failure, .. } => CallOutcome::Failed(failure.clone()),
                StreamEnd::Unterminated => CallOutcome::Unterminated,
            },
        }
    }
}

/// How a drained stream ended; the response is there exactly when the terminal event was.
#[derive(Debug, Clone)]
pub enum StreamEnd {
    Completed(CallResponse),
    /// The stream yielded an error; `response` is there when it also delivered its terminal
    /// event.
    Failed {
        failure: CallFailure,
        response: Option<CallResponse>,
    },
    /// The stream ended without a terminal event and without an error.
    Unterminated,
}

impl StreamEnd {
    fn withholding_body(self) -> Self {
        let withhold = |r: CallResponse| CallResponse {
            body: r.body.withheld(),
            ..r
        };
        match self {
            Self::Completed(response) => Self::Completed(withhold(response)),
            Self::Failed { failure, response } => Self::Failed {
                failure,
                response: response.map(withhold),
            },
            Self::Unterminated => Self::Unterminated,
        }
    }

    fn response(&self) -> Option<&CallResponse> {
        match self {
            Self::Completed(response) => Some(response),
            Self::Failed { response, .. } => response.as_ref(),
            Self::Unterminated => None,
        }
    }
}

/// How a call ended, flat, as a listing shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum CallOutcome {
    /// The stream delivered its terminal event.
    Completed,
    /// The dispatch layer refused the call.
    Refused(NonEmpty<CompatibilityReason>),
    /// The call, or its stream, failed.
    Failed(CallFailure),
    /// The stream ended without a terminal event and without an error.
    Unterminated,
    /// The caller dropped the call — before the stream opened, or before it ended.
    Cancelled,
}

impl CallOutcome {
    /// Returns the error kind of a refused or failed call.
    pub fn error_kind(&self) -> Option<LlmErrorKind> {
        match self {
            Self::Refused(_) => Some(LlmErrorKind::Incompatible),
            Self::Failed(failure) => Some(failure.kind()),
            Self::Completed | Self::Unterminated | Self::Cancelled => None,
        }
    }

    /// Returns why the dispatch layer refused the call, when it did.
    pub fn refusal_reasons(&self) -> Option<&[CompatibilityReason]> {
        match self {
            Self::Refused(reasons) => Some(reasons.as_slice()),
            _ => None,
        }
    }

    /// Returns why a validator rejected the call, when one did and the row records it.
    pub fn validation_reason(&self) -> Option<&ValidationReason> {
        match self {
            Self::Failed(CallFailure::ValidationFailed { reason }) => reason.as_ref(),
            _ => None,
        }
    }
}

/// Why a sent call failed, with what the row records of the failure beyond its kind. A refusal's
/// reasons live on [`CallDispatch::Refused`]; an `Incompatible` a stream reported carries its
/// kind only.
#[derive(Debug, Clone, PartialEq)]
pub enum CallFailure {
    /// A validator rejected the call.
    ValidationFailed {
        /// `None` when the failure came from the stream, which reports only its kind.
        reason: Option<ValidationReason>,
    },
    /// A failure the row records only by its kind.
    Other(OtherFailure),
}

impl CallFailure {
    /// Returns the failure of `kind` with nothing recorded beyond it.
    pub fn of_kind(kind: LlmErrorKind) -> Self {
        match kind {
            LlmErrorKind::ValidationFailed => Self::ValidationFailed { reason: None },
            kind => Self::Other(OtherFailure(kind)),
        }
    }

    pub fn kind(&self) -> LlmErrorKind {
        match self {
            Self::ValidationFailed { .. } => LlmErrorKind::ValidationFailed,
            Self::Other(OtherFailure(kind)) => *kind,
        }
    }
}

/// The kind of a failure that carries nothing more. It is never `ValidationFailed`, which has
/// its own [`CallFailure`] variant; only [`CallFailure::of_kind`] makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtherFailure(LlmErrorKind);

/// What the model answered.
#[derive(Debug, Clone)]
pub struct CallResponse {
    /// The serialized `ChatResponse`.
    pub body: CallBody,
    /// The model the provider reports having answered with; `None` when it did not say.
    pub actual_model: Option<String>,
    pub stop_reason: Option<StopReason>,
}

/// A request or response body. Its absence is a stated fact, never a null.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallBody {
    Present(String),
    /// The writer's policy keeps no bodies — a release build keeping its users' code
    /// and the model's answers off their disk. Every other column of the row is
    /// recorded as usual, the request's `request_sha256` included.
    Withheld,
    /// The body did not serialize; `error` is the serializer's message.
    Unserializable {
        error: String,
    },
}

impl CallBody {
    /// Returns the body text, when it was kept.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Present(text) => Some(text),
            Self::Withheld | Self::Unserializable { .. } => None,
        }
    }

    fn withheld(self) -> Self {
        match self {
            Self::Present(_) => Self::Withheld,
            body => body,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Present(text) => text.len(),
            Self::Withheld => 0,
            Self::Unserializable { error } => error.len(),
        }
    }
}

impl LogTable for LlmCallTable {
    type Row = LlmCallRow;
    type Key = CallId;

    const NAME: TableName = TableName::new("llm_call");
    const ROW_GROUP_ROWS: NonZeroUsize = ROW_GROUP_ROWS;

    fn sort_key(row: &LlmCallRow) -> CallId {
        row.call_id
    }

    fn approx_bytes(row: &LlmCallRow) -> usize {
        row.request_body.len() + row.response().map_or(0, |r| r.body.len())
    }
}

impl Partitioned for LlmCallTable {
    /// Returns the tenant and the call's own day, not the day the row was written.
    fn partition(row: &LlmCallRow) -> Partition {
        Partition {
            tenant: row.tenant.clone(),
            day: row.call_id.day(),
        }
    }
}

/// A call as a listing shows it, decoded without its bodies.
#[derive(Debug, Clone)]
pub struct LlmCallSummary {
    pub call_id: CallId,
    pub tenant: TenantId,
    pub model: String,
    /// `None` when there was no response, or the provider did not say which model answered.
    pub actual_model: Option<String>,
    pub outcome: CallOutcome,
    pub usage: Option<Usage>,
    pub provider_latency_ms: Option<u64>,
    pub tags: Vec<String>,
}

impl LlmCallSummary {
    /// Returns when the call started, the time its id was minted with.
    pub fn at_ms(&self) -> Millis {
        started_at(self.call_id)
    }

    pub fn of(row: &LlmCallRow) -> Self {
        Self {
            call_id: row.call_id,
            tenant: row.tenant.clone(),
            model: row.model.clone(),
            actual_model: row.response().and_then(|r| r.actual_model.clone()),
            outcome: row.outcome(),
            usage: row.usage(),
            provider_latency_ms: row.provider_latency_ms,
            tags: row.tags.clone(),
        }
    }
}

impl Projected for LlmCallTable {
    type Partial = LlmCallSummary;
    type Filter = TagFilter;

    fn partial(row: &LlmCallRow) -> LlmCallSummary {
        LlmCallSummary::of(row)
    }

    fn partial_key(call: &LlmCallSummary) -> CallId {
        call.call_id
    }

    fn partial_tenant(call: &LlmCallSummary) -> &TenantId {
        &call.tenant
    }

    fn keeps(filter: &TagFilter, call: &LlmCallSummary) -> bool {
        filter.keeps(&call.tags)
    }
}

/// Which calls a read keeps, by their cohort tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagFilter {
    Every,
    /// Only the calls whose `tags` contain this tag.
    Carrying(String),
}

impl TagFilter {
    pub fn keeps(&self, tags: &[String]) -> bool {
        match self {
            Self::Every => true,
            Self::Carrying(tag) => tags.contains(tag),
        }
    }
}

impl From<Option<String>> for TagFilter {
    fn from(tag: Option<String>) -> Self {
        tag.map_or(Self::Every, Self::Carrying)
    }
}

/// The `eval_score` table.
pub struct EvalScoreTable;

/// One evaluator's score of one call. Sorted and partitioned by the call it
/// scores, so a call's scores sit in its day. Its writer found the call in the call log before
/// writing it.
#[derive(Debug, Clone)]
pub struct EvalScoreRow {
    pub score_id: Uuid,
    pub call_id: CallId,
    pub tenant: TenantId,
    /// When the score was written (epoch ms).
    pub at_ms: Millis,
    pub evaluator: String,
    pub score: f64,
    pub explanation: Option<String>,
    pub tags: Vec<String>,
}

impl EvalScoreRow {
    /// Builds a score of `call`, which the writer found in the call log; the score takes the
    /// call's own tenant.
    pub fn of_call(
        call: &LlmCallRow,
        score_id: Uuid,
        at_ms: Millis,
        evaluator: String,
        score: f64,
        explanation: Option<String>,
        tags: Vec<String>,
    ) -> Self {
        Self {
            score_id,
            call_id: call.call_id,
            tenant: call.tenant.clone(),
            at_ms,
            evaluator,
            score,
            explanation,
            tags,
        }
    }
}

impl LogTable for EvalScoreTable {
    type Row = EvalScoreRow;
    type Key = CallId;

    const NAME: TableName = TableName::new("eval_score");
    const ROW_GROUP_ROWS: NonZeroUsize = ROW_GROUP_ROWS;

    fn sort_key(row: &EvalScoreRow) -> CallId {
        row.call_id
    }

    fn approx_bytes(row: &EvalScoreRow) -> usize {
        row.evaluator.len() + row.explanation.as_ref().map_or(0, String::len)
    }
}

impl Partitioned for EvalScoreTable {
    fn partition(row: &EvalScoreRow) -> Partition {
        Partition {
            tenant: row.tenant.clone(),
            day: row.call_id.day(),
        }
    }
}

/// Returns the tenant's newest calls in the scope's window that `filter` keeps, newest first and
/// at most `limit`. The port reads the window's days newest first and stops at the limit; a call
/// is in the window by its start, whose UTC day is its partition's (both are the call's start,
/// the time in its id). Narrowing to the window after the port's cut loses no call, because the
/// calls the window drops are older than every call it keeps.
pub async fn window(
    src: &dyn LogSource<LlmCallTable>,
    scope: &TenantScope,
    filter: &TagFilter,
    limit: Option<usize>,
) -> Result<Vec<LlmCallRow>, LogError> {
    let mut calls: Vec<LlmCallRow> = src
        .scan_newest(scope, filter, limit)
        .await?
        .into_iter()
        .filter(|r| scope.window().admits(r.at_ms()))
        .collect();
    calls.sort_by_key(|r| std::cmp::Reverse(r.call_id));
    Ok(calls)
}

/// Returns what [`window`] does, as summaries; no body is decoded.
pub async fn summaries(
    src: &dyn PartialSource<LlmCallTable>,
    scope: &TenantScope,
    filter: &TagFilter,
    limit: Option<usize>,
) -> Result<Vec<LlmCallSummary>, LogError> {
    let mut calls: Vec<LlmCallSummary> = src
        .scan_partial(scope, filter, limit)
        .await?
        .into_iter()
        .filter(|c| scope.window().admits(c.at_ms()))
        .collect();
    calls.sort_by_key(|c| std::cmp::Reverse(c.call_id));
    Ok(calls)
}

/// Returns where the tenant's call `id` and its scores are: the partition of the day its id names.
pub fn call_scope(tenant: &TenantId, id: CallId) -> TenantScope {
    TenantScope::new(tenant.clone(), TimeWindow::day(id.day()))
}

#[cfg(all(test, feature = "parquet"))]
pub(crate) mod tests;
