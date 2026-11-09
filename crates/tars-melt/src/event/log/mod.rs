//! The append-only log mechanism. A writer hands rows to a [`LogSink`]; they sit in a bounded
//! buffer until the [`SealPolicy`] says seal, and only then does a [`LogSource`] see them. A row
//! is in exactly one place: in flight (lost on a crash, a window of at most `max_age`) or sealed.
//!
//! A table declares its own row, key and partition; the mechanism knows no table's domain. The
//! adapters are [`MemLog`] and, with the `parquet` feature, [`ParquetLog`], which also merges a
//! partition's files by an injected [`CompactRule`]. The durable store's sealed journals are not a
//! log table: `tars_storage::durable::journal` defines their port, and [`ParquetTrajectoryLog`]
//! implements it.

#[cfg(test)]
pub(crate) mod contract;
mod mem;
#[cfg(feature = "parquet")]
mod parquet;
mod scope;
mod seal;
mod sink;

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use tars_types::{TenantId, UtcDay};
use thiserror::Error;

pub use mem::MemLog;
#[cfg(all(test, feature = "parquet"))]
pub(crate) use parquet::probe;
#[cfg(feature = "parquet")]
pub(crate) use parquet::{Bucket, StoreId, binaries, decode_rows, string_lists, strs};
#[cfg(feature = "parquet")]
pub use parquet::{
    BucketSetupError, ColumnError, CompactRule, DecodeError, EncodeError, IoWork, ObjectKeyError,
    OffsetLimit, ParquetLog, ParquetProjected, ParquetTable, ParquetTrajectoryLog,
};
pub use scope::{TenantScope, TimeWindow, WindowError};
pub use seal::SealPolicy;
pub use sink::{BufferedSink, SinkConfig};

/// A table's name, restricted to `[a-z_]+` so it is safe as a directory name. The table's owner
/// declares it once as an associated `const`, so an invalid name fails the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableName(&'static str);

impl TableName {
    pub const fn new(name: &'static str) -> Self {
        let bytes = name.as_bytes();
        assert!(!bytes.is_empty(), "table name must not be empty");
        let mut i = 0;
        while i < bytes.len() {
            assert!(
                bytes[i].is_ascii_lowercase() || bytes[i] == b'_',
                "table name must be [a-z_]+"
            );
            i += 1;
        }
        Self(name)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

/// One log table: its row type and the physical layout rows are sealed in.
pub trait LogTable: Send + Sync + 'static {
    type Row: Send + Sync + 'static;
    /// What rows are sorted by when sealed, and what a point lookup asks for
    /// (`llm_call`: its `call_id`).
    type Key: Ord + Clone + Send + Sync + 'static;

    const NAME: TableName;
    /// Rows per row group; a point lookup reads only the groups whose
    /// `[min, max]` key range can hold the key.
    const ROW_GROUP_ROWS: NonZeroUsize;

    fn sort_key(row: &Self::Row) -> Self::Key;
    /// Returns the row's size as counted against `SealPolicy::max_bytes`.
    fn approx_bytes(row: &Self::Row) -> usize;
}

/// Where a row lives: its tenant and its UTC day. `ParquetLog` lays these
/// out as `tenant=<t>/<table>/date=<d>/`; a batch spanning several is split.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Partition {
    pub tenant: TenantId,
    pub day: UtcDay,
}

/// A table whose rows are partitioned by tenant and day. The durable store's
/// journal is not: its files are placed by the durable store's own registry.
pub trait Partitioned: LogTable {
    fn partition(row: &Self::Row) -> Partition;
}

/// A table read in summary: each row as its own smaller type, holding only
/// the columns a listing needs — a column it leaves out (a body) is not
/// there to be read as empty.
///
/// A row's sort key orders it after every row of an earlier partition day
/// (`llm_call`: the key is the call id, whose day is the partition's), so
/// the rows with the greatest keys are in the newest days.
///
/// A newest-first read is narrowed by the table's own [`Self::Filter`],
/// judged on the summary: the filter reads only what a listing shows.
pub trait Projected: Partitioned {
    type Partial: Send + Sync + 'static;
    /// Which rows a read keeps (`llm_call`: a cohort tag).
    type Filter: Clone + Send + Sync + 'static;

    fn partial(row: &Self::Row) -> Self::Partial;
    fn partial_key(partial: &Self::Partial) -> Self::Key;
    fn partial_tenant(partial: &Self::Partial) -> &TenantId;
    fn keeps(filter: &Self::Filter, partial: &Self::Partial) -> bool;
}

/// Write side. Appending does no IO; sealing happens behind it.
#[async_trait]
pub trait LogSink<T: LogTable>: Send + Sync {
    /// Waits for buffer space, then enqueues the row; after `close` it returns `Err(Closed)`.
    async fn append(&self, row: T::Row) -> Result<(), AppendError>;
    /// Enqueues the row without waiting, for a caller that cannot await (a drop guard). When the
    /// buffer is full the row is dropped, counted, and `Err(Full)` returned.
    fn try_append(&self, row: T::Row) -> Result<(), AppendError>;
    /// Seals what is left and stops. `Ok` means no row handed to this sink was lost; a dropped
    /// row, a refused one included, makes it `Err(Dropped)`.
    async fn close(&self) -> Result<(), LogError>;
    fn health(&self) -> SinkHealth;
}

/// Read side: sealed rows only.
#[async_trait]
pub trait LogSource<T: LogTable>: Send + Sync {
    /// Returns every sealed row of the partitions `scope` covers: its tenant's, on the days its
    /// window [`covers`](TimeWindow::covers). Narrowing to the instants the window admits is the
    /// table's job, because the port knows days, not a row's time.
    async fn scan(&self, scope: &TenantScope) -> Result<Vec<T::Row>, LogError>
    where
        T: Partitioned;
    /// Returns the rows of [`Self::scan`] whose sort key equals `key`, reading only the row
    /// groups that can hold it.
    async fn scan_key(&self, scope: &TenantScope, key: &T::Key) -> Result<Vec<T::Row>, LogError>
    where
        T: Partitioned;
    /// Returns the rows of [`Self::scan`] that `filter` keeps, greatest key first and at most
    /// `limit`, read as [`PartialSource::scan_partial`] reads: newest day first, the limit counted
    /// after the filter, no older day opened once it is met.
    async fn scan_newest(
        &self,
        scope: &TenantScope,
        filter: &T::Filter,
        limit: Option<usize>,
    ) -> Result<Vec<T::Row>, LogError>
    where
        T: Projected;
}

/// Read side, in summary: [`Projected::Partial`]s, never whole rows.
#[async_trait]
pub trait PartialSource<T: Projected>: Send + Sync {
    /// Returns the summaries of the rows [`LogSource::scan`] would return that `filter` keeps,
    /// greatest key first and at most `limit`. With a
    /// limit, the scope's days are read newest first, each day's summaries
    /// filtered before they count, and reading stops at the first whole day
    /// that brings the count to `limit`: no older day is opened.
    async fn scan_partial(
        &self,
        scope: &TenantScope,
        filter: &T::Filter,
        limit: Option<usize>,
    ) -> Result<Vec<T::Partial>, LogError>;
}

/// Returns `read` greatest key first, cut to `limit`.
fn greatest_first<R, K: Ord>(
    mut read: Vec<R>,
    key: impl Fn(&R) -> K,
    limit: Option<usize>,
) -> Vec<R> {
    read.sort_by_key(|r| std::cmp::Reverse(key(r)));
    if let Some(n) = limit {
        read.truncate(n);
    }
    read
}

#[derive(Debug, Clone, Default)]
pub struct SinkHealth {
    /// `None` ⇔ no row has been lost.
    pub dropped: Option<Loss>,
    /// `None` ⇔ no upkeep (merging, cleanup) has failed. A failed upkeep
    /// loses no row, so it never makes `close()` fail.
    pub upkeep_failed: Option<UpkeepFailure>,
}

/// Upkeep passes that failed so far and why the last one did.
#[derive(Debug, Clone)]
pub struct UpkeepFailure {
    pub times: u64,
    pub last: Arc<LogError>,
}

/// Rows lost so far and why the last one was lost.
#[derive(Debug, Clone, Error)]
#[error("{rows} row(s) never sealed")]
pub struct Loss {
    pub rows: u64,
    #[source]
    pub last: Arc<LogError>,
}

/// Why a sink did not take a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AppendError {
    #[error("log sink is closed")]
    Closed,
    /// The row was dropped, and the sink counts it among its lost rows.
    #[error("log sink buffer is full ({capacity} rows); row dropped")]
    Full { capacity: usize },
}

#[derive(Debug, Error)]
pub enum LogError {
    /// A row the sink refused: the cause of a [`Loss`], or an appending caller's failure.
    #[error(transparent)]
    Append(#[from] AppendError),
    #[error(transparent)]
    Dropped(Loss),
    #[error("log sealer task failed")]
    Sealer(#[from] tokio::task::JoinError),
    /// The blocking task running `work` panicked or was cancelled; a panic's message is in
    /// `source`.
    #[cfg(feature = "parquet")]
    #[error("{work}: the file IO task failed")]
    IoTask {
        work: IoWork,
        source: tokio::task::JoinError,
    },
    #[cfg(feature = "parquet")]
    #[error("{}", path.display())]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[cfg(feature = "parquet")]
    #[error("{}: unreadable manifest", path.display())]
    Manifest {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
    #[cfg(feature = "parquet")]
    #[error("{}: merge wrote {found} row(s), its inputs held {expected}", path.display())]
    RowCountChanged {
        path: std::path::PathBuf,
        expected: u64,
        found: u64,
    },
    #[cfg(feature = "parquet")]
    #[error("{}: row {row} sorts before the row ahead of it; a merge takes only sorted files", path.display())]
    UnsortedMergeInput { path: std::path::PathBuf, row: u64 },
    #[cfg(feature = "parquet")]
    #[error("{}: a row of tenant `{found}` in a partition of tenant `{expected}`", path.display())]
    ForeignRow {
        path: std::path::PathBuf,
        expected: TenantId,
        found: TenantId,
    },
    #[cfg(feature = "parquet")]
    #[error("{}: not a `date=YYYY-MM-DD` partition directory", path.display())]
    PartitionDay { path: std::path::PathBuf },
    #[cfg(feature = "parquet")]
    #[error("{}: rows do not decode", path.display())]
    Decode {
        path: std::path::PathBuf,
        source: ColumnError,
    },
    /// `row` counts from the file's first row.
    #[cfg(feature = "parquet")]
    #[error("{}: row {row} does not decode", path.display())]
    DecodeRow {
        path: std::path::PathBuf,
        row: u64,
        source: ColumnError,
    },
    /// The footer states a negative row count, which no writer produces.
    #[cfg(feature = "parquet")]
    #[error("{}: footer states {rows} rows", path.display())]
    NegativeRowCount { path: std::path::PathBuf, rows: i64 },
    #[cfg(feature = "parquet")]
    #[error("{}: no key column `{column}`", path.display())]
    KeyColumn {
        path: std::path::PathBuf,
        column: &'static str,
    },
    /// The file does not open as Parquet, a damaged footer for one, or a reader of its row groups
    /// cannot be built.
    #[cfg(feature = "parquet")]
    #[error("{}: does not open as Parquet", path.display())]
    Open {
        path: std::path::PathBuf,
        source: ::parquet::errors::ParquetError,
    },
    /// The Parquet writer could not write the file: its sink failed, or the rows do not fit the
    /// schema.
    #[cfg(feature = "parquet")]
    #[error("{}: writing Parquet failed", path.display())]
    Write {
        path: std::path::PathBuf,
        source: ::parquet::errors::ParquetError,
    },
    /// The rows of a group do not encode into the table's columns. Encoding depends on the rows
    /// alone, so writing the same rows again fails the same way.
    #[cfg(feature = "parquet")]
    #[error("{}: rows do not encode", path.display())]
    Encode {
        path: std::path::PathBuf,
        source: EncodeError,
    },
    /// The bucket refused the upload of the frozen file at `path`; `source` is the client's error
    /// as it came back, boxed because it is several times the size of every other variant.
    #[cfg(feature = "parquet")]
    #[error("{}: the upload to the bucket failed", path.display())]
    Upload {
        path: std::path::PathBuf,
        source: Box<object_store::Error>,
    },
    /// The frozen file at `path` has no name in the bucket.
    #[cfg(feature = "parquet")]
    #[error("{}: no object name in the bucket for this file", path.display())]
    ObjectKey {
        path: std::path::PathBuf,
        source: ObjectKeyError,
    },
}
