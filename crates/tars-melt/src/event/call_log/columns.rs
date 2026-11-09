//! The Parquet columns of `llm_call` and `eval_score`. Scalars a query groups
//! or sums on are typed columns; nested structures a query only displays
//! (`validation_summary`, `retry_attempts`) are JSON text.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type, Int64Type, UInt32Type, UInt64Type};
use arrow_array::{
    Array, ArrayRef, BooleanArray, FixedSizeBinaryArray, Float32Array, Float64Array, Int64Array,
    ListArray, RecordBatch, StringArray, StructArray, UInt32Array, UInt64Array,
};
use arrow_schema::{ArrowError, DataType, Field, FieldRef, Fields, Schema, SchemaRef};
use parquet::file::statistics::Statistics;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tars_types::{
    CallId, CatalogSource, Degradation, LlmErrorKind, OutputLimit, ProviderId, Usage,
};
use uuid::Uuid;

use super::{
    CallBody, CallDispatch, CallFailure, CallOutcome, CallResponse, EvalScoreRow, EvalScoreTable,
    LlmCallRow, LlmCallSummary, LlmCallTable, RefusalReasons, SentEnd, StreamEnd,
};
use crate::event::log::{
    ColumnError, DecodeError, EncodeError, OffsetLimit, ParquetProjected, ParquetTable,
    decode_rows, string_lists, strs,
};

pub(crate) const UUID_BYTES: i32 = 16;
const SHA256_BYTES: i32 = 32;

const OK: &str = "ok";
const ERROR: &str = "error";
const UNTERMINATED: &str = "unterminated";
const CANCELLED: &str = "cancelled";

const BODY_PRESENT: &str = "present";
const BODY_WITHHELD: &str = "withheld";
const BODY_UNSERIALIZABLE: &str = "unserializable";

const DEGRADED: &str = "degraded";
/// A JSON array of [`CompatibilityReason`](tars_types::CompatibilityReason)s, whose serde form that type keeps stable as a storage
/// format.
const REFUSAL_REASONS: &str = "refusal_reasons";
const VALIDATION_REASON: &str = "validation_reason";

const OUTPUT_LIMIT_TOKENS: &str = "output_limit_tokens";
const OUTPUT_LIMIT_SOURCE: &str = "output_limit_source";
const SOURCE_KIND: &str = "kind";
const SOURCE_AS_OF: &str = "as_of";

impl ParquetTable for LlmCallTable {
    const KEY_COLUMN: &'static str = "call_id";

    /// A column added here needs its one writer in `writers`.
    fn schema() -> SchemaRef {
        use DataType::{Boolean, Float32, Int64, UInt32, UInt64, Utf8};
        Arc::new(Schema::new(vec![
            Field::new("call_id", DataType::FixedSizeBinary(UUID_BYTES), false),
            Field::new("tenant", Utf8, false),
            Field::new("at_ms", Int64, false),
            Field::new("session_id", Utf8, true),
            Field::new("trace_id", Utf8, false),
            Field::new("provider_id", Utf8, true),
            Field::new("model", Utf8, false),
            Field::new("request_body", Utf8, true),
            Field::new("request_body_state", Utf8, false),
            Field::new(
                "request_sha256",
                DataType::FixedSizeBinary(SHA256_BYTES),
                false,
            ),
            Field::new("has_tools", Boolean, false),
            Field::new("has_thinking", Boolean, false),
            Field::new("has_structured_output", Boolean, false),
            Field::new("temperature", Float32, true),
            Field::new("max_output_tokens", UInt32, true),
            Field::new(OUTPUT_LIMIT_TOKENS, UInt32, true),
            Field::new(
                OUTPUT_LIMIT_SOURCE,
                DataType::Struct(limit_source_fields()),
                false,
            ),
            Field::new(DEGRADED, DataType::List(list_item()), true),
            Field::new("result", Utf8, false),
            Field::new("error_kind", Utf8, true),
            Field::new("response_body", Utf8, true),
            Field::new("response_body_state", Utf8, true),
            Field::new("actual_model", Utf8, true),
            Field::new("stop_reason", Utf8, true),
            Field::new("input_tokens", UInt64, true),
            Field::new("output_tokens", UInt64, true),
            Field::new("cached_input_tokens", UInt64, true),
            Field::new("cache_creation_tokens", UInt64, true),
            Field::new("thinking_tokens", UInt64, true),
            Field::new("retry_attempts", Utf8, false),
            Field::new("provider_latency_ms", UInt64, true),
            Field::new("pipeline_total_ms", UInt64, true),
            list_field("layers"),
            Field::new("validation_summary", Utf8, false),
            Field::new(VALIDATION_REASON, Utf8, true),
            Field::new(REFUSAL_REASONS, Utf8, true),
            list_field("tags"),
        ]))
    }

    fn encode_within(rows: &[LlmCallRow], limit: OffsetLimit) -> Result<RecordBatch, EncodeError> {
        let usage = |f: fn(&Usage) -> u64| -> ArrayRef {
            Arc::new(UInt64Array::from_iter(
                rows.iter().map(|r| r.usage().as_ref().map(f)),
            ))
        };
        let [request_body, request_state] = body_columns(
            limit,
            ["request_body", "request_body_state"],
            rows.iter().map(|r| Some(&r.request_body)),
        )?;
        let [response_body, response_state] = body_columns(
            limit,
            ["response_body", "response_body_state"],
            rows.iter().map(|r| r.response().map(|x| &x.body)),
        )?;
        let [limit_tokens, limit_source] =
            output_limit_columns(limit, rows.iter().map(|r| &r.output_limit))?;
        let outcomes: Vec<CallOutcome> = rows.iter().map(LlmCallRow::outcome).collect();

        let columns: Vec<ArrayRef> = vec![
            call_ids(rows.iter().map(|r| r.call_id))?,
            strs(
                limit,
                "tenant",
                rows.iter().map(|r| Some(r.tenant.as_str())),
            )?,
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(LlmCallRow::at_ms),
            )),
            {
                let sessions: Vec<Option<String>> = rows
                    .iter()
                    .map(|r| r.session_id.map(String::from))
                    .collect();
                strs(limit, "session_id", sessions.iter().map(Option::as_deref))?
            },
            strs(
                limit,
                "trace_id",
                rows.iter().map(|r| Some(r.trace_id.as_str())),
            )?,
            strs(
                limit,
                "provider_id",
                rows.iter().map(|r| r.provider_id().map(ProviderId::as_str)),
            )?,
            strs(limit, "model", rows.iter().map(|r| Some(r.model.as_str())))?,
            request_body,
            request_state,
            fixed(
                rows.iter().map(|r| r.request_sha256.as_slice()),
                SHA256_BYTES,
            )?,
            bools(rows.iter().map(|r| r.has_tools)),
            bools(rows.iter().map(|r| r.has_thinking)),
            bools(rows.iter().map(|r| r.has_structured_output)),
            Arc::new(Float32Array::from_iter(rows.iter().map(|r| r.temperature))),
            Arc::new(UInt32Array::from_iter(
                rows.iter().map(|r| r.max_output_tokens),
            )),
            limit_tokens,
            limit_source,
            degraded_column(limit, rows.iter().map(LlmCallRow::degraded))?,
            strs(
                limit,
                "result",
                outcomes.iter().map(|o| Some(result_word(o))),
            )?,
            strs(
                limit,
                "error_kind",
                outcomes
                    .iter()
                    .map(|o| o.error_kind().as_ref().map(LlmErrorKind::as_str)),
            )?,
            response_body,
            response_state,
            strs(
                limit,
                "actual_model",
                rows.iter()
                    .map(|r| r.response().map(|x| x.actual_model.as_str())),
            )?,
            strs(
                limit,
                "stop_reason",
                rows.iter().map(|r| {
                    r.response()
                        .and_then(|x| x.stop_reason)
                        .map(|reason| reason.as_str())
                }),
            )?,
            usage(|u| u.input_tokens),
            usage(|u| u.output_tokens),
            usage(|u| u.cached_input_tokens),
            usage(|u| u.cache_creation_tokens),
            usage(|u| u.thinking_tokens),
            json_col(
                limit,
                "retry_attempts",
                rows.iter().map(|r| Some(&r.retry_attempts)),
            )?,
            Arc::new(UInt64Array::from_iter(
                rows.iter().map(|r| r.provider_latency_ms),
            )),
            Arc::new(UInt64Array::from_iter(
                rows.iter().map(|r| r.pipeline_total_ms),
            )),
            lists(limit, "layers", rows.iter().map(|r| &r.layers))?,
            json_col(
                limit,
                "validation_summary",
                rows.iter().map(|r| Some(&r.validation_summary)),
            )?,
            json_col(
                limit,
                VALIDATION_REASON,
                outcomes.iter().map(CallOutcome::validation_reason),
            )?,
            json_col(
                limit,
                REFUSAL_REASONS,
                outcomes.iter().map(CallOutcome::refusal_reasons),
            )?,
            lists(limit, "tags", rows.iter().map(|r| &r.tags))?,
        ];
        Ok(RecordBatch::try_new(Self::schema(), columns)?)
    }

    fn decode(batch: &RecordBatch) -> Result<Vec<LlmCallRow>, DecodeError> {
        let c = Columns(batch);
        let call_id = c.fixed("call_id")?;
        let tenant = c.utf8("tenant")?;
        let session_id = c.utf8("session_id")?;
        let trace_id = c.utf8("trace_id")?;
        let provider_id = c.utf8("provider_id")?;
        let model = c.utf8("model")?;
        let request_body = c.body("request_body")?;
        let request_sha256 = c.fixed("request_sha256")?;
        let has_tools = c.bool("has_tools")?;
        let has_thinking = c.bool("has_thinking")?;
        let has_structured_output = c.bool("has_structured_output")?;
        let temperature = c.prim::<Float32Type>("temperature")?;
        let max_output_tokens = c.prim::<UInt32Type>("max_output_tokens")?;
        let output_limit = c.output_limit()?;
        let degraded = c.list(DEGRADED)?;
        let result = c.utf8("result")?;
        let error_kind = c.utf8("error_kind")?;
        let response_body = c.body("response_body")?;
        let actual_model = c.utf8("actual_model")?;
        let stop_reason = c.utf8("stop_reason")?;
        let usage = c.usage()?;
        let retry_attempts = c.utf8("retry_attempts")?;
        let provider_latency_ms = c.prim::<UInt64Type>("provider_latency_ms")?;
        let pipeline_total_ms = c.prim::<UInt64Type>("pipeline_total_ms")?;
        let layers = c.list("layers")?;
        let validation_summary = c.utf8("validation_summary")?;
        let validation_reason = c.utf8(VALIDATION_REASON)?;
        let refusal_reasons = c.utf8(REFUSAL_REASONS)?;
        let tags = c.list("tags")?;

        decode_rows(batch.num_rows(), |i| {
            let response = response_body
                .at(i)?
                .map(|body| -> Result<CallResponse, ColumnError> {
                    Ok(CallResponse {
                        body,
                        actual_model: opt_str(actual_model, i)
                            .ok_or_else(|| missing("actual_model"))?
                            .to_owned(),
                        stop_reason: opt_str(stop_reason, i).map(from_name).transpose()?,
                    })
                })
                .transpose()?;
            let outcome = outcome_at(
                result.value(i),
                opt_str(error_kind, i),
                opt_str(refusal_reasons, i),
                opt_str(validation_reason, i),
            )?;
            let dispatch = dispatch_at(Ended {
                outcome,
                degraded: degraded_at(degraded, i)?,
                provider_id: opt_str(provider_id, i).map(from_name).transpose()?,
                usage: usage.at(i)?,
                response,
            })?;
            Ok(LlmCallRow {
                call_id: call_id_at(call_id, i)?,
                tenant: from_name(tenant.value(i))?,
                session_id: opt_str(session_id, i).map(from_name).transpose()?,
                trace_id: from_name(opt_str(trace_id, i).ok_or_else(|| missing("trace_id"))?)?,
                model: model.value(i).to_owned(),
                request_body: request_body
                    .at(i)?
                    .ok_or_else(|| missing("request_body_state"))?,
                request_sha256: sha256_at(request_sha256, i)?,
                has_tools: has_tools.value(i),
                has_thinking: has_thinking.value(i),
                has_structured_output: has_structured_output.value(i),
                temperature: opt(temperature, i),
                max_output_tokens: opt(max_output_tokens, i),
                output_limit: output_limit.at(i)?,
                dispatch,
                retry_attempts: from_json(retry_attempts.value(i))?,
                provider_latency_ms: opt(provider_latency_ms, i),
                pipeline_total_ms: opt(pipeline_total_ms, i),
                layers: strings_at(layers, "layers", i)?,
                validation_summary: from_json(validation_summary.value(i))?,
                tags: strings_at(tags, "tags", i)?,
            })
        })
    }

    fn key_range(stats: &Statistics) -> Option<(CallId, CallId)> {
        call_id_range(stats)
    }
}

impl ParquetProjected for LlmCallTable {
    const PARTIAL_COLUMNS: &'static [&'static str] = &[
        "call_id",
        "tenant",
        "model",
        "actual_model",
        "result",
        "error_kind",
        REFUSAL_REASONS,
        VALIDATION_REASON,
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cache_creation_tokens",
        "thinking_tokens",
        "provider_latency_ms",
        "tags",
    ];

    fn decode_partial(batch: &RecordBatch) -> Result<Vec<LlmCallSummary>, DecodeError> {
        let c = Columns(batch);
        let call_id = c.fixed("call_id")?;
        let tenant = c.utf8("tenant")?;
        let model = c.utf8("model")?;
        let actual_model = c.utf8("actual_model")?;
        let result = c.utf8("result")?;
        let error_kind = c.utf8("error_kind")?;
        let refusal_reasons = c.utf8(REFUSAL_REASONS)?;
        let validation_reason = c.utf8(VALIDATION_REASON)?;
        let usage = c.usage()?;
        let provider_latency_ms = c.prim::<UInt64Type>("provider_latency_ms")?;
        let tags = c.list("tags")?;

        decode_rows(batch.num_rows(), |i| {
            Ok(LlmCallSummary {
                call_id: call_id_at(call_id, i)?,
                tenant: from_name(tenant.value(i))?,
                model: model.value(i).to_owned(),
                actual_model: opt_str(actual_model, i).map(str::to_owned),
                outcome: outcome_at(
                    result.value(i),
                    opt_str(error_kind, i),
                    opt_str(refusal_reasons, i),
                    opt_str(validation_reason, i),
                )?,
                usage: usage.at(i)?,
                provider_latency_ms: opt(provider_latency_ms, i),
                tags: strings_at(tags, "tags", i)?,
            })
        })
    }
}

impl ParquetTable for EvalScoreTable {
    const KEY_COLUMN: &'static str = "call_id";

    /// A column added here needs its one writer in `writers`.
    fn schema() -> SchemaRef {
        use DataType::{Float64, Int64, Utf8};
        Arc::new(Schema::new(vec![
            Field::new("call_id", DataType::FixedSizeBinary(UUID_BYTES), false),
            Field::new("score_id", DataType::FixedSizeBinary(UUID_BYTES), false),
            Field::new("tenant", Utf8, false),
            Field::new("at_ms", Int64, false),
            Field::new("evaluator", Utf8, false),
            Field::new("score", Float64, false),
            Field::new("explanation", Utf8, true),
            list_field("tags"),
        ]))
    }

    fn encode_within(
        rows: &[EvalScoreRow],
        limit: OffsetLimit,
    ) -> Result<RecordBatch, EncodeError> {
        let columns: Vec<ArrayRef> = vec![
            call_ids(rows.iter().map(|r| r.call_id))?,
            fixed(
                rows.iter().map(|r| r.score_id.as_bytes().as_slice()),
                UUID_BYTES,
            )?,
            strs(
                limit,
                "tenant",
                rows.iter().map(|r| Some(r.tenant.as_str())),
            )?,
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.at_ms))),
            strs(
                limit,
                "evaluator",
                rows.iter().map(|r| Some(r.evaluator.as_str())),
            )?,
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.score))),
            strs(
                limit,
                "explanation",
                rows.iter().map(|r| r.explanation.as_deref()),
            )?,
            lists(limit, "tags", rows.iter().map(|r| &r.tags))?,
        ];
        Ok(RecordBatch::try_new(Self::schema(), columns)?)
    }

    fn decode(batch: &RecordBatch) -> Result<Vec<EvalScoreRow>, DecodeError> {
        let c = Columns(batch);
        let call_id = c.fixed("call_id")?;
        let score_id = c.fixed("score_id")?;
        let tenant = c.utf8("tenant")?;
        let at_ms = c.prim::<Int64Type>("at_ms")?;
        let evaluator = c.utf8("evaluator")?;
        let score = c.prim::<Float64Type>("score")?;
        let explanation = c.utf8("explanation")?;
        let tags = c.list("tags")?;

        decode_rows(batch.num_rows(), |i| {
            Ok(EvalScoreRow {
                score_id: uuid_at(score_id, i)?,
                call_id: call_id_at(call_id, i)?,
                tenant: from_name(tenant.value(i))?,
                at_ms: at_ms.value(i),
                evaluator: evaluator.value(i).to_owned(),
                score: score.value(i),
                explanation: opt_str(explanation, i).map(str::to_owned),
                tags: strings_at(tags, "tags", i)?,
            })
        })
    }

    fn key_range(stats: &Statistics) -> Option<(CallId, CallId)> {
        call_id_range(stats)
    }
}

// ── output limit ────────────────────────────────────────────────────

/// `output_limit_source.kind`: where the ceiling came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LimitSourceKind {
    Configured,
    CatalogLive,
    CatalogShipped,
    Unknown,
}

impl LimitSourceKind {
    /// Returns the word serde writes, the column's value.
    fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::CatalogLive => "catalog_live",
            Self::CatalogShipped => "catalog_shipped",
            Self::Unknown => "unknown",
        }
    }
}

/// A limit's source as its two struct fields; `as_of` is the catalog's
/// `queried_at` (live) or `verified` (shipped) date.
type LimitSource<'a> = (LimitSourceKind, Option<&'a str>);

fn limit_source_fields() -> Fields {
    Fields::from(vec![
        Field::new(SOURCE_KIND, DataType::Utf8, false),
        Field::new(SOURCE_AS_OF, DataType::Utf8, true),
    ])
}

fn limit_source(limit: &OutputLimit) -> LimitSource<'_> {
    match limit {
        OutputLimit::Configured { .. } => (LimitSourceKind::Configured, None),
        OutputLimit::Model {
            source: CatalogSource::Live { queried_at },
            ..
        } => (LimitSourceKind::CatalogLive, Some(queried_at)),
        OutputLimit::Model {
            source: CatalogSource::Shipped { verified },
            ..
        } => (LimitSourceKind::CatalogShipped, Some(verified)),
        OutputLimit::Unknown => (LimitSourceKind::Unknown, None),
    }
}

fn output_limit_from(
    tokens: Option<u32>,
    source: LimitSource<'_>,
) -> Result<OutputLimit, ColumnError> {
    use LimitSourceKind as K;
    match (source, tokens) {
        ((K::Configured, None), Some(tokens)) => Ok(OutputLimit::Configured { tokens }),
        ((K::CatalogLive, Some(at)), Some(tokens)) => Ok(OutputLimit::Model {
            tokens,
            source: CatalogSource::Live {
                queried_at: at.to_owned(),
            },
        }),
        ((K::CatalogShipped, Some(at)), Some(tokens)) => Ok(OutputLimit::Model {
            tokens,
            source: CatalogSource::Shipped {
                verified: at.to_owned(),
            },
        }),
        ((K::Unknown, None), None) => Ok(OutputLimit::Unknown),
        (source, tokens) => Err(ColumnError::Inconsistent {
            columns: "output_limit_tokens and output_limit_source",
            found: format!("{OUTPUT_LIMIT_TOKENS}={tokens:?} {OUTPUT_LIMIT_SOURCE}={source:?}"),
        }),
    }
}

// ── outcome ─────────────────────────────────────────────────────────

/// Returns the `result` word of an outcome; a refused call is an error, its kind `incompatible`.
fn result_word(o: &CallOutcome) -> &'static str {
    match o {
        CallOutcome::Completed => OK,
        CallOutcome::Refused(_) | CallOutcome::Failed(_) => ERROR,
        CallOutcome::Unterminated => UNTERMINATED,
        CallOutcome::Cancelled => CANCELLED,
    }
}

/// Reads an outcome from its four columns; the two reason columns, JSON text, may hold a value
/// only on an outcome of their kind, and refusal reasons are never an empty list.
fn outcome_at(
    result: &str,
    kind: Option<&str>,
    refusal_reasons: Option<&str>,
    validation_reason: Option<&str>,
) -> Result<CallOutcome, ColumnError> {
    let inconsistent = || ColumnError::Inconsistent {
        columns: "result, error_kind, refusal_reasons and validation_reason",
        found: format!(
            "result={result:?} error_kind={kind:?} refusal_reasons={refusal_reasons:?} \
             validation_reason={validation_reason:?}"
        ),
    };
    match (result, kind, refusal_reasons, validation_reason) {
        (OK, None, None, None) => Ok(CallOutcome::Completed),
        (UNTERMINATED, None, None, None) => Ok(CallOutcome::Unterminated),
        (CANCELLED, None, None, None) => Ok(CallOutcome::Cancelled),
        (ERROR, Some(kind), refusal, validation) => match (from_name(kind)?, refusal, validation) {
            (LlmErrorKind::Incompatible, Some(reasons), None) => {
                RefusalReasons::new(from_json(reasons)?)
                    .map(CallOutcome::Refused)
                    .ok_or_else(inconsistent)
            }
            (LlmErrorKind::ValidationFailed, None, reason) => {
                Ok(CallOutcome::Failed(CallFailure::ValidationFailed {
                    reason: reason.map(from_json).transpose()?,
                }))
            }
            (kind, None, None) => Ok(CallOutcome::Failed(CallFailure::of_kind(kind))),
            _ => Err(inconsistent()),
        },
        _ => Err(inconsistent()),
    }
}

/// What a row's columns say of how its call ended, before they are checked against each other.
#[derive(Debug)]
struct Ended {
    outcome: CallOutcome,
    degraded: Option<Vec<Degradation>>,
    provider_id: Option<ProviderId>,
    usage: Option<Usage>,
    response: Option<CallResponse>,
}

/// Returns the dispatch the columns describe, or why they do not describe one.
fn dispatch_at(ended: Ended) -> Result<CallDispatch, ColumnError> {
    dispatch_of(ended).map_err(|ended| ColumnError::Inconsistent {
        columns: "result, degraded, provider_id, usage and response",
        found: format!(
            "outcome={:?} degraded={:?} provider_id={:?} usage={} response={}",
            ended.outcome,
            ended.degraded,
            ended.provider_id,
            if ended.usage.is_some() { "set" } else { "null" },
            if ended.response.is_some() {
                "set"
            } else {
                "null"
            },
        ),
    })
}

/// Returns the dispatch `ended` describes, or `ended` back when its parts do not fit: a refused
/// call has no degradations, provider, usage or response; a sent call has degradations (maybe
/// none), usage exactly when a stream was drained, and a response only from a drained stream.
fn dispatch_of(ended: Ended) -> Result<CallDispatch, Box<Ended>> {
    match ended {
        Ended {
            outcome: CallOutcome::Refused(reasons),
            degraded: None,
            provider_id: None,
            usage: None,
            response: None,
        } => Ok(CallDispatch::Refused { reasons }),
        Ended {
            outcome,
            degraded: Some(degraded),
            provider_id,
            usage,
            response,
        } => {
            let end = match (outcome, usage, response) {
                (CallOutcome::Completed, Some(usage), Some(response)) => SentEnd::Streamed {
                    usage,
                    end: StreamEnd::Completed(response),
                },
                (CallOutcome::Failed(failure), None, None) => SentEnd::FailedBeforeStream(failure),
                (CallOutcome::Failed(failure), Some(usage), response) => SentEnd::Streamed {
                    usage,
                    end: StreamEnd::Failed { failure, response },
                },
                (CallOutcome::Unterminated, Some(usage), None) => SentEnd::Streamed {
                    usage,
                    end: StreamEnd::Unterminated,
                },
                (CallOutcome::Cancelled, None, None) => SentEnd::Cancelled,
                (outcome, usage, response) => {
                    return Err(Box::new(Ended {
                        outcome,
                        degraded: Some(degraded),
                        provider_id,
                        usage,
                        response,
                    }));
                }
            };
            Ok(CallDispatch::Sent {
                degraded,
                provider_id,
                end,
            })
        }
        ended => Err(Box::new(ended)),
    }
}

// ── keys ────────────────────────────────────────────────────────────

/// Parquet orders FIXED_LEN_BYTE_ARRAY statistics as unsigned bytes, which
/// is `CallId`'s order.
pub(crate) fn call_id_range(stats: &Statistics) -> Option<(CallId, CallId)> {
    let Statistics::FixedLenByteArray(s) = stats else {
        return None;
    };
    let id = |v: &parquet::data_type::FixedLenByteArray| {
        Uuid::from_slice(v.data())
            .ok()
            .and_then(|u| CallId::try_from(u).ok())
    };
    Some((id(s.min_opt()?)?, id(s.max_opt()?)?))
}

pub(crate) fn call_ids(ids: impl Iterator<Item = CallId>) -> Result<ArrayRef, ArrowError> {
    let bytes: Vec<[u8; 16]> = ids.map(|id| Uuid::from(id).into_bytes()).collect();
    fixed(bytes.iter().map(<[u8; 16]>::as_slice), UUID_BYTES)
}

fn uuid_at(a: &FixedSizeBinaryArray, i: usize) -> Result<Uuid, ColumnError> {
    Uuid::from_slice(a.value(i)).map_err(ColumnError::Uuid)
}

pub(crate) fn call_id_at(a: &FixedSizeBinaryArray, i: usize) -> Result<CallId, ColumnError> {
    CallId::try_from(uuid_at(a, i)?).map_err(ColumnError::CallId)
}

fn sha256_at(a: &FixedSizeBinaryArray, i: usize) -> Result<[u8; 32], ColumnError> {
    <[u8; 32]>::try_from(a.value(i)).map_err(ColumnError::Sha256)
}

// ── encode helpers ──────────────────────────────────────────────────

/// `output_limit_tokens` (null where no ceiling is known) and
/// `output_limit_source` (never null).
fn output_limit_columns<'a>(
    limit: OffsetLimit,
    limits: impl Iterator<Item = &'a OutputLimit>,
) -> Result<[ArrayRef; 2], EncodeError> {
    let limits: Vec<&OutputLimit> = limits.collect();
    let sources: Vec<LimitSource> = limits.iter().map(|l| limit_source(l)).collect();

    let tokens = UInt32Array::from_iter(limits.iter().map(|l| l.tokens()));
    let source = StructArray::try_new(
        limit_source_fields(),
        vec![
            strs(
                limit,
                OUTPUT_LIMIT_SOURCE,
                sources.iter().map(|(kind, _)| Some(kind.as_str())),
            )?,
            strs(
                limit,
                OUTPUT_LIMIT_SOURCE,
                sources.iter().map(|(_, as_of)| *as_of),
            )?,
        ],
        None,
    )?;
    Ok([Arc::new(tokens), Arc::new(source)])
}

/// `<body>` and `<body>_state`, named by `[text, state]`; both null where there is no body (a
/// call without a response).
fn body_columns<'a>(
    limit: OffsetLimit,
    [text, state]: [&'static str; 2],
    bodies: impl Iterator<Item = Option<&'a CallBody>>,
) -> Result<[ArrayRef; 2], EncodeError> {
    let bodies: Vec<Option<&CallBody>> = bodies.collect();
    Ok([
        strs(
            limit,
            text,
            bodies.iter().map(|b| {
                b.and_then(|b| match b {
                    CallBody::Present(text) => Some(text.as_str()),
                    CallBody::Unserializable { error } => Some(error.as_str()),
                    CallBody::Withheld => None,
                })
            }),
        )?,
        strs(
            limit,
            state,
            bodies.iter().map(|b| {
                b.map(|b| match b {
                    CallBody::Present(_) => BODY_PRESENT,
                    CallBody::Withheld => BODY_WITHHELD,
                    CallBody::Unserializable { .. } => BODY_UNSERIALIZABLE,
                })
            }),
        )?,
    ])
}

fn list_field(name: &str) -> Field {
    Field::new(name, DataType::List(list_item()), false)
}

fn list_item() -> FieldRef {
    Arc::new(Field::new("item", DataType::Utf8, true))
}

pub(crate) fn bools(values: impl Iterator<Item = bool>) -> ArrayRef {
    Arc::new(BooleanArray::from_iter(values.map(Some)))
}

fn fixed<'a>(values: impl Iterator<Item = &'a [u8]>, size: i32) -> Result<ArrayRef, ArrowError> {
    Ok(Arc::new(
        FixedSizeBinaryArray::try_from_sparse_iter_with_size(values.map(Some), size)?,
    ))
}

fn lists<'a>(
    limit: OffsetLimit,
    column: &'static str,
    values: impl Iterator<Item = &'a Vec<String>>,
) -> Result<ArrayRef, EncodeError> {
    string_lists(
        limit,
        column,
        list_item(),
        values.map(|list| Some(list.as_slice())),
    )
}

/// Returns the `degraded` column: a list per row, empty when nothing was left out, and null where
/// the row does not record it (a refused call). Each item is a [`Degradation`]'s serde name, which
/// that type keeps stable as a storage format.
fn degraded_column<'a>(
    limit: OffsetLimit,
    rows: impl Iterator<Item = Option<&'a [Degradation]>>,
) -> Result<ArrayRef, EncodeError> {
    let names: Vec<Option<Vec<String>>> = rows
        .map(|degraded| degraded.map(|unsent| unsent.iter().map(|d| d.kind().to_owned()).collect()))
        .collect();
    string_lists(
        limit,
        DEGRADED,
        list_item(),
        names.iter().map(Option::as_deref),
    )
}

fn json_col<'a, V: Serialize + ?Sized + 'a>(
    limit: OffsetLimit,
    column: &'static str,
    values: impl Iterator<Item = Option<&'a V>>,
) -> Result<ArrayRef, EncodeError> {
    let texts: Vec<Option<String>> = values
        .map(|v| {
            v.map(serde_json::to_string)
                .transpose()
                .map_err(|source| EncodeError::Json { column, source })
        })
        .collect::<Result<_, _>>()?;
    strs(limit, column, texts.iter().map(Option::as_deref))
}

// ── decode helpers ──────────────────────────────────────────────────

pub(crate) struct Columns<'a>(pub(crate) &'a RecordBatch);

impl<'a> Columns<'a> {
    pub(crate) fn get(&self, name: &str) -> Result<&'a ArrayRef, ColumnError> {
        self.0
            .column_by_name(name)
            .ok_or_else(|| ColumnError::Missing {
                column: name.to_owned(),
            })
    }

    pub(crate) fn typed<A: ?Sized>(
        &self,
        name: &str,
        cast: impl FnOnce(&'a ArrayRef) -> Option<&'a A>,
    ) -> Result<&'a A, ColumnError> {
        let col = self.get(name)?;
        cast(col).ok_or_else(|| ColumnError::WrongType {
            column: name.to_owned(),
            found: col.data_type().clone(),
        })
    }

    pub(crate) fn utf8(&self, name: &str) -> Result<&'a StringArray, ColumnError> {
        self.typed(name, |c| c.as_string_opt::<i32>())
    }

    pub(crate) fn fixed(&self, name: &str) -> Result<&'a FixedSizeBinaryArray, ColumnError> {
        self.typed(name, |c| c.as_fixed_size_binary_opt())
    }

    pub(crate) fn bool(&self, name: &str) -> Result<&'a BooleanArray, ColumnError> {
        self.typed(name, |c| c.as_boolean_opt())
    }

    pub(crate) fn list(&self, name: &str) -> Result<&'a ListArray, ColumnError> {
        self.typed(name, |c| c.as_list_opt::<i32>())
    }

    fn body(&self, name: &str) -> Result<BodyColumns<'a>, ColumnError> {
        Ok(BodyColumns {
            name: name.to_owned(),
            text: self.utf8(name)?,
            state: self.utf8(&format!("{name}_state"))?,
        })
    }

    fn output_limit(&self) -> Result<LimitColumns<'a>, ColumnError> {
        let source: &StructArray = self.typed(OUTPUT_LIMIT_SOURCE, |c| c.as_struct_opt())?;
        let field = |name: &str| {
            source
                .column_by_name(name)
                .and_then(|c| c.as_string_opt::<i32>())
                .ok_or_else(|| ColumnError::Missing {
                    column: format!("{OUTPUT_LIMIT_SOURCE}.{name}"),
                })
        };
        Ok(LimitColumns {
            tokens: self.prim::<UInt32Type>(OUTPUT_LIMIT_TOKENS)?,
            kind: field(SOURCE_KIND)?,
            as_of: field(SOURCE_AS_OF)?,
        })
    }

    fn usage(&self) -> Result<UsageColumns<'a>, ColumnError> {
        Ok(UsageColumns {
            input: self.prim::<UInt64Type>("input_tokens")?,
            output: self.prim::<UInt64Type>("output_tokens")?,
            cached_input: self.prim::<UInt64Type>("cached_input_tokens")?,
            cache_creation: self.prim::<UInt64Type>("cache_creation_tokens")?,
            thinking: self.prim::<UInt64Type>("thinking_tokens")?,
        })
    }

    pub(crate) fn prim<T: arrow_array::ArrowPrimitiveType>(
        &self,
        name: &str,
    ) -> Result<&'a arrow_array::PrimitiveArray<T>, ColumnError> {
        self.typed(name, |c| c.as_primitive_opt::<T>())
    }
}

/// The five usage columns: all set, or all null where no stream was drained.
struct UsageColumns<'a> {
    input: &'a UInt64Array,
    output: &'a UInt64Array,
    cached_input: &'a UInt64Array,
    cache_creation: &'a UInt64Array,
    thinking: &'a UInt64Array,
}

impl UsageColumns<'_> {
    fn at(&self, i: usize) -> Result<Option<Usage>, ColumnError> {
        match (
            opt(self.input, i),
            opt(self.output, i),
            opt(self.cached_input, i),
            opt(self.cache_creation, i),
            opt(self.thinking, i),
        ) {
            (Some(input), Some(output), Some(cached), Some(creation), Some(thinking)) => {
                Ok(Some(Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cached_input_tokens: cached,
                    cache_creation_tokens: creation,
                    thinking_tokens: thinking,
                }))
            }
            (None, None, None, None, None) => Ok(None),
            partial => Err(ColumnError::Inconsistent {
                columns: "the usage columns",
                found: format!("{partial:?}"),
            }),
        }
    }
}

struct LimitColumns<'a> {
    tokens: &'a UInt32Array,
    kind: &'a StringArray,
    as_of: &'a StringArray,
}

impl LimitColumns<'_> {
    fn at(&self, i: usize) -> Result<OutputLimit, ColumnError> {
        let kind =
            opt_str(self.kind, i).ok_or_else(|| missing(&format!("{OUTPUT_LIMIT_SOURCE}.kind")))?;
        output_limit_from(
            opt(self.tokens, i),
            (from_name(kind)?, opt_str(self.as_of, i)),
        )
    }
}

struct BodyColumns<'a> {
    name: String,
    text: &'a StringArray,
    state: &'a StringArray,
}

impl BodyColumns<'_> {
    /// `None` ⇔ this row has no such body.
    fn at(&self, i: usize) -> Result<Option<CallBody>, ColumnError> {
        let text = opt_str(self.text, i).map(str::to_owned);
        match (opt_str(self.state, i), text) {
            (None, None) => Ok(None),
            (Some(BODY_PRESENT), Some(text)) => Ok(Some(CallBody::Present(text))),
            (Some(BODY_WITHHELD), None) => Ok(Some(CallBody::Withheld)),
            (Some(BODY_UNSERIALIZABLE), Some(error)) => {
                Ok(Some(CallBody::Unserializable { error }))
            }
            (state, text) => Err(ColumnError::Inconsistent {
                columns: "a body's state and text",
                found: format!(
                    "`{}_state` = {state:?} with {} text",
                    self.name,
                    if text.is_some() { "a" } else { "no" },
                ),
            }),
        }
    }
}

/// Reads a null as a row that does not record what the request was sent without, which is a
/// refused call. A name this build does not know fails the row, naming it: a build that knew the
/// name wrote it.
fn degraded_at(degraded: &ListArray, i: usize) -> Result<Option<Vec<Degradation>>, ColumnError> {
    if !degraded.is_valid(i) {
        return Ok(None);
    }
    Ok(Some(
        strings_at(degraded, DEGRADED, i)?
            .iter()
            .map(|name| {
                from_name_or_err::<Degradation>(name).map_err(|source| ColumnError::UnknownName {
                    column: DEGRADED,
                    value: name.clone(),
                    source,
                })
            })
            .collect::<Result<_, _>>()?,
    ))
}

pub(crate) fn opt<T: arrow_array::ArrowPrimitiveType>(
    a: &arrow_array::PrimitiveArray<T>,
    i: usize,
) -> Option<T::Native> {
    a.is_valid(i).then(|| a.value(i))
}

pub(crate) fn opt_str(a: &StringArray, i: usize) -> Option<&str> {
    a.is_valid(i).then(|| a.value(i))
}

/// Returns row `i` of the text list column `column`; no writer of ours puts a null in one, so a
/// null item fails the row.
fn strings_at(a: &ListArray, column: &str, i: usize) -> Result<Vec<String>, ColumnError> {
    let values = a.value(i);
    let strings = values
        .as_string_opt::<i32>()
        .ok_or_else(|| ColumnError::WrongItemType {
            column: column.to_owned(),
            found: values.data_type().clone(),
        })?;
    strings
        .iter()
        .map(|item| {
            item.map(str::to_owned)
                .ok_or_else(|| ColumnError::NullItem {
                    column: column.to_owned(),
                })
        })
        .collect()
}

fn from_json<V: DeserializeOwned>(text: &str) -> Result<V, ColumnError> {
    serde_json::from_str(text).map_err(ColumnError::Value)
}

/// Parses a bare string through its type's `Deserialize`, the one fallible way into a unit enum or
/// a non-empty id.
pub(crate) fn from_name<V: DeserializeOwned>(name: &str) -> Result<V, ColumnError> {
    from_name_or_err(name).map_err(ColumnError::Value)
}

fn from_name_or_err<V: DeserializeOwned>(name: &str) -> Result<V, serde_json::Error> {
    serde_json::from_value(serde_json::Value::String(name.to_owned()))
}

pub(crate) fn missing(column: &str) -> ColumnError {
    ColumnError::Null {
        column: column.to_owned(),
    }
}

/// The writer of each column (`docs/design/melt-parquet.md` §4.1): every column of a source table
/// has exactly one named writer, and a column added to `schema()` without an entry here fails the
/// contract test.
#[cfg(test)]
pub(super) mod writers {
    use super::{
        DEGRADED, OUTPUT_LIMIT_SOURCE, OUTPUT_LIMIT_TOKENS, REFUSAL_REASONS, VALIDATION_REASON,
    };

    /// Who writes an `llm_call` column.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum LlmCallWriter {
        /// `EventEmitterMiddleware`, from what it holds when the call starts.
        AtStart,
        /// `EventEmitterMiddleware`, from how the call ended.
        AtEnd,
        /// `EventEmitterMiddleware`, from the call's telemetry accumulator.
        FromTelemetry,
    }

    pub(crate) const LLM_CALL: &[(&str, LlmCallWriter)] = {
        use LlmCallWriter::{AtEnd, AtStart, FromTelemetry};
        &[
            ("call_id", AtStart),
            ("tenant", AtStart),
            ("at_ms", AtStart),
            ("session_id", AtStart),
            ("trace_id", AtStart),
            ("provider_id", FromTelemetry),
            ("model", AtStart),
            ("request_body", AtStart),
            ("request_body_state", AtStart),
            ("request_sha256", AtStart),
            ("has_tools", AtStart),
            ("has_thinking", AtStart),
            ("has_structured_output", AtStart),
            ("temperature", AtStart),
            ("max_output_tokens", AtStart),
            (OUTPUT_LIMIT_TOKENS, AtStart),
            (OUTPUT_LIMIT_SOURCE, AtStart),
            (DEGRADED, AtStart),
            ("result", AtEnd),
            ("error_kind", AtEnd),
            ("response_body", AtEnd),
            ("response_body_state", AtEnd),
            ("actual_model", AtEnd),
            ("stop_reason", AtEnd),
            ("input_tokens", AtEnd),
            ("output_tokens", AtEnd),
            ("cached_input_tokens", AtEnd),
            ("cache_creation_tokens", AtEnd),
            ("thinking_tokens", AtEnd),
            ("retry_attempts", FromTelemetry),
            ("provider_latency_ms", FromTelemetry),
            ("pipeline_total_ms", FromTelemetry),
            ("layers", FromTelemetry),
            ("validation_summary", AtEnd),
            (VALIDATION_REASON, AtEnd),
            (REFUSAL_REASONS, AtEnd),
            ("tags", AtStart),
        ]
    };

    /// Who writes an `eval_score` column.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum EvalScoreWriter {
        /// tars-py `write_score`, every column of the row.
        RowsOwnWriter,
    }

    pub(crate) const EVAL_SCORE: &[(&str, EvalScoreWriter)] = {
        use EvalScoreWriter::RowsOwnWriter;
        &[
            ("call_id", RowsOwnWriter),
            ("score_id", RowsOwnWriter),
            ("tenant", RowsOwnWriter),
            ("at_ms", RowsOwnWriter),
            ("evaluator", RowsOwnWriter),
            ("score", RowsOwnWriter),
            ("explanation", RowsOwnWriter),
            ("tags", RowsOwnWriter),
        ]
    };
}

#[cfg(test)]
mod tests {
    use tars_types::CompatibilityReason;

    use super::*;

    const REASONS: &str = r#"[{"kind":"vision"}]"#;
    const REASON: &str = r#"{"not_empty":{"field":"text"}}"#;

    #[test]
    fn refusal_reasons_read_as_a_refusal() {
        let got = outcome_at(ERROR, Some("incompatible"), Some(REASONS), None).unwrap();
        assert_eq!(
            got,
            CallOutcome::Refused(
                RefusalReasons::new(vec![CompatibilityReason::VisionUnsupported]).unwrap()
            )
        );
    }

    #[test]
    fn an_empty_list_of_refusal_reasons_is_inconsistent() {
        let got = outcome_at(ERROR, Some("incompatible"), Some("[]"), None);
        assert!(
            matches!(got, Err(ColumnError::Inconsistent { .. })),
            "{got:?}"
        );
    }

    #[test]
    fn an_incompatible_failure_without_reasons_is_a_failure_of_that_kind() {
        let got = outcome_at(ERROR, Some("incompatible"), None, None).unwrap();
        assert_eq!(
            got,
            CallOutcome::Failed(CallFailure::of_kind(LlmErrorKind::Incompatible))
        );
    }

    fn ended(outcome: CallOutcome, degraded: Option<Vec<Degradation>>, usage: bool) -> Ended {
        Ended {
            outcome,
            degraded,
            provider_id: None,
            usage: usage.then(Usage::default),
            response: None,
        }
    }

    #[test]
    fn a_refusal_with_degradations_recorded_is_inconsistent() {
        let refused = || {
            CallOutcome::Refused(
                RefusalReasons::new(vec![CompatibilityReason::VisionUnsupported]).unwrap(),
            )
        };
        assert!(dispatch_of(ended(refused(), None, false)).is_ok());
        assert!(dispatch_of(ended(refused(), Some(Vec::new()), false)).is_err());
    }

    #[test]
    fn a_failure_with_usage_failed_in_its_stream_and_without_failed_before_it() {
        let failed = || CallOutcome::Failed(CallFailure::of_kind(LlmErrorKind::Network));
        assert!(matches!(
            dispatch_of(ended(failed(), Some(Vec::new()), false)),
            Ok(CallDispatch::Sent {
                end: SentEnd::FailedBeforeStream(_),
                ..
            })
        ));
        assert!(matches!(
            dispatch_of(ended(failed(), Some(Vec::new()), true)),
            Ok(CallDispatch::Sent {
                end: SentEnd::Streamed {
                    end: StreamEnd::Failed { response: None, .. },
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn a_sent_call_without_its_degradations_recorded_is_inconsistent() {
        assert!(dispatch_of(ended(CallOutcome::Cancelled, Some(Vec::new()), false)).is_ok());
        assert!(dispatch_of(ended(CallOutcome::Cancelled, None, false)).is_err());
    }

    #[test]
    fn refusal_reasons_on_a_failure_of_another_kind_are_inconsistent() {
        let got = outcome_at(ERROR, Some("network"), Some(REASONS), None);
        assert!(
            matches!(got, Err(ColumnError::Inconsistent { .. })),
            "{got:?}"
        );
    }

    #[test]
    fn a_validation_reason_reads_onto_a_validation_failure() {
        let got = outcome_at(ERROR, Some("validation_failed"), None, Some(REASON)).unwrap();
        assert!(
            matches!(
                got,
                CallOutcome::Failed(CallFailure::ValidationFailed { reason: Some(_) })
            ),
            "{got:?}"
        );
    }

    #[test]
    fn a_validation_reason_on_a_refusal_is_inconsistent() {
        let got = outcome_at(ERROR, Some("incompatible"), None, Some(REASON));
        assert!(
            matches!(got, Err(ColumnError::Inconsistent { .. })),
            "{got:?}"
        );
    }

    #[test]
    fn a_reason_on_a_completed_call_is_inconsistent() {
        assert!(outcome_at(OK, None, None, None).is_ok());
        let got = outcome_at(OK, None, Some(REASONS), None);
        assert!(
            matches!(got, Err(ColumnError::Inconsistent { .. })),
            "{got:?}"
        );
    }

    #[test]
    fn a_value_that_does_not_serialize_names_its_column() {
        let unserializable = std::collections::BTreeMap::from([((1, 2), 3)]);
        let got = json_col(
            OffsetLimit::ARROW,
            VALIDATION_REASON,
            std::iter::once(Some(&unserializable)),
        );
        assert!(
            matches!(
                got,
                Err(EncodeError::Json {
                    column: VALIDATION_REASON,
                    ..
                })
            ),
            "{got:?}"
        );
    }

    #[test]
    fn a_failure_of_another_kind_reads_with_nothing_beyond_its_kind() {
        let got = outcome_at(ERROR, Some("network"), None, None).unwrap();
        assert_eq!(
            got,
            CallOutcome::Failed(CallFailure::of_kind(LlmErrorKind::Network))
        );
    }
}
