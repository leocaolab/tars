//! Keeps a session's conversation verbatim. While a session runs, its writer appends one jsonl
//! line per message; when it ends, each line becomes one row here, the line itself byte for byte
//! in `raw_json`; the other columns are what the line is found by.
//!
//! Rows sort by `(session_id, seq)`, so one session's lines are one contiguous run.
//!
//! Partitioned by the row's tenant and the day the session opened, so a
//! session that runs past midnight stays in one `date=` directory.

use std::num::NonZeroUsize;

use tars_types::{Millis, SessionId, TenantId};

use super::log::{LogTable, Partition, Partitioned, TableName};

/// One transcript line as sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRow {
    pub tenant: TenantId,
    pub session_id: SessionId,
    /// The line's position in its session, from 0.
    pub seq: u64,
    /// When the line was written (epoch ms), as the line itself says.
    pub at_ms: Millis,
    /// The jsonl line, byte for byte, without its newline.
    pub raw_json: String,
}

/// The `session_transcript` table.
pub struct SessionTranscript;

/// Large enough that a session's lines usually fit in one row group.
const ROW_GROUP_ROWS: NonZeroUsize = NonZeroUsize::new(65_536).expect("non-zero");

impl LogTable for SessionTranscript {
    type Row = TranscriptRow;
    type Key = (SessionId, u64);

    const NAME: TableName = TableName::new("session_transcript");
    const ROW_GROUP_ROWS: NonZeroUsize = ROW_GROUP_ROWS;

    fn sort_key(row: &TranscriptRow) -> (SessionId, u64) {
        (row.session_id, row.seq)
    }

    fn approx_bytes(row: &TranscriptRow) -> usize {
        row.raw_json.len() + row.tenant.as_str().len()
    }
}

impl Partitioned for SessionTranscript {
    fn partition(row: &TranscriptRow) -> Partition {
        Partition {
            tenant: row.tenant.clone(),
            day: row.session_id.day(),
        }
    }
}

#[cfg(feature = "parquet")]
mod parquet {
    use std::sync::Arc;

    use ::parquet::file::statistics::Statistics;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Int64Type, UInt64Type};
    use arrow_array::{ArrayRef, Int64Array, RecordBatch, UInt64Array};
    use arrow_schema::{DataType, Field, Schema, SchemaRef};
    use tars_types::{SessionId, TenantId};

    use super::{SessionTranscript, TranscriptRow};
    use crate::event::log::{
        ColumnError, DecodeError, EncodeError, OffsetLimit, ParquetTable, decode_rows, strs,
    };

    impl ParquetTable for SessionTranscript {
        /// Hyphenated lowercase hex sorts like the id's bytes, so the string
        /// statistics bound the ids.
        const KEY_COLUMN: &'static str = "session_id";

        fn schema() -> SchemaRef {
            Arc::new(Schema::new(vec![
                Field::new("tenant", DataType::Utf8, false),
                Field::new("session_id", DataType::Utf8, false),
                Field::new("seq", DataType::UInt64, false),
                Field::new("at_ms", DataType::Int64, false),
                Field::new("raw_json", DataType::Utf8, false),
            ]))
        }

        fn encode_within(
            rows: &[TranscriptRow],
            limit: OffsetLimit,
        ) -> Result<RecordBatch, EncodeError> {
            let session_ids: Vec<String> = rows.iter().map(|r| r.session_id.to_string()).collect();
            let columns: Vec<ArrayRef> = vec![
                strs(
                    limit,
                    "tenant",
                    rows.iter().map(|r| Some(r.tenant.as_str())),
                )?,
                strs(
                    limit,
                    "session_id",
                    session_ids.iter().map(|s| Some(s.as_str())),
                )?,
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.seq))),
                Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.at_ms))),
                strs(
                    limit,
                    "raw_json",
                    rows.iter().map(|r| Some(r.raw_json.as_str())),
                )?,
            ];
            Ok(RecordBatch::try_new(Self::schema(), columns)?)
        }

        fn decode(batch: &RecordBatch) -> Result<Vec<TranscriptRow>, DecodeError> {
            let shape_error = || ColumnError::Shape {
                expected: "utf8, utf8, u64, i64, utf8",
                found: batch.schema(),
            };
            let tenants = batch
                .column(0)
                .as_string_opt::<i32>()
                .ok_or_else(shape_error)?;
            let sessions = batch
                .column(1)
                .as_string_opt::<i32>()
                .ok_or_else(shape_error)?;
            let seqs = batch
                .column(2)
                .as_primitive_opt::<UInt64Type>()
                .ok_or_else(shape_error)?;
            let ats = batch
                .column(3)
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(shape_error)?;
            let raws = batch
                .column(4)
                .as_string_opt::<i32>()
                .ok_or_else(shape_error)?;

            decode_rows(batch.num_rows(), |i| {
                Ok(TranscriptRow {
                    tenant: tenant(tenants.value(i))?,
                    session_id: session(sessions.value(i))?,
                    seq: seqs.value(i),
                    at_ms: ats.value(i),
                    raw_json: raws.value(i).to_owned(),
                })
            })
        }

        fn key_range(stats: &Statistics) -> Option<((SessionId, u64), (SessionId, u64))> {
            let Statistics::ByteArray(s) = stats else {
                return None;
            };
            let id = |bytes: &::parquet::data_type::ByteArray| {
                bytes.as_utf8().ok()?.parse::<SessionId>().ok()
            };
            Some(((id(s.min_opt()?)?, 0), (id(s.max_opt()?)?, u64::MAX)))
        }
    }

    fn tenant(text: &str) -> Result<TenantId, ColumnError> {
        text.parse().map_err(ColumnError::Tenant)
    }

    fn session(text: &str) -> Result<SessionId, ColumnError> {
        text.parse().map_err(ColumnError::SessionId)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn raw_json_past_the_limit_names_the_column_and_row() {
            let line = |seq| TranscriptRow {
                tenant: TenantId::new("t"),
                session_id: SessionId::new_v7(1_790_553_600_000).unwrap(),
                seq,
                at_ms: 0,
                raw_json: format!("{{\"pad\":\"{}\"}}", "x".repeat(40)),
            };
            // Two session ids take 72 bytes; two lines take 100.
            let limit = OffsetLimit::new(80).unwrap();

            let got = SessionTranscript::encode_within(&[line(0), line(1)], limit);

            assert!(
                matches!(
                    got,
                    Err(EncodeError::TextTooLong {
                        column: "raw_json",
                        row: 1
                    })
                ),
                "{got:?}"
            );
        }

        #[test]
        fn a_row_with_an_empty_tenant_is_a_decode_error() {
            assert!(matches!(tenant(""), Err(ColumnError::Tenant(_))));
            assert_eq!(
                tenant("t").map(TenantId::into_inner).ok(),
                Some("t".to_owned())
            );
        }
    }
}
