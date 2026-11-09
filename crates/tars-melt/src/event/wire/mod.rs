//! The wire log (`llm_wire`): one [`WireRow`] per attempt a provider's transport made, holding the
//! bytes it sent and the bytes it got back, verbatim. The transport writes it when the attempt
//! ends, because it is the only layer that sees those bytes.
//!
//! Partitioned by tenant and by the UTC day of the call's [`CallId`], so every attempt of a call,
//! a retry past midnight too, sits in the partition of the call's `llm_call` row.

#[cfg(feature = "parquet")]
mod columns;

use std::num::NonZeroUsize;

use bytes::Bytes;
use tars_types::{CallId, Millis, ProviderId, TenantId};

#[cfg(feature = "parquet")]
use super::log::CompactRule;
use super::log::{LogError, LogSource, LogTable, Partition, Partitioned, SinkConfig, TableName};

/// Rows per row group: a point lookup by `call_id` decodes at most this many
/// attempts' bytes.
const ROW_GROUP_ROWS: NonZeroUsize = NonZeroUsize::new(16).unwrap();

/// The wire log seals on the call log's policy, because its rows are written on the same response
/// path and lost in the same crash window.
pub const WIRE_LOG_SINK: SinkConfig = super::call_log::CALL_LOG_SINK;

/// How the wire log's partitions merge: as the call log's do.
#[cfg(feature = "parquet")]
pub const WIRE_LOG_COMPACTION: CompactRule = super::call_log::CALL_LOG_COMPACTION;

/// The `llm_wire` table.
pub struct WireTable;

/// One attempt of one call, as the transport saw it on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRow {
    pub tenant: TenantId,
    /// The call this attempt belongs to: the id of its `llm_call` row.
    pub call_id: CallId,
    /// 0 for the first attempt, then one more per retry.
    pub attempt: u32,
    /// When the attempt started (epoch ms).
    pub at_ms: Millis,
    pub provider_id: ProviderId,
    pub model: String,
    pub transport: Transport,
    /// What was sent, byte for byte, without headers. HTTP: the request
    /// body. Subprocess: the bytes written into its stdin — empty for a
    /// delegate whose prompt travels in the argv.
    pub request_bytes: Bytes,
    /// What came back, byte for byte: the SSE body, the first 8 KiB of an
    /// error body, or a subprocess's stdout. As much as arrived before the
    /// attempt ended.
    pub response_bytes: Bytes,
    /// The transport stopped reading at its cap with bytes left unread.
    pub response_truncated: bool,
    pub outcome: WireOutcome,
}

/// The frame format an attempt went out in, with what only that format has. What is behind the
/// transport is the provider's `InterfaceKind`, not this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// An HTTP request answered by a server-sent-event stream.
    HttpSse {
        /// `None` when no response head arrived.
        status: Option<u16>,
    },
    /// A delegate CLI: its stdin in `request_bytes`, its stdout in `response_bytes`.
    Subprocess {
        /// `None` when no process was spawned.
        process: Option<SpawnedProcess>,
    },
}

impl Transport {
    /// Returns the column word of the frame format.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HttpSse { .. } => "http_sse",
            Self::Subprocess { .. } => "subprocess",
        }
    }
}

/// A delegate CLI's process, once spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedProcess {
    /// The argv it was spawned with — under a write-jail, the wrapper's program and args.
    pub argv: Vec<String>,
    /// Its stderr, byte for byte, as much as was read.
    pub stderr: Bytes,
}

/// How one attempt ended on the wire. Not a call's outcome: a call that
/// retried past a failed attempt can still complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireOutcome {
    /// The provider sent its end of the response (the frame that decodes to
    /// `Finished`), or the response was read to its end. A caller that stops
    /// reading after the provider's end did not cancel anything.
    Completed,
    /// The attempt failed; `error` is the error, as the transport raised it.
    Failed { error: String },
    /// The caller dropped the attempt before the provider's end;
    /// `response_bytes` is what had arrived by then.
    Cancelled,
}

impl LogTable for WireTable {
    type Row = WireRow;
    type Key = CallId;

    const NAME: TableName = TableName::new("llm_wire");
    const ROW_GROUP_ROWS: NonZeroUsize = ROW_GROUP_ROWS;

    fn sort_key(row: &WireRow) -> CallId {
        row.call_id
    }

    fn approx_bytes(row: &WireRow) -> usize {
        let process = match &row.transport {
            Transport::Subprocess {
                process: Some(process),
            } => process.argv.iter().map(String::len).sum::<usize>() + process.stderr.len(),
            Transport::Subprocess { process: None } | Transport::HttpSse { .. } => 0,
        };
        row.request_bytes.len() + row.response_bytes.len() + process
    }
}

impl Partitioned for WireTable {
    /// Returns the tenant and the call's own day, not the day the attempt was written.
    fn partition(row: &WireRow) -> Partition {
        Partition {
            tenant: row.tenant.clone(),
            day: row.call_id.day(),
        }
    }
}

/// Returns every recorded attempt of the tenant's call `id`, first attempt first. It is empty when
/// no transport recorded one: the call ran with no wire log, on a path that has no raw bytes
/// (a batch, a cache hit), or never reached a transport.
pub async fn attempts(
    src: &dyn LogSource<WireTable>,
    tenant: &TenantId,
    id: CallId,
) -> Result<Vec<WireRow>, LogError> {
    let scope = super::call_log::call_scope(tenant, id);
    let mut rows = src.scan_key(&scope, &id).await?;
    rows.sort_by_key(|r| r.attempt);
    Ok(rows)
}

#[cfg(all(test, feature = "parquet"))]
mod tests;
