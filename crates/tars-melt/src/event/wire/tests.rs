use bytes::Bytes;
use tars_types::{CallId, Millis, ProviderId, TenantId};
use tokio::runtime::Handle;

use super::*;
use crate::event::call_log::tests::{assert_one_writer_per_column, through_a_parquet_file};
use crate::event::log::{EncodeError, LogSink, OffsetLimit, ParquetLog, ParquetTable};

/// 2026-09-28T00:00:00Z.
const MIDNIGHT: Millis = 1_790_553_600_000;

fn attempt(call_id: CallId, attempt: u32, at_ms: Millis, outcome: WireOutcome) -> WireRow {
    WireRow {
        tenant: TenantId::new("acme"),
        call_id,
        attempt,
        at_ms,
        provider_id: ProviderId::new("anthropic"),
        model: "claude-x".into(),
        transport: Transport::HttpSse { status: Some(200) },
        request_bytes: Bytes::from_static(b"{\"model\":\"claude-x\"}"),
        // Not UTF-8: the column holds bytes, not text.
        response_bytes: Bytes::from_static(b"data: {}\n\n\xff\xfe"),
        response_truncated: false,
        outcome,
    }
}

fn spawned(argv: Vec<String>, stderr: Bytes) -> Transport {
    Transport::Subprocess {
        process: Some(SpawnedProcess { argv, stderr }),
    }
}

#[test]
fn every_llm_wire_column_has_exactly_one_writer() {
    assert_one_writer_per_column::<WireTable, _>(columns::writers::LLM_WIRE);
}

#[test]
fn every_outcome_round_trips_through_a_parquet_file() {
    let call = CallId::new_v7(MIDNIGHT).unwrap();
    let rows = vec![
        attempt(call, 0, MIDNIGHT, WireOutcome::Completed),
        WireRow {
            transport: Transport::HttpSse { status: Some(429) },
            response_truncated: true,
            ..attempt(
                call,
                1,
                MIDNIGHT + 1,
                WireOutcome::Failed {
                    error: "rate limited: slow down".into(),
                },
            )
        },
        WireRow {
            transport: spawned(
                vec!["claude".into(), "-p".into(), "".into()],
                // Not UTF-8 either; and an empty stderr is a stream that said nothing.
                Bytes::from_static(b"rate limited\n\xff"),
            ),
            ..attempt(call, 2, MIDNIGHT + 2, WireOutcome::Cancelled)
        },
    ];

    let batch = through_a_parquet_file(&WireTable::encode(&rows).unwrap());

    assert_eq!(WireTable::decode(&batch).unwrap(), rows);
}

#[test]
fn a_spawned_process_with_an_empty_stderr_is_kept_apart_from_one_never_spawned() {
    let call = CallId::new_v7(MIDNIGHT).unwrap();
    let rows = vec![
        WireRow {
            transport: spawned(vec!["claude".into()], Bytes::new()),
            ..attempt(call, 0, MIDNIGHT, WireOutcome::Completed)
        },
        WireRow {
            transport: Transport::Subprocess { process: None },
            ..attempt(call, 1, MIDNIGHT + 1, WireOutcome::Completed)
        },
    ];

    let got =
        WireTable::decode(&through_a_parquet_file(&WireTable::encode(&rows).unwrap())).unwrap();

    assert_eq!(got, rows);
}

/// Two rows of 40 bytes placed by `place`; the second ends past a limit of 64, which the other
/// columns of two rows stay within.
fn two_rows_past_64(place: impl Fn(Bytes, WireRow) -> WireRow) -> Vec<WireRow> {
    let call = CallId::new_v7(MIDNIGHT).unwrap();
    let bytes = Bytes::from_static(&[0; 40]);
    (0..2)
        .map(|i| {
            place(
                bytes.clone(),
                attempt(call, i, MIDNIGHT, WireOutcome::Completed),
            )
        })
        .collect()
}

fn limit_64() -> OffsetLimit {
    OffsetLimit::new(64).unwrap()
}

#[test]
fn request_bytes_past_the_limit_name_the_column_and_row() {
    let rows = two_rows_past_64(|request_bytes, row| WireRow {
        request_bytes,
        ..row
    });

    let got = WireTable::encode_within(&rows, limit_64());

    assert!(
        matches!(
            got,
            Err(EncodeError::BytesTooLong {
                column: "request_bytes",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[test]
fn response_bytes_past_the_limit_name_the_column_and_row() {
    let rows = two_rows_past_64(|response_bytes, row| WireRow {
        response_bytes,
        ..row
    });

    let got = WireTable::encode_within(&rows, limit_64());

    assert!(
        matches!(
            got,
            Err(EncodeError::BytesTooLong {
                column: "response_bytes",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[test]
fn stderr_past_the_limit_names_the_column_and_row() {
    let rows = two_rows_past_64(|stderr, row| WireRow {
        transport: spawned(Vec::new(), stderr),
        ..row
    });

    let got = WireTable::encode_within(&rows, limit_64());

    assert!(
        matches!(
            got,
            Err(EncodeError::BytesTooLong {
                column: "stderr_bytes",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[test]
fn argv_past_the_limit_names_the_column_and_row() {
    let rows = two_rows_past_64(|_, row| WireRow {
        transport: spawned(vec!["x".repeat(40)], Bytes::new()),
        ..row
    });

    let got = WireTable::encode_within(&rows, limit_64());

    assert!(
        matches!(
            got,
            Err(EncodeError::TextTooLong {
                column: "argv",
                row: 1
            })
        ),
        "{got:?}"
    );
}

#[tokio::test]
async fn a_calls_attempts_sit_in_its_days_partition_even_past_midnight() {
    let dir = tempfile::tempdir().unwrap();
    let log = ParquetLog::<WireTable>::open(dir.path());
    let call = CallId::new_v7(MIDNIGHT - 1).unwrap();
    let other = CallId::new_v7(MIDNIGHT - 1).unwrap();
    let rows = [
        // The retry starts the next day; the call started the day before.
        attempt(call, 1, MIDNIGHT + 5, WireOutcome::Completed),
        attempt(
            call,
            0,
            MIDNIGHT - 1,
            WireOutcome::Failed {
                error: "HTTP 503".into(),
            },
        ),
        attempt(other, 0, MIDNIGHT - 1, WireOutcome::Completed),
    ];
    let sink = log.sink(WIRE_LOG_SINK, Some(WIRE_LOG_COMPACTION), &Handle::current());
    for r in rows.clone() {
        sink.append(r).await.unwrap();
    }
    sink.close().await.unwrap();

    let got = attempts(&log, &TenantId::new("acme"), call).await.unwrap();

    assert_eq!(got, vec![rows[1].clone(), rows[0].clone()]);
}

#[tokio::test]
async fn a_call_no_transport_recorded_has_no_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let log = ParquetLog::<WireTable>::open(dir.path());

    let got = attempts(
        &log,
        &TenantId::new("acme"),
        CallId::new_v7(MIDNIGHT).unwrap(),
    )
    .await
    .unwrap();

    assert!(got.is_empty());
}
