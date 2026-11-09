//! Parquet adapter: one Parquet file per sealed batch and partition,
//! published into a partition directory whose `_live.json` names the files
//! readers may see. After each seal the writer merges the partitions it
//! sealed into by its [`CompactRule`] — in the sink's upkeep loop, never on
//! the sealing path; merged-away files stay on disk for `grace` under a
//! name a glob of `*.parquet` does not match, and the pass that deletes them
//! is deferred to when the first of them expires. So a glob of
//! `tenant=<t>/<table>/**/*.parquet` reads the live files and no others.
//! When its sink opens, and at the first upkeep of each new day — after a
//! seal, or at UTC midnight when nothing is sealed — it also merges every
//! past day into one file (`sweep`).
//!
//! A writer given a bucket ([`Bucket`]) then freezes each past day's
//! files, which no merge takes in again, and uploads them on a task of
//! its own (`upload`); neither the sealing path nor the upkeep waits on the
//! bucket.
//!
//! On disk: `<root>/tenant=<t>/<table>/date=<YYYY-MM-DD>/{_live.json, .lock,
//! <writer>-<seq>.parquet, L<n>-<sha16>.parquet, <file>.parquet.retired}`,
//! `<t>` percent-encoded so any tenant id is one safe path segment.

mod compact;
mod encode;
mod file;
mod journal;
mod partition;
#[cfg(test)]
pub(crate) mod probe;
mod read;
mod ship;
mod sweep;
mod upload;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::parquet::file::statistics::Statistics;
use arrow_array::RecordBatch;
use arrow_schema::{ArrowError, DataType, SchemaRef};
use async_trait::async_trait;
use itertools::Itertools;
use tars_storage::durable::journal::JournalFileName;
use tars_types::{Millis, TenantId, UtcDay};
use thiserror::Error;
use tokio::runtime::Handle;
use uuid::Uuid;

use super::seal::{RowGroup, row_groups};
use super::sink::{BufferedSink, Coalesce, SealWriter, SinkConfig, UpkeepReport, lock};
use super::{
    LogError, LogSource, LogTable, PartialSource, Partition, Partitioned, Projected, SinkHealth,
    TENANT_PREFIX, TableName, TenantScope, tenant_dir,
};
use compact::DataFile;
use file::Scan;
#[cfg(test)]
use partition::read_listed;
use partition::{DayAge, InFlight, PartitionDir, TmpFile, io_at};
use sweep::{past_partitions, sweep_day};
use upload::Uploader;

const DATE_PREFIX: &str = "date=";

pub use compact::CompactRule;
pub use encode::OffsetLimit;
pub(crate) use encode::{binaries, string_lists, strs};
pub use journal::ParquetTrajectoryLog;
pub(crate) use ship::{Bucket, StoreId};
pub use ship::{BucketSetupError, ObjectKeyError};

/// A [`LogTable`]'s columns: owned by the Parquet adapter, not the port.
pub trait ParquetTable: LogTable {
    /// The column holding `sort_key`; its row-group statistics prune a lookup.
    const KEY_COLUMN: &'static str;

    fn schema() -> SchemaRef;
    /// Encodes `rows` within Arrow's own offsets.
    fn encode(rows: &[Self::Row]) -> Result<RecordBatch, EncodeError> {
        Self::encode_within(rows, OffsetLimit::ARROW)
    }
    /// Encodes `rows`, refusing a string, binary or list column whose offsets pass `limit`.
    fn encode_within(rows: &[Self::Row], limit: OffsetLimit) -> Result<RecordBatch, EncodeError>;
    fn decode(batch: &RecordBatch) -> Result<Vec<Self::Row>, DecodeError>;
    /// Returns the `[min, max]` key range `KEY_COLUMN`'s statistics state, or `None` when they
    /// state none.
    fn key_range(stats: &Statistics) -> Option<(Self::Key, Self::Key)>;
}

/// A [`Projected`] table's summary columns: the reader decodes these and
/// no other, so a large column the summary leaves out is never
/// decompressed.
pub trait ParquetProjected: ParquetTable + Projected {
    /// The columns `decode_partial` reads.
    const PARTIAL_COLUMNS: &'static [&'static str];

    fn decode_partial(batch: &RecordBatch) -> Result<Vec<Self::Partial>, DecodeError>;
}

/// Why rows do not encode into a table's columns. Rows do not know which file they are written
/// to; the writer adds it.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// The text of `column` from its first row through `row` is longer than the 2 GiB a string
    /// column's 32-bit offsets reach.
    #[error("the text of column `{column}` through row {row} is longer than a string column holds")]
    TextTooLong { column: &'static str, row: usize },
    /// The bytes of `column` from its first row through `row` are more than the 2 GiB a binary
    /// column's 32-bit offsets reach.
    #[error("the bytes of column `{column}` through row {row} are more than a binary column holds")]
    BytesTooLong { column: &'static str, row: usize },
    /// The items of `column` from its first row through `row` are more than a list column's
    /// 32-bit offsets count.
    #[error("the items of column `{column}` through row {row} are more than a list column holds")]
    ListTooLong { column: &'static str, row: usize },
    #[error("a value of column `{column}` does not serialize to JSON")]
    Json {
        column: &'static str,
        source: serde_json::Error,
    },
    #[error(transparent)]
    Arrow(#[from] ArrowError),
}

/// Why a batch does not decode into a table's rows. A batch does not know which file it came
/// from or where in it it starts; the reader adds both.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// The batch's columns are not the table's.
    #[error(transparent)]
    Columns(#[from] ColumnError),
    /// One row does not decode; `row` counts from the batch's first row.
    #[error("row {row}")]
    Row {
        row: usize,
        #[source]
        source: ColumnError,
    },
}

/// Why a batch's columns, or one row of them, are not what this table's writer seals.
#[derive(Debug, Error)]
pub enum ColumnError {
    #[error("no column `{column}`")]
    Missing { column: String },
    #[error("column `{column}` is {found}")]
    WrongType { column: String, found: DataType },
    #[error("`{column}` is a list of {found}")]
    WrongItemType { column: String, found: DataType },
    /// The batch's columns, taken by position, are not the table's.
    #[error("not ({expected}): {found:?}")]
    Shape {
        expected: &'static str,
        found: SchemaRef,
    },
    #[error("`{column}` is null")]
    Null { column: String },
    #[error("`{column}` holds a null item")]
    NullItem { column: String },
    /// Columns that are written together hold a combination no writer of ours produces;
    /// `found` is the values as read.
    #[error("{columns} hold no valid combination: {found}")]
    Inconsistent {
        columns: &'static str,
        found: String,
    },
    /// A stored name this build does not know; a build that knew the name wrote it.
    #[error("`{column}` holds `{value}`, which this build does not know")]
    UnknownName {
        column: &'static str,
        value: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("a stored value does not parse")]
    Value(#[source] serde_json::Error),
    #[error("a stored id is not a UUID")]
    Uuid(#[source] uuid::Error),
    #[error("a stored call id is not a call id")]
    CallId(#[source] tars_types::CallIdError),
    #[error("a stored sha256 is not 32 bytes")]
    Sha256(#[source] std::array::TryFromSliceError),
    #[error("a stored tenant is not a tenant id")]
    Tenant(#[source] tars_types::ParseTenantIdError),
    #[error("a stored session id is not a session id")]
    SessionId(#[source] tars_types::SessionIdError),
    #[error(transparent)]
    Arrow(#[from] ArrowError),
}

/// Decodes rows `0..rows` of a batch with `row`, naming the first row that fails.
pub(crate) fn decode_rows<R>(
    rows: usize,
    mut row: impl FnMut(usize) -> Result<R, ColumnError>,
) -> Result<Vec<R>, DecodeError> {
    (0..rows)
        .map(|i| row(i).map_err(|source| DecodeError::Row { row: i, source }))
        .collect()
}

/// A Parquet log rooted at a directory. Opening does no IO; readers see
/// what the manifest lists at the moment they read.
pub struct ParquetLog<T: ParquetTable + Partitioned> {
    root: PathBuf,
    /// Where this log's sinks upload the past days' files; readers do not use it.
    bucket: Option<Bucket>,
    table: PhantomData<fn() -> T>,
}

impl<T: ParquetTable + Partitioned> ParquetLog<T> {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            bucket: None,
            table: PhantomData,
        }
    }

    /// Returns this log with its sinks uploading the past days' files to `bucket`, or never
    /// uploading for `None`.
    pub(crate) fn uploading_to(self, bucket: Option<Bucket>) -> Self {
        Self { bucket, ..self }
    }

    /// Returns a sink sealing into this log and merging each partition it seals into by `compact`
    /// (`None`: never merge), and every past day into one file; with a bucket, it then
    /// freezes and uploads every past day's files.
    /// `config.seal.grace` is how old another writer's `.tmp`, or a
    /// merged-away file, must be before this one deletes it.
    pub fn sink(
        &self,
        config: SinkConfig,
        compact: Option<CompactRule>,
        rt: &Handle,
    ) -> BufferedSink<T> {
        self.sink_on(config, compact, Arc::new(SystemTime::now), rt)
    }

    fn sink_on(
        &self,
        config: SinkConfig,
        compact: Option<CompactRule>,
        clock: Clock,
        rt: &Handle,
    ) -> BufferedSink<T> {
        let health = Arc::<Mutex<SinkHealth>>::default();
        let uploader = self.bucket.clone().map(|bucket| {
            Uploader::spawn(self.root.clone(), bucket, T::NAME, Arc::clone(&health), rt)
        });
        let writer = ParquetWriter {
            root: self.root.clone(),
            grace: config.seal.grace,
            compact,
            clock,
            swept: Mutex::new(None),
            uploader,
            id: WriterId::new(),
            in_flight: InFlight::default(),
            seq: AtomicU64::new(0),
            table: PhantomData,
        };
        BufferedSink::spawn(writer, health, config, rt)
    }

    async fn read_scoped(
        &self,
        scope: &TenantScope,
        scan: Scan<T::Key>,
    ) -> Result<Vec<T::Row>, LogError> {
        let root = self.root.clone();
        let scope = scope.clone();
        blocking(IoWork::Read(T::NAME), move || {
            read::read_scoped::<T>(&root, &scope, &scan)
        })
        .await
    }

    async fn read_newest<D: read::Decode<T>>(
        &self,
        scope: &TenantScope,
        filter: &T::Filter,
        limit: Option<usize>,
    ) -> Result<Vec<D::Out>, LogError>
    where
        T: Projected,
    {
        let root = self.root.clone();
        let scope = scope.clone();
        let filter = filter.clone();
        blocking(IoWork::Read(T::NAME), move || {
            read::read_newest::<T, D>(&root, &scope, &filter, limit)
        })
        .await
    }
}

impl<T: ParquetTable + Partitioned> std::fmt::Debug for ParquetLog<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParquetLog")
            .field("root", &self.root)
            .field("table", &T::NAME.as_str())
            .finish()
    }
}

#[async_trait]
impl<T: ParquetTable + Partitioned> LogSource<T> for ParquetLog<T> {
    async fn scan(&self, scope: &TenantScope) -> Result<Vec<T::Row>, LogError> {
        self.read_scoped(scope, Scan::All).await
    }

    async fn scan_key(&self, scope: &TenantScope, key: &T::Key) -> Result<Vec<T::Row>, LogError> {
        self.read_scoped(scope, Scan::Key(key.clone())).await
    }

    async fn scan_newest(
        &self,
        scope: &TenantScope,
        filter: &T::Filter,
        limit: Option<usize>,
    ) -> Result<Vec<T::Row>, LogError>
    where
        T: Projected,
    {
        self.read_newest::<read::Rows>(scope, filter, limit).await
    }
}

#[async_trait]
impl<T: ParquetProjected> PartialSource<T> for ParquetLog<T> {
    async fn scan_partial(
        &self,
        scope: &TenantScope,
        filter: &T::Filter,
        limit: Option<usize>,
    ) -> Result<Vec<T::Partial>, LogError> {
        self.read_newest::<read::Summaries>(scope, filter, limit)
            .await
    }
}

/// Reads a partition whose manifest named `listed` a moment ago, retrying a file found gone as
/// `read::read_owned_partition` does.
#[cfg(test)]
fn read_partition<T: ParquetTable>(
    dir: &PartitionDir,
    listed: &[PathBuf],
    scan: &Scan<T::Key>,
) -> Result<Vec<T::Row>, LogError> {
    match read_files::<T>(listed, scan) {
        Err(e) if is_gone(&e) => read_files::<T>(&dir.live_files()?, scan),
        read => read,
    }
}

#[cfg(test)]
fn read_files<T: ParquetTable>(
    paths: &[PathBuf],
    scan: &Scan<T::Key>,
) -> Result<Vec<T::Row>, LogError> {
    Ok(paths
        .iter()
        .map(|path| read_listed(path, |path| file::read::<T>(path, scan)))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect())
}

fn is_gone(e: &LogError) -> bool {
    matches!(e, LogError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
}

/// Where a writer reads the time: once per seal or sweep, then passed down.
type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Who names a sealed file (`<writer>-<seq>`) and every `.tmp` it writes
/// (`<writer>-<seq>`, `<writer>-L<n>-<sha16>`), so no two writers collide, and a writer's
/// upkeep can tell its own `.tmp`s — kept only while in flight — from
/// another writer's, kept until `grace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct WriterId(Uuid);

impl WriterId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the data file this writer's seal `seq` is published as.
    fn sealed_file(self, seq: u64) -> DataFile {
        DataFile::of_stem(format_args!("{}-{seq}", self.0))
    }

    /// Returns the `.tmp` this writer's seal `seq` is written to.
    fn seal_tmp(self, seq: u64) -> TmpFile {
        TmpFile::of(self, seq)
    }

    /// Returns the `.tmp` this writer merges `output` into outside the lock, so two writers that
    /// planned the same merge never write one file.
    fn merge_tmp(self, output: &DataFile) -> TmpFile {
        TmpFile::of(self, output.stem())
    }
}

/// One sink's writer.
struct ParquetWriter<T> {
    root: PathBuf,
    grace: Duration,
    compact: Option<CompactRule>,
    clock: Clock,
    /// The last day this writer swept the past days for.
    swept: Mutex<Option<UtcDay>>,
    uploader: Option<Uploader>,
    id: WriterId,
    in_flight: InFlight,
    seq: AtomicU64,
    table: PhantomData<fn() -> T>,
}

/// The upkeep seals owe: the partitions they sealed into, and the clock's
/// reading at the latest of them.
struct DirtyPartitions {
    partitions: BTreeSet<Partition>,
    now: SystemTime,
}

impl Coalesce for DirtyPartitions {
    fn and(mut self, later: Self) -> Self {
        self.partitions.extend(later.partitions);
        self.now = later.now;
        self
    }
}

/// The upkeep time defers: the partition directories holding retired files, due when the first
/// of those files expires, and the sweep of a day that has ended, due at UTC midnight.
struct ExpiringDirs {
    dirs: BTreeSet<PartitionDir>,
    at: SystemTime,
    /// The clock reading of the round that deferred it, from which the wait for `at` is
    /// measured.
    as_of: SystemTime,
}

impl Coalesce for ExpiringDirs {
    fn and(mut self, other: Self) -> Self {
        self.dirs.extend(other.dirs);
        self.at = self.at.min(other.at);
        self.as_of = self.as_of.min(other.as_of);
        self
    }
}

#[async_trait]
impl<T: ParquetTable + Partitioned> SealWriter<T> for ParquetWriter<T> {
    type Due = DirtyPartitions;
    type Deferred = ExpiringDirs;

    async fn upkeep_on_open(&self) -> UpkeepReport<ExpiringDirs> {
        let now = (self.clock)();
        both(self.sweep_if_due(now).await, midnight(now))
    }

    /// Writes one file per partition the batch touches. A failed publish stops the batch;
    /// partitions already published stay visible, and are maintained by the next seal into them
    /// or the next sweep.
    async fn seal(&self, groups: Vec<RowGroup<T>>) -> Result<DirtyPartitions, LogError> {
        let now = (self.clock)();
        let partitions = self.publish(groups).await?;
        Ok(DirtyPartitions { partitions, now })
    }

    /// Does each dirty partition's upkeep, then the past days' sweep if a new day began. A failed
    /// upkeep loses no row: it is reported, and the next round plans it afresh.
    async fn upkeep(&self, dirty: DirtyPartitions) -> UpkeepReport<ExpiringDirs> {
        let dirs = dirty
            .partitions
            .iter()
            .map(|p| partition_dir(&self.root, T::NAME.as_str(), p))
            .collect();
        let maintained = self.maintain(dirs, dirty.now).await;
        let swept = self.sweep_if_due(dirty.now).await;
        both(both(maintained, swept), midnight(dirty.now))
    }

    /// Returns how long until `expiring` is due, measured from the reading of the round that
    /// deferred it. The upkeep loop starts the wait after that round, so once it is over the real
    /// time is past `at`.
    fn wait(&self, expiring: &ExpiringDirs) -> Duration {
        // [arc:intentional-handle] reason: a due time the clock has already passed is due now.
        expiring
            .at
            .duration_since(expiring.as_of)
            .unwrap_or(Duration::ZERO)
    }

    /// Does the deferred upkeep as of the time it fell due, not a fresh reading: the wait ended
    /// after `at`, so no file goes early and a sweep due at midnight sweeps the day that ended.
    async fn upkeep_deferred(&self, expiring: ExpiringDirs) -> UpkeepReport<ExpiringDirs> {
        let at = expiring.at;
        let maintained = self.maintain(expiring.dirs.into_iter().collect(), at).await;
        let swept = self.sweep_if_due(at).await;
        both(both(maintained, swept), midnight(at))
    }

    async fn finish(self) {
        if let Some(uploader) = self.uploader {
            uploader.finish().await;
        }
    }
}

impl<T: ParquetTable + Partitioned> ParquetWriter<T> {
    /// Publishes the batch and returns the partitions it was published into.
    async fn publish(&self, groups: Vec<RowGroup<T>>) -> Result<BTreeSet<Partition>, LogError> {
        self.publish_by(groups, file::write::<T>).await
    }

    /// Publishes as `publish` does; `write`, which writes one partition's `.tmp`, is a parameter
    /// so a test can fail a seal, or run an upkeep, once its `.tmp` is on disk. The `.tmp` is in
    /// flight from before it is written until it is published or the publish failed: claimed on
    /// the blocking thread, so dropping this future never releases a `.tmp` still being written.
    async fn publish_by(
        &self,
        groups: Vec<RowGroup<T>>,
        write: impl Fn(&Path, &[RowGroup<T>]) -> Result<(), LogError> + Send + 'static,
    ) -> Result<BTreeSet<Partition>, LogError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tmp, file) = (self.id.seal_tmp(seq), self.id.sealed_file(seq));
        let root = self.root.clone();
        let in_flight = self.in_flight.clone();
        blocking(IoWork::Seal(T::NAME), move || {
            let _sealing = in_flight.claim(tmp.clone());
            by_partition::<T>(groups)
                .into_iter()
                .map(|(partition, groups)| {
                    let dir = partition_dir(&root, T::NAME.as_str(), &partition);
                    dir.create()?;
                    write(&dir.tmp_path(&tmp), &groups)?;
                    dir.publish(&tmp, file.clone())?;
                    Ok(partition)
                })
                .collect()
        })
        .await
    }

    /// Maintains `dirs`, then queues for upload those whose pass freezes; one whose pass failed
    /// holds nothing new to upload, and the uploader finds that in its manifest.
    async fn maintain(
        &self,
        dirs: Vec<PartitionDir>,
        now: SystemTime,
    ) -> UpkeepReport<ExpiringDirs> {
        let round = self.round(now);
        let frozen: Vec<PartitionDir> = dirs
            .iter()
            .filter(|dir| round.age(dir) == DayAge::Past { freeze: true })
            .cloned()
            .collect();
        let report = blocking(IoWork::Upkeep(T::NAME), move || {
            Ok(maintain_dirs::<T>(dirs, &round))
        })
        .await
        .unwrap_or_else(|failed| UpkeepReport::from_iter([failed]));
        if let Some(uploader) = &self.uploader {
            uploader.queue(frozen);
        }
        report
    }

    /// Merges every past day into one file, once per day this writer sees. A `date=` directory
    /// naming no day is reported and passed over, the others swept; a listing that fails is
    /// reported and leaves the day unswept, so the next round lists again.
    async fn sweep_if_due(&self, now: SystemTime) -> UpkeepReport<ExpiringDirs> {
        let Some(today) = sweep_day(*lock(&self.swept), day_of(now)) else {
            return UpkeepReport::default();
        };
        let root = self.root.clone();
        let listed = blocking(IoWork::Sweep(T::NAME), move || {
            listed_partitions(&root, T::NAME.as_str())
        })
        .await;
        let listed = match listed {
            Ok(listed) => listed,
            Err(failed) => return UpkeepReport::from_iter([failed]),
        };
        *lock(&self.swept) = Some(today);

        let (dirs, unnamed): (Vec<_>, Vec<_>) = listed.into_iter().partition_result();
        let maintained = self.maintain(past_partitions(dirs, today), now).await;
        both(maintained, UpkeepReport::from_iter(unnamed))
    }

    fn round(&self, now: SystemTime) -> Round {
        Round {
            compact: self.compact,
            uploads: self.uploader.is_some(),
            grace: self.grace,
            now,
            writer: self.id,
            in_flight: self.in_flight.clone(),
        }
    }
}

/// What every partition's pass in one round of upkeep shares.
#[derive(Debug, Clone)]
struct Round {
    /// `None`: never merge (cleanup still runs).
    compact: Option<CompactRule>,
    /// Whether the writer has a bucket to upload frozen files to.
    uploads: bool,
    grace: Duration,
    now: SystemTime,
    /// The writer doing the upkeep: it names this round's merge `.tmp`s, and
    /// its own `.tmp`s not in flight are residue at any age.
    writer: WriterId,
    /// The `.tmp`s this writer is sealing or merging right now.
    in_flight: InFlight,
}

impl Round {
    /// Returns how old `dir`'s day is as of this round's clock reading; a past day is frozen for
    /// the bucket only by a writer with one. A clock reading with no day calls no day past.
    fn age(&self, dir: &PartitionDir) -> DayAge {
        match day_of(self.now) {
            Some(today) if is_past(today, dir.day()) => DayAge::Past {
                freeze: self.uploads,
            },
            _ => DayAge::Current,
        }
    }
}

fn is_past(today: UtcDay, day: UtcDay) -> bool {
    day < today
}

/// Maintains each partition on its own, so one that fails does not stop the others, and defers
/// each to when its next retired file expires.
fn maintain_dirs<T: ParquetTable>(
    dirs: Vec<PartitionDir>,
    round: &Round,
) -> UpkeepReport<ExpiringDirs> {
    dirs.into_iter()
        .map(|dir| maintain_dir::<T>(dir, round))
        .fold(UpkeepReport::default(), both)
}

/// Joins two parts of one round's report: the failures of both, and one deferral due at the
/// earlier of the two.
fn both(
    a: UpkeepReport<ExpiringDirs>,
    b: UpkeepReport<ExpiringDirs>,
) -> UpkeepReport<ExpiringDirs> {
    let deferred = match (a.deferred, b.deferred) {
        (Some(a), Some(b)) => Some(a.and(b)),
        (a, b) => a.or(b),
    };
    UpkeepReport {
        failures: a.failures.into_iter().chain(b.failures).collect(),
        deferred,
    }
}

fn maintain_dir<T: ParquetTable>(dir: PartitionDir, round: &Round) -> UpkeepReport<ExpiringDirs> {
    let maintained = dir.maintain::<T>(round, round.age(&dir)).err();

    match dir.next_expiry(round.grace) {
        Ok(expiry) => UpkeepReport {
            failures: maintained.into_iter().collect(),
            deferred: expiry.and_then(|at| expiring(dir, at, round.now)),
        },
        Err(unread) => maintained.into_iter().chain([unread]).collect(),
    }
}

/// Returns `dir`'s pass once `at` comes; `None` unless `at` is still ahead of `now`. A retired
/// file already expired is one this pass failed to delete, and that failure is reported; retrying
/// it at once would spin.
fn expiring(dir: PartitionDir, at: SystemTime, now: SystemTime) -> Option<ExpiringDirs> {
    (at > now).then(|| ExpiringDirs {
        dirs: BTreeSet::from([dir]),
        at,
        as_of: now,
    })
}

/// Returns the sweep due when `now`'s UTC day ends, so a day is swept, frozen and uploaded although
/// nothing is sealed after it.
///
/// A reading with no UTC day (before the epoch, or past what a day can be named for) is reported
/// here, once per round, as a failure carrying the reading; the sweep and the day ages pass over
/// it, so only the tiered merge runs until the clock is set.
fn midnight(now: SystemTime) -> UpkeepReport<ExpiringDirs> {
    match day_of(now).and(next_midnight(now)) {
        Some(at) => UpkeepReport {
            failures: Vec::new(),
            deferred: Some(ExpiringDirs {
                dirs: BTreeSet::new(),
                at,
                as_of: now,
            }),
        },
        None => UpkeepReport::from_iter([LogError::ClockOutOfRange { reading: now }]),
    }
}

const SECS_PER_DAY: u64 = 86_400;

/// Returns the first instant of the UTC day after `now`'s, or `None` for a clock before the epoch
/// or past `SystemTime`'s range.
fn next_midnight(now: SystemTime) -> Option<SystemTime> {
    let days = now.duration_since(UNIX_EPOCH).ok()?.as_secs() / SECS_PER_DAY;
    let secs = days.checked_add(1)?.checked_mul(SECS_PER_DAY)?;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

fn day_of(now: SystemTime) -> Option<UtcDay> {
    epoch_millis(now).and_then(UtcDay::of)
}

/// Returns `at` in epoch ms; `None` for a clock before the epoch or past `Millis`, which
/// [`midnight`] reports.
fn epoch_millis(at: SystemTime) -> Option<Millis> {
    let since = at.duration_since(UNIX_EPOCH).ok()?;
    Millis::try_from(since.as_millis()).ok()
}

/// Splits a sealed batch by partition, each part re-laid into its own sorted row groups, so a file
/// never mixes tenants or days.
fn by_partition<T: Partitioned>(groups: Vec<RowGroup<T>>) -> BTreeMap<Partition, Vec<RowGroup<T>>> {
    let parts = groups.into_iter().flat_map(RowGroup::into_rows).fold(
        BTreeMap::<Partition, Vec<T::Row>>::new(),
        |mut parts, row| {
            parts.entry(T::partition(&row)).or_default().push(row);
            parts
        },
    );
    parts
        .into_iter()
        .map(|(partition, rows)| (partition, row_groups::<T>(rows)))
        .collect()
}

fn partition_dir(root: &Path, table: &str, p: &Partition) -> PartitionDir {
    PartitionDir::new(&table_dir(root, table, &p.tenant), p.day)
}

/// Returns `<root>/tenant=<t>/<table>`, the one directory holding a tenant's partitions of
/// `table`.
fn table_dir(root: &Path, table: &str, tenant: &TenantId) -> PathBuf {
    tenant_dir(root, tenant).join(table)
}

/// Lists every partition directory of `table` across all tenants, each with the day its name
/// states, or the failure of a `date=` directory naming no day, which is never taken for a day it
/// is not.
fn listed_partitions(
    root: &Path,
    table: &str,
) -> Result<Vec<Result<PartitionDir, LogError>>, LogError> {
    Ok(partition_dirs(root, table)?
        .into_iter()
        .map(partition_named)
        .collect())
}

/// Reads each `date=` directory as the partition of the day its name states; one naming no day
/// fails the whole list.
fn dated(dirs: Vec<PathBuf>) -> Result<Vec<PartitionDir>, LogError> {
    dirs.into_iter().map(partition_named).collect()
}

/// Reads a `date=` directory as the partition of the day its name states.
fn partition_named(dir: PathBuf) -> Result<PartitionDir, LogError> {
    let day = dir_day(&dir).ok_or_else(|| LogError::PartitionDay { path: dir.clone() })?;
    let table_dir = dir
        .parent()
        .ok_or_else(|| LogError::PartitionDay { path: dir.clone() })?;
    Ok(PartitionDir::new(table_dir, day))
}

/// Returns the day a `date=<day>` directory names, the inverse of [`PartitionDir::new`].
fn dir_day(dir: &Path) -> Option<UtcDay> {
    day_named(dir.file_name()?.to_str()?.strip_prefix(DATE_PREFIX)?)
}

/// Parses `YYYY-MM-DD`, the inverse of `UtcDay`'s `Display`.
pub(crate) fn day_named(date: &str) -> Option<UtcDay> {
    let midnight = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_utc();
    UtcDay::of(midnight.timestamp_millis())
}

/// Lists every `tenant=*/<table>/date=*` directory under `root`. A root nothing was sealed into
/// is an empty log, not an error.
fn partition_dirs(root: &Path, table: &str) -> Result<Vec<PathBuf>, LogError> {
    let tables = subdirs(root, TENANT_PREFIX)?
        .into_iter()
        .map(|tenant| tenant.join(table));
    let mut dirs = Vec::new();
    for table_dir in tables {
        dirs.extend(subdirs(&table_dir, DATE_PREFIX)?);
    }
    Ok(dirs)
}

/// Lists the subdirectories of `dir` named `<prefix>…`, none when `dir` is absent.
fn subdirs(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>, LogError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_at(dir)(e)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_at(dir))?;
        let is_dir = entry.file_type().map_err(io_at(&entry.path()))?.is_dir();
        if is_dir && entry.file_name().to_string_lossy().starts_with(prefix) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// The file work a blocking task runs, named so a task that panics or is cancelled says which
/// work it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoWork {
    Seal(TableName),
    Upkeep(TableName),
    Sweep(TableName),
    /// Uploading, or recording the upload of, the frozen files at this path.
    Upload(PathBuf),
    Read(TableName),
    StageJournal(JournalFileName),
    ReadJournalGroup {
        file: JournalFileName,
        row_group: u32,
    },
    RemoveJournal(JournalFileName),
    ListJournals,
}

impl std::fmt::Display for IoWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seal(table) => write!(f, "sealing a batch of `{}`", table.as_str()),
            Self::Upkeep(table) => write!(f, "upkeep of `{}`", table.as_str()),
            Self::Sweep(table) => write!(f, "listing the past days of `{}`", table.as_str()),
            Self::Upload(path) => write!(f, "uploading `{}` to the bucket", path.display()),
            Self::Read(table) => write!(f, "reading `{}`", table.as_str()),
            Self::StageJournal(file) => write!(f, "staging journal file `{}`", file.as_str()),
            Self::ReadJournalGroup { file, row_group } => write!(
                f,
                "reading row group {row_group} of journal file `{}`",
                file.as_str()
            ),
            Self::RemoveJournal(file) => write!(f, "removing journal file `{}`", file.as_str()),
            Self::ListJournals => f.write_str("listing journal files"),
        }
    }
}

/// Runs `run`, the file IO of `work`, off the async workers.
async fn blocking<R: Send + 'static>(
    work: IoWork,
    run: impl FnOnce() -> Result<R, LogError> + Send + 'static,
) -> Result<R, LogError> {
    tokio::task::spawn_blocking(run)
        .await
        .map_err(|source| LogError::IoTask { work, source })?
}

#[cfg(test)]
mod tests;
