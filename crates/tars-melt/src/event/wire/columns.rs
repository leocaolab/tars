//! The Parquet columns of `llm_wire`. The bytes are binary columns,
//! stored as they were sent and received; the outcome is a typed column, and
//! `error` holds the error's text on a failed attempt only.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Int64Type, UInt16Type, UInt32Type};
use arrow_array::{
    Array, ArrayRef, BinaryArray, Int64Array, ListArray, RecordBatch, UInt16Array, UInt32Array,
};
use arrow_schema::{DataType, Field, FieldRef, Schema, SchemaRef};
use bytes::Bytes;
use parquet::file::statistics::Statistics;
use tars_types::{CallId, ProviderId};

use super::{SpawnedProcess, Transport, WireOutcome, WireRow, WireTable};
use crate::event::call_log::columns::{
    Columns, UUID_BYTES, bools, call_id_at, call_id_range, call_ids, from_name, missing, opt,
    opt_str,
};
use crate::event::log::{
    ColumnError, DecodeError, EncodeError, OffsetLimit, ParquetTable, binaries, decode_rows,
    string_lists, strs,
};

const ARGV: &str = "argv";
const STDERR_BYTES: &str = "stderr_bytes";

const HTTP_SSE: &str = "http_sse";
const SUBPROCESS: &str = "subprocess";

const COMPLETED: &str = "completed";
const FAILED: &str = "failed";
const CANCELLED: &str = "cancelled";

impl ParquetTable for WireTable {
    const KEY_COLUMN: &'static str = "call_id";

    /// A column added here needs its one writer in `writers`.
    fn schema() -> SchemaRef {
        use DataType::{Binary, Boolean, Int64, UInt16, UInt32, Utf8};
        Arc::new(Schema::new(vec![
            Field::new("call_id", DataType::FixedSizeBinary(UUID_BYTES), false),
            Field::new("tenant", Utf8, false),
            Field::new("attempt", UInt32, false),
            Field::new("at_ms", Int64, false),
            Field::new("provider_id", Utf8, false),
            Field::new("model", Utf8, false),
            Field::new("transport", Utf8, false),
            Field::new("request_bytes", Binary, false),
            Field::new(ARGV, DataType::List(argv_item()), true),
            Field::new("status", UInt16, true),
            Field::new("response_bytes", Binary, false),
            Field::new("response_truncated", Boolean, false),
            Field::new(STDERR_BYTES, Binary, true),
            Field::new("outcome", Utf8, false),
            Field::new("error", Utf8, true),
        ]))
    }

    fn encode_within(rows: &[WireRow], limit: OffsetLimit) -> Result<RecordBatch, EncodeError> {
        let outcomes: Vec<(&str, Option<&str>)> =
            rows.iter().map(|r| outcome_columns(&r.outcome)).collect();
        let columns: Vec<ArrayRef> = vec![
            call_ids(rows.iter().map(|r| r.call_id))?,
            strs(
                limit,
                "tenant",
                rows.iter().map(|r| Some(r.tenant.as_str())),
            )?,
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.attempt),
            )),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.at_ms))),
            strs(
                limit,
                "provider_id",
                rows.iter().map(|r| Some(r.provider_id.as_str())),
            )?,
            strs(limit, "model", rows.iter().map(|r| Some(r.model.as_str())))?,
            strs(
                limit,
                "transport",
                rows.iter().map(|r| Some(r.transport.as_str())),
            )?,
            binaries(
                limit,
                "request_bytes",
                rows.iter().map(|r| Some(r.request_bytes.as_ref())),
            )?,
            string_lists(
                limit,
                ARGV,
                argv_item(),
                rows.iter()
                    .map(|r| spawned(&r.transport).map(|p| p.argv.as_slice())),
            )?,
            Arc::new(UInt16Array::from_iter(
                rows.iter().map(|r| status(&r.transport)),
            )),
            binaries(
                limit,
                "response_bytes",
                rows.iter().map(|r| Some(r.response_bytes.as_ref())),
            )?,
            bools(rows.iter().map(|r| r.response_truncated)),
            binaries(
                limit,
                STDERR_BYTES,
                rows.iter()
                    .map(|r| spawned(&r.transport).map(|p| p.stderr.as_ref())),
            )?,
            strs(
                limit,
                "outcome",
                outcomes.iter().map(|(outcome, _)| Some(*outcome)),
            )?,
            strs(limit, "error", outcomes.iter().map(|(_, error)| *error))?,
        ];
        Ok(RecordBatch::try_new(Self::schema(), columns)?)
    }

    fn decode(batch: &RecordBatch) -> Result<Vec<WireRow>, DecodeError> {
        let c = Columns(batch);
        let call_id = c.fixed("call_id")?;
        let tenant = c.utf8("tenant")?;
        let attempt = c.prim::<UInt32Type>("attempt")?;
        let at_ms = c.prim::<Int64Type>("at_ms")?;
        let provider_id = c.utf8("provider_id")?;
        let model = c.utf8("model")?;
        let transport = c.utf8("transport")?;
        let request_bytes = c.binary("request_bytes")?;
        let argv = c.list(ARGV)?;
        let status = c.prim::<UInt16Type>("status")?;
        let response_bytes = c.binary("response_bytes")?;
        let response_truncated = c.bool("response_truncated")?;
        let stderr_bytes = c.binary(STDERR_BYTES)?;
        let outcome = c.utf8("outcome")?;
        let error = c.utf8("error")?;

        decode_rows(batch.num_rows(), |i| {
            Ok(WireRow {
                call_id: call_id_at(call_id, i)?,
                tenant: from_name(tenant.value(i))?,
                attempt: attempt.value(i),
                at_ms: at_ms.value(i),
                provider_id: from_name::<ProviderId>(provider_id.value(i))?,
                model: model.value(i).to_owned(),
                transport: transport_at(
                    transport.value(i),
                    opt(status, i),
                    opt_strings_at(argv, i)?,
                    opt_bytes_at(stderr_bytes, i),
                )?,
                request_bytes: Bytes::copy_from_slice(request_bytes.value(i)),
                response_bytes: Bytes::copy_from_slice(response_bytes.value(i)),
                response_truncated: response_truncated.value(i),
                outcome: outcome_at(outcome.value(i), opt_str(error, i))?,
            })
        })
    }

    fn key_range(stats: &Statistics) -> Option<(CallId, CallId)> {
        call_id_range(stats)
    }
}

fn argv_item() -> FieldRef {
    Arc::new(Field::new("item", DataType::Utf8, false))
}

fn opt_bytes_at(a: &BinaryArray, i: usize) -> Option<Bytes> {
    a.is_valid(i).then(|| Bytes::copy_from_slice(a.value(i)))
}

fn opt_strings_at(a: &ListArray, i: usize) -> Result<Option<Vec<String>>, ColumnError> {
    if !a.is_valid(i) {
        return Ok(None);
    }
    let values = a.value(i);
    let strings = values
        .as_string_opt::<i32>()
        .ok_or_else(|| ColumnError::WrongItemType {
            column: ARGV.to_owned(),
            found: values.data_type().clone(),
        })?;
    Ok(Some(strings.iter().flatten().map(str::to_owned).collect()))
}

impl<'a> Columns<'a> {
    fn binary(&self, name: &str) -> Result<&'a BinaryArray, ColumnError> {
        self.typed(name, |c| c.as_binary_opt::<i32>())
    }
}

fn spawned(t: &Transport) -> Option<&SpawnedProcess> {
    match t {
        Transport::Subprocess { process } => process.as_ref(),
        Transport::HttpSse { .. } => None,
    }
}

fn status(t: &Transport) -> Option<u16> {
    match t {
        Transport::HttpSse { status } => *status,
        Transport::Subprocess { .. } => None,
    }
}

/// Reads a transport from its four columns: a status only on HTTP, and argv and stderr together,
/// only on a subprocess that was spawned.
fn transport_at(
    word: &str,
    status: Option<u16>,
    argv: Option<Vec<String>>,
    stderr: Option<Bytes>,
) -> Result<Transport, ColumnError> {
    match (word, status, argv, stderr) {
        (HTTP_SSE, status, None, None) => Ok(Transport::HttpSse { status }),
        (SUBPROCESS, None, None, None) => Ok(Transport::Subprocess { process: None }),
        (SUBPROCESS, None, Some(argv), Some(stderr)) => Ok(Transport::Subprocess {
            process: Some(SpawnedProcess { argv, stderr }),
        }),
        (word, status, argv, stderr) => Err(ColumnError::Inconsistent {
            columns: "transport, status, argv and stderr_bytes",
            found: format!(
                "transport={word:?} status={status:?} argv={argv:?} stderr_bytes={}",
                if stderr.is_some() { "set" } else { "null" },
            ),
        }),
    }
}

fn outcome_columns(o: &WireOutcome) -> (&'static str, Option<&str>) {
    match o {
        WireOutcome::Completed => (COMPLETED, None),
        WireOutcome::Failed { error } => (FAILED, Some(error)),
        WireOutcome::Cancelled => (CANCELLED, None),
    }
}

fn outcome_at(outcome: &str, error: Option<&str>) -> Result<WireOutcome, ColumnError> {
    match (outcome, error) {
        (COMPLETED, None) => Ok(WireOutcome::Completed),
        (FAILED, Some(error)) => Ok(WireOutcome::Failed {
            error: error.to_owned(),
        }),
        (FAILED, None) => Err(missing("error")),
        (CANCELLED, None) => Ok(WireOutcome::Cancelled),
        (outcome, error) => Err(ColumnError::Inconsistent {
            columns: "outcome and error",
            found: format!("outcome={outcome:?} error={error:?}"),
        }),
    }
}

/// The writer of each `llm_wire` column (`docs/design/melt-parquet.md` §4.1): every column is
/// written by the `WireRecorder` of the backend that made the attempt.
#[cfg(test)]
pub(super) mod writers {
    /// Who writes an `llm_wire` column.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum WireWriter {
        /// The transport that made the attempt, through its `WireRecorder`.
        Transport,
    }

    pub(crate) const LLM_WIRE: &[(&str, WireWriter)] = {
        use WireWriter::Transport;
        &[
            ("call_id", Transport),
            ("tenant", Transport),
            ("attempt", Transport),
            ("at_ms", Transport),
            ("provider_id", Transport),
            ("model", Transport),
            ("transport", Transport),
            ("request_bytes", Transport),
            (super::ARGV, Transport),
            ("status", Transport),
            ("response_bytes", Transport),
            ("response_truncated", Transport),
            (super::STDERR_BYTES, Transport),
            ("outcome", Transport),
            ("error", Transport),
        ]
    };
}
