use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::UInt32Type;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{Field, Schema};
use tars_types::{
    CatalogSource, LlmErrorKind, ProviderId, SessionId, StopReason, TenantId, TraceId, Usage,
    ValidationSummary,
};
use tokio::runtime::Handle;

use super::*;
use crate::event::log::{
    ColumnError, DecodeError, EncodeError, LogSink, LogSource, OffsetLimit, ParquetLog,
    ParquetProjected, PartialSource, TenantScope, TimeWindow, probe,
};

const DAY_MS: Millis = 86_400_000;
/// 2026-09-28T00:00:00Z.
const MIDNIGHT: Millis = 1_790_553_600_000;

/// Returns the dispatch of a call that ended as `outcome`: a completed call ran on provider `p`,
/// an unterminated one drained a stream, a failed one failed before its stream opened.
fn dispatch(outcome: CallOutcome) -> CallDispatch {
    let sent = |provider_id, end| CallDispatch::Sent {
        degraded: Vec::new(),
        provider_id,
        end,
    };
    let usage = Usage {
        input_tokens: 10,
        output_tokens: 2,
        cached_input_tokens: 4,
        cache_creation_tokens: 1,
        thinking_tokens: 0,
    };
    match outcome {
        CallOutcome::Completed => sent(
            Some(ProviderId::new("p")),
            SentEnd::Streamed {
                usage: Some(usage),
                end: StreamEnd::Completed(CallResponse {
                    body: CallBody::Present(r#"{"text":"hi"}"#.into()),
                    actual_model: Some("m-2026".into()),
                    stop_reason: Some(StopReason::EndTurn),
                }),
            },
        ),
        CallOutcome::Refused(reasons) => CallDispatch::Refused { reasons },
        CallOutcome::Failed(failure) => sent(None, SentEnd::FailedBeforeStream(failure)),
        CallOutcome::Unterminated => sent(
            None,
            SentEnd::Streamed {
                usage: Some(usage),
                end: StreamEnd::Unterminated,
            },
        ),
        CallOutcome::Cancelled => sent(None, SentEnd::Cancelled),
    }
}

pub(crate) fn row(tenant: &str, at_ms: Millis, outcome: CallOutcome) -> LlmCallRow {
    LlmCallRow {
        call_id: CallId::new_v7(at_ms).unwrap(),
        tenant: tenant.parse::<TenantId>().unwrap(),
        session_id: Some(SessionId::new_v7(1_790_553_600_000).unwrap()),
        trace_id: TraceId::new("tr"),
        model: "m".into(),
        request_body: CallBody::Present(format!(r#"{{"at":{at_ms}}}"#)),
        request_sha256: [7; 32],
        has_tools: false,
        has_thinking: true,
        has_structured_output: false,
        temperature: Some(0.5),
        max_output_tokens: None,
        output_limit: OutputLimit::Configured { tokens: 100 },
        dispatch: dispatch(outcome),
        retry_attempts: Vec::new(),
        provider_latency_ms: Some(12),
        pipeline_total_ms: None,
        layers: vec!["event_emitter".into(), "telemetry".into()],
        validation_summary: ValidationSummary::default(),
        tags: vec!["dogfood".into()],
    }
}

/// Returns every `.parquet` under `root`, relative to it.
fn parquet_files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                let rel = path.strip_prefix(root).unwrap().parent().unwrap();
                out.push(rel.display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[tokio::test]
async fn one_batch_splits_into_tenant_and_day_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let log = ParquetLog::<LlmCallTable>::open(dir.path());
    let rows = [
        row("acme", MIDNIGHT - 1, CallOutcome::Completed),
        row(
            "acme",
            MIDNIGHT,
            CallOutcome::Failed(CallFailure::of_kind(LlmErrorKind::RateLimited)),
        ),
        row("a/b", MIDNIGHT + DAY_MS / 2, CallOutcome::Cancelled),
        row("acme", MIDNIGHT + 1, CallOutcome::Unterminated),
    ];

    let sink = log.sink(CALL_LOG_SINK, Some(CALL_LOG_COMPACTION), &Handle::current());
    for r in rows.clone() {
        sink.append(r).await.unwrap();
    }
    sink.close().await.unwrap();

    assert_eq!(
        parquet_files(dir.path()),
        vec![
            "tenant=a%2Fb/llm_call/date=2026-09-28",
            "tenant=acme/llm_call/date=2026-09-27",
            "tenant=acme/llm_call/date=2026-09-28",
        ]
    );
    let mut read = Vec::new();
    for tenant in ["acme", "a/b"] {
        let scope = TenantScope::new(TenantId::new(tenant), TimeWindow::all());
        read.extend(log.scan(&scope).await.unwrap());
    }
    assert_eq!(read.len(), rows.len());
}

#[tokio::test]
async fn lookup_by_call_id_returns_the_row_with_both_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let log = ParquetLog::<LlmCallTable>::open(dir.path());
    // More than one row group in one file, so pruning has groups to skip.
    let rows: Vec<LlmCallRow> = (0..40)
        .map(|i| row("acme", MIDNIGHT + i, CallOutcome::Completed))
        .collect();
    let sink = log.sink(CALL_LOG_SINK, Some(CALL_LOG_COMPACTION), &Handle::current());
    for r in rows.clone() {
        sink.append(r).await.unwrap();
    }
    sink.close().await.unwrap();

    let want = &rows[23];
    let found = log
        .scan_key(&call_scope(&want.tenant, want.call_id), &want.call_id)
        .await
        .unwrap();

    assert_eq!(found.len(), 1);
    let got = &found[0];
    assert_eq!(got.call_id, want.call_id);
    assert_eq!(got.request_body, want.request_body);
    let response = got.response().expect("completed call has a response");
    assert_eq!(response.body, CallBody::Present(r#"{"text":"hi"}"#.into()));
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(got.usage().map(|u| u.cache_creation_tokens), Some(1));
    assert_eq!(got.layers, want.layers);
}

#[tokio::test]
async fn failed_and_cancelled_calls_round_trip_without_a_response() {
    let dir = tempfile::tempdir().unwrap();
    let log = ParquetLog::<LlmCallTable>::open(dir.path());
    let failed = row(
        "t",
        MIDNIGHT,
        CallOutcome::Failed(CallFailure::of_kind(LlmErrorKind::Network)),
    );
    let cancelled = row("t", MIDNIGHT + 1, CallOutcome::Cancelled);
    let sink = log.sink(CALL_LOG_SINK, Some(CALL_LOG_COMPACTION), &Handle::current());
    sink.append(failed.clone()).await.unwrap();
    sink.append(cancelled.clone()).await.unwrap();
    sink.close().await.unwrap();

    let get = |id| {
        let log = &log;
        async move {
            let scope = call_scope(&TenantId::new("t"), id);
            log.scan_key(&scope, &id).await.unwrap().remove(0)
        }
    };
    let f = get(failed.call_id).await;
    let c = get(cancelled.call_id).await;

    assert_eq!(
        f.outcome(),
        CallOutcome::Failed(CallFailure::of_kind(LlmErrorKind::Network))
    );
    assert_eq!(c.outcome(), CallOutcome::Cancelled);
    assert!(f.response().is_none() && c.response().is_none());
    assert!(f.usage().is_none() && c.usage().is_none());
}

#[tokio::test]
async fn eval_scores_sit_in_their_calls_partition() {
    let dir = tempfile::tempdir().unwrap();
    let log = ParquetLog::<EvalScoreTable>::open(dir.path());
    let call_id = CallId::new_v7(MIDNIGHT - 1).unwrap();
    let score = EvalScoreRow {
        score_id: Uuid::new_v4(),
        call_id,
        tenant: TenantId::new("acme"),
        at_ms: MIDNIGHT + DAY_MS,
        evaluator: "exact".into(),
        score: 0.75,
        explanation: Some("close".into()),
        tags: vec![],
    };
    let sink = log.sink(CALL_LOG_SINK, Some(CALL_LOG_COMPACTION), &Handle::current());
    sink.append(score.clone()).await.unwrap();
    sink.close().await.unwrap();

    assert_eq!(
        parquet_files(dir.path()),
        vec!["tenant=acme/eval_score/date=2026-09-27"]
    );
    let got = log
        .scan_key(&call_scope(&score.tenant, call_id), &call_id)
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].score_id, score.score_id);
    assert_eq!(got[0].explanation.as_deref(), Some("close"));
}

/// A file without a column this build writes, such as one sealed before the column existed,
/// does not read: the error names the column.
#[test]
fn a_file_missing_a_column_fails_naming_the_column() {
    use crate::event::log::ParquetTable;

    let rows = [row("t", MIDNIGHT, CallOutcome::Completed)];
    let batch = LlmCallTable::encode(&rows).unwrap();
    let schema = batch.schema();
    let without: Vec<usize> = (0..schema.fields().len())
        .filter(|&i| schema.field(i).name() != "request_body_state")
        .collect();
    let old = through_a_parquet_file(&batch.project(&without).unwrap());

    let err = LlmCallTable::decode(&old).unwrap_err();
    assert!(
        matches!(&err, DecodeError::Columns(ColumnError::Missing { column }) if column == "request_body_state"),
        "{err:?}"
    );
}

/// A row whose writer withheld its bodies reads back withheld through a Parquet file, and keeps
/// every other column, the request's hash included.
#[test]
fn withheld_bodies_round_trip_as_withheld() {
    use crate::event::log::ParquetTable;

    let kept = row("t", MIDNIGHT, CallOutcome::Completed);
    let withheld = row("t", MIDNIGHT + 1, CallOutcome::Completed).withhold_bodies();
    let batch = through_a_parquet_file(&LlmCallTable::encode(&[kept, withheld.clone()]).unwrap());
    let got = LlmCallTable::decode(&batch).unwrap();
    assert_eq!(got[1].request_body, CallBody::Withheld);
    assert_eq!(
        got[1].response().map(|r| &r.body),
        Some(&CallBody::Withheld)
    );
    assert_eq!(
        got[1].request_sha256, withheld.request_sha256,
        "the request is still identified"
    );
    assert_eq!(
        (got[1].model.as_str(), got[1].outcome()),
        (withheld.model.as_str(), withheld.outcome())
    );
    assert_eq!(
        got[1].usage().is_some(),
        withheld.usage().is_some(),
        "usage is recorded as usual"
    );
    assert!(
        matches!(got[0].request_body, CallBody::Present(_)),
        "a kept body is still kept"
    );
}

#[test]
fn degraded_is_a_list_of_what_was_left_out_empty_when_nothing_and_null_on_a_refusal() {
    use crate::event::log::ParquetTable;
    use tars_types::Degradation;

    let both = vec![
        Degradation::StructuredOutputNotEnforced,
        Degradation::ChatTemplateThinkingNotSent,
    ];
    let written = [Some(both), Some(Vec::new()), None];
    let refused = || {
        CallOutcome::Refused(
            NonEmpty::from_vec(vec![CompatibilityReason::VisionUnsupported]).unwrap(),
        )
    };
    let rows: Vec<LlmCallRow> = written
        .iter()
        .zip(0..)
        .map(|(degraded, i)| match degraded {
            Some(degraded) => LlmCallRow {
                dispatch: CallDispatch::Sent {
                    degraded: degraded.clone(),
                    provider_id: None,
                    end: SentEnd::Cancelled,
                },
                ..row("t", MIDNIGHT + i, CallOutcome::Cancelled)
            },
            None => row("t", MIDNIGHT + i, refused()),
        })
        .collect();
    let batch = through_a_parquet_file(&LlmCallTable::encode(&rows).unwrap());

    let column = batch.column_by_name("degraded").unwrap().as_list::<i32>();
    let texts: Vec<Option<Vec<String>>> = (0..column.len())
        .map(|i| {
            column.is_valid(i).then(|| {
                column
                    .value(i)
                    .as_string::<i32>()
                    .iter()
                    .flatten()
                    .map(str::to_owned)
                    .collect()
            })
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            Some(vec![
                "structured_output".to_owned(),
                "chat_template_thinking".to_owned()
            ]),
            Some(Vec::new()),
            None,
        ]
    );

    let got: Vec<Option<Vec<Degradation>>> = LlmCallTable::decode(&batch)
        .unwrap()
        .iter()
        .map(|r| r.degraded().map(<[Degradation]>::to_vec))
        .collect();
    assert_eq!(got, written);
}

#[test]
fn a_refused_call_s_reasons_round_trip_through_a_parquet_file_and_a_served_call_has_none() {
    use crate::event::log::ParquetTable;

    let reasons = vec![
        CompatibilityReason::ToolUseUnsupported { tool_count: 2 },
        CompatibilityReason::ContextWindowExceeded {
            estimated_prompt_tokens: 9,
            max_context_tokens: 8,
        },
    ];
    let refused = CallOutcome::Refused(NonEmpty::from_vec(reasons).unwrap());
    let rows = [
        row("t", MIDNIGHT, refused.clone()),
        row("t", MIDNIGHT + 1, CallOutcome::Completed),
    ];

    let read = through_a_parquet_file(&LlmCallTable::encode(&rows).unwrap());
    let got: Vec<CallOutcome> = LlmCallTable::decode(&read)
        .unwrap()
        .into_iter()
        .map(|r| r.outcome())
        .collect();
    assert_eq!(got, vec![refused, CallOutcome::Completed]);
}

#[test]
fn a_rejected_call_s_validation_reason_round_trips_through_a_parquet_file() {
    use crate::event::log::ParquetTable;

    let rejected = CallOutcome::Failed(CallFailure::ValidationFailed {
        reason: Some(ValidationReason::NotEmpty {
            field: "text".into(),
        }),
    });
    let rows = [row("t", MIDNIGHT, rejected.clone())];

    let read = through_a_parquet_file(&LlmCallTable::encode(&rows).unwrap());
    let got = LlmCallTable::decode(&read).unwrap();
    assert_eq!(got[0].outcome(), rejected);
}

/// Returns `batch` with its `degraded` column replaced by `column`.
fn with_degraded(batch: &RecordBatch, column: ArrayRef) -> RecordBatch {
    let schema = batch.schema();
    let at = schema.index_of("degraded").unwrap();
    let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
    let mut columns = batch.columns().to_vec();
    fields[at] = Field::new("degraded", column.data_type().clone(), true);
    columns[at] = column;
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

fn one_degraded_list(items: &[Option<&str>]) -> ArrayRef {
    use arrow_array::builder::{ListBuilder, StringBuilder};
    let mut b = ListBuilder::new(StringBuilder::new());
    for item in items {
        b.values().append_option(*item);
    }
    b.append(true);
    Arc::new(b.finish())
}

#[test]
fn a_degradation_name_this_build_does_not_know_fails_the_row_naming_column_row_and_name() {
    use crate::event::log::ParquetTable;

    let rows = [row("t", MIDNIGHT, CallOutcome::Completed)];
    let encoded = LlmCallTable::encode(&rows).unwrap();
    let known = with_degraded(&encoded, one_degraded_list(&[Some("structured_output")]));
    let unknown = with_degraded(&encoded, one_degraded_list(&[Some("tool_choice")]));

    assert_eq!(
        LlmCallTable::decode(&through_a_parquet_file(&known)).unwrap()[0].degraded(),
        Some(&[tars_types::Degradation::StructuredOutputNotEnforced][..])
    );
    let err = tars_utils::display_chain(
        &LlmCallTable::decode(&through_a_parquet_file(&unknown)).unwrap_err(),
    );
    assert!(
        err.contains("row 0") && err.contains("`degraded`") && err.contains("`tool_choice`"),
        "{err}"
    );
}

#[test]
fn a_null_item_in_a_text_list_column_fails_the_row_instead_of_being_dropped() {
    use crate::event::log::ParquetTable;

    let rows = [row("t", MIDNIGHT, CallOutcome::Completed)];
    let encoded = LlmCallTable::encode(&rows).unwrap();
    let whole = with_degraded(&encoded, one_degraded_list(&[Some("structured_output")]));
    let holed = with_degraded(
        &encoded,
        one_degraded_list(&[None, Some("structured_output")]),
    );

    assert!(LlmCallTable::decode(&through_a_parquet_file(&whole)).is_ok());
    let err = tars_utils::display_chain(
        &LlmCallTable::decode(&through_a_parquet_file(&holed)).unwrap_err(),
    );
    assert!(
        err.contains("row 0") && err.contains("`degraded` holds a null item"),
        "{err}"
    );
}

#[test]
fn only_a_sent_call_gives_what_its_request_was_sent_without() {
    let unsent = vec![tars_types::Degradation::ChatTemplateThinkingNotSent];
    let sent = LlmCallRow {
        dispatch: CallDispatch::Sent {
            degraded: unsent.clone(),
            provider_id: None,
            end: SentEnd::Cancelled,
        },
        ..row("t", MIDNIGHT, CallOutcome::Cancelled)
    };
    let refused = row(
        "t",
        MIDNIGHT,
        CallOutcome::Refused(
            NonEmpty::from_vec(vec![CompatibilityReason::VisionUnsupported]).unwrap(),
        ),
    );
    assert_eq!(sent.degraded(), Some(unsent.as_slice()));
    assert_eq!(
        row("t", MIDNIGHT, CallOutcome::Cancelled).degraded(),
        Some(&[][..])
    );
    assert_eq!(refused.degraded(), None);
}

/// Writes `batch` to a real Parquet file and reads it back, the way a sealed file is.
pub(crate) fn through_a_parquet_file(batch: &RecordBatch) -> RecordBatch {
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.parquet");
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), batch.schema(), None).unwrap();
    writer.write(batch).unwrap();
    writer.close().unwrap();
    let mut batches = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&path).unwrap())
        .unwrap()
        .build()
        .unwrap();
    batches.next().unwrap().unwrap()
}

/// Every way a call can carry its output limit.
fn output_limits() -> [OutputLimit; 4] {
    [
        OutputLimit::Configured { tokens: 100 },
        OutputLimit::Model {
            tokens: 65_536,
            source: CatalogSource::Live {
                queried_at: "2026-09-14T00:00:00Z".into(),
            },
        },
        OutputLimit::Model {
            tokens: 8_192,
            source: CatalogSource::Shipped {
                verified: "2026-09-01".into(),
            },
        },
        OutputLimit::Unknown,
    ]
}

fn rows_with(limits: &[OutputLimit]) -> Vec<LlmCallRow> {
    limits
        .iter()
        .zip(0..)
        .map(|(limit, i)| LlmCallRow {
            output_limit: limit.clone(),
            ..row("t", MIDNIGHT + i, CallOutcome::Completed)
        })
        .collect()
}

#[test]
fn output_limit_is_a_tokens_column_and_a_source_column() {
    use crate::event::log::ParquetTable;

    let limits = output_limits();
    let batch = through_a_parquet_file(&LlmCallTable::encode(&rows_with(&limits)).unwrap());

    assert!(batch.column_by_name("output_limit").is_none());
    let tokens: Vec<Option<u32>> = batch
        .column_by_name("output_limit_tokens")
        .unwrap()
        .as_primitive::<UInt32Type>()
        .iter()
        .collect();
    assert_eq!(tokens, vec![Some(100), Some(65_536), Some(8_192), None]);
    let source = batch
        .column_by_name("output_limit_source")
        .unwrap()
        .as_struct();
    assert_eq!(source.null_count(), 0, "the source is never null (b/103)");
    let field = |name| -> Vec<Option<String>> {
        let text = source.column_by_name(name).unwrap().as_string::<i32>();
        (0..source.len())
            .map(|i| (source.is_valid(i) && text.is_valid(i)).then(|| text.value(i).to_owned()))
            .collect()
    };
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(
        field("kind"),
        vec![
            some("configured"),
            some("catalog_live"),
            some("catalog_shipped"),
            some("unknown"),
        ]
    );
    assert_eq!(
        field("as_of"),
        vec![None, some("2026-09-14T00:00:00Z"), some("2026-09-01"), None,]
    );

    let got: Vec<OutputLimit> = LlmCallTable::decode(&batch)
        .unwrap()
        .into_iter()
        .map(|r| r.output_limit)
        .collect();
    assert_eq!(got, limits);
}

fn score() -> EvalScoreRow {
    EvalScoreRow {
        score_id: Uuid::new_v4(),
        call_id: CallId::new_v7(MIDNIGHT).unwrap(),
        tenant: TenantId::new("acme"),
        at_ms: MIDNIGHT + 1,
        evaluator: "exact".into(),
        score: 1.0,
        explanation: None,
        tags: vec![],
    }
}

#[test]
fn a_score_of_a_call_takes_the_call_s_id_and_tenant() {
    let call = row("acme", MIDNIGHT, CallOutcome::Completed);

    let scored = EvalScoreRow::of_call(
        &call,
        Uuid::new_v4(),
        MIDNIGHT + 1,
        "exact".into(),
        1.0,
        None,
        vec![],
    );

    assert_eq!(
        (scored.call_id, &scored.tenant),
        (call.call_id, &call.tenant)
    );
}

/// Asserts that `T`'s schema columns are exactly the columns `writers` lists, each with exactly
/// one writer (`docs/design/melt-parquet.md` §4.1).
pub(crate) fn assert_one_writer_per_column<T, W>(writers: &[(&str, W)])
where
    T: crate::event::log::ParquetTable,
    W: std::fmt::Debug,
{
    use std::collections::BTreeSet;

    let schema = T::schema();
    let schema_columns: BTreeSet<&str> =
        schema.fields().iter().map(|f| f.name().as_str()).collect();
    let registered: BTreeSet<&str> = writers.iter().map(|(column, _)| *column).collect();

    let listed_twice: Vec<&(&str, W)> = writers
        .iter()
        .filter(|(column, _)| writers.iter().filter(|(c, _)| c == column).count() > 1)
        .collect();
    assert!(
        listed_twice.is_empty(),
        "columns with more than one writer: {listed_twice:?}"
    );

    let unregistered: Vec<&&str> = schema_columns.difference(&registered).collect();
    let not_in_schema: Vec<&&str> = registered.difference(&schema_columns).collect();
    assert!(
        unregistered.is_empty() && not_in_schema.is_empty(),
        "schema columns without a writer: {unregistered:?}; writers of no schema column: {not_in_schema:?}"
    );
}

#[test]
fn every_llm_call_column_has_exactly_one_writer() {
    assert_one_writer_per_column::<LlmCallTable, _>(columns::writers::LLM_CALL);
}

#[test]
fn every_eval_score_column_has_exactly_one_writer() {
    assert_one_writer_per_column::<EvalScoreTable, _>(columns::writers::EVAL_SCORE);
}

fn calls_of_every_outcome() -> Vec<LlmCallRow> {
    vec![
        row("acme", MIDNIGHT + 1, CallOutcome::Completed),
        row(
            "acme",
            MIDNIGHT + 2,
            CallOutcome::Failed(CallFailure::of_kind(LlmErrorKind::RateLimited)),
        ),
        row("acme", MIDNIGHT + 3, CallOutcome::Cancelled),
        row("acme", MIDNIGHT - DAY_MS, CallOutcome::Unterminated),
    ]
}

#[test]
fn a_summary_read_touches_only_the_summary_columns_never_a_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("calls.parquet");
    probe::sealed_file::<LlmCallTable>(&path, calls_of_every_outcome());

    let whole = probe::columns_read_whole::<LlmCallTable>(&path);
    let summary = probe::columns_read_in_summary::<LlmCallTable>(&path);

    assert!(
        whole.contains("request_body") && whole.contains("response_body"),
        "the probe sees a body read: {whole:?}"
    );
    assert_eq!(
        summary,
        LlmCallTable::PARTIAL_COLUMNS
            .iter()
            .map(|c| (*c).to_owned())
            .collect(),
        "no body column chunk was read"
    );
}

#[tokio::test]
async fn a_summary_read_from_parquet_is_the_summary_of_the_full_row() {
    let dir = tempfile::tempdir().unwrap();
    let log = ParquetLog::<LlmCallTable>::open(dir.path());
    let rows = calls_of_every_outcome();
    let sink = log.sink(CALL_LOG_SINK, None, &Handle::current());
    for r in rows.clone() {
        sink.append(r).await.unwrap();
    }
    sink.close().await.unwrap();
    let scope = TenantScope::new(TenantId::new("acme"), TimeWindow::all());

    let read = log
        .scan_partial(&scope, &TagFilter::Every, None)
        .await
        .unwrap();

    let mut want: Vec<LlmCallSummary> = rows.iter().map(LlmCallSummary::of).collect();
    want.sort_by_key(|c| std::cmp::Reverse(c.call_id));
    assert_eq!(format!("{read:?}"), format!("{want:?}"));
}

/// A limit the other columns of two plain rows stay within; a column of 1000 bytes a row passes it
/// at the second row.
fn limit_1500() -> OffsetLimit {
    OffsetLimit::new(1500).unwrap()
}

fn two_calls(place: impl Fn(LlmCallRow) -> LlmCallRow) -> Vec<LlmCallRow> {
    (0..2)
        .map(|i| place(row("t", MIDNIGHT + i, CallOutcome::Completed)))
        .collect()
}

#[test]
fn tags_past_the_limit_name_the_column_and_row() {
    use crate::event::log::ParquetTable;
    let rows = two_calls(|r| LlmCallRow {
        tags: vec!["x".repeat(1000)],
        ..r
    });

    let got = LlmCallTable::encode_within(&rows, limit_1500());

    assert!(
        matches!(
            got,
            Err(EncodeError::TextTooLong {
                column: "tags",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[test]
fn layers_past_the_limit_name_the_column_and_row() {
    use crate::event::log::ParquetTable;
    let rows = two_calls(|r| LlmCallRow {
        layers: vec!["x".repeat(1000)],
        ..r
    });

    let got = LlmCallTable::encode_within(&rows, limit_1500());

    assert!(
        matches!(
            got,
            Err(EncodeError::TextTooLong {
                column: "layers",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[test]
fn degraded_past_the_limit_names_the_column_and_row() {
    use crate::event::log::ParquetTable;
    use tars_types::Degradation;
    let rows = two_calls(|r| LlmCallRow {
        dispatch: CallDispatch::Sent {
            degraded: vec![Degradation::StructuredOutputNotEnforced; 60],
            provider_id: None,
            end: SentEnd::Cancelled,
        },
        ..r
    });

    let got = LlmCallTable::encode_within(&rows, limit_1500());

    assert!(
        matches!(
            got,
            Err(EncodeError::TextTooLong {
                column: "degraded",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[test]
fn degraded_with_more_items_than_the_limit_names_the_column_and_row() {
    use crate::event::log::ParquetTable;
    use tars_types::Degradation;
    let rows = two_calls(|r| LlmCallRow {
        dispatch: CallDispatch::Sent {
            degraded: vec![Degradation::StructuredOutputNotEnforced; 800],
            provider_id: None,
            end: SentEnd::Cancelled,
        },
        ..r
    });

    let got = LlmCallTable::encode_within(&rows, limit_1500());

    assert!(
        matches!(
            got,
            Err(EncodeError::ListTooLong {
                column: "degraded",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[test]
fn score_tags_past_the_limit_name_the_column_and_row() {
    use crate::event::log::ParquetTable;
    let rows: Vec<EvalScoreRow> = (0..2)
        .map(|_| EvalScoreRow {
            tags: vec!["x".repeat(1000)],
            ..score()
        })
        .collect();

    let got = EvalScoreTable::encode_within(&rows, limit_1500());

    assert!(
        matches!(
            got,
            Err(EncodeError::TextTooLong {
                column: "tags",
                row: 1
            })
        ),
        "{got:?}"
    );
}
