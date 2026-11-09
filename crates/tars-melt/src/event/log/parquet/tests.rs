use std::io::{BufRead, BufReader};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, mpsc};

use arrow_array::cast::AsArray;
use arrow_array::types::UInt64Type;
use arrow_array::{ArrayRef, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::*;
use crate::event::log::contract::{Harness, TENANT, TestRow, TestTable, contract_suite, scope};
use crate::event::log::seal::row_groups;
use crate::event::log::{ColumnError, LogSink, SealPolicy, SinkHealth, TableName};
use compact::{CompactionPlan, DataFile};

impl ParquetTable for TestTable {
    const KEY_COLUMN: &'static str = "key";

    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("key", DataType::UInt64, false),
            Field::new("body", DataType::Utf8, false),
        ]))
    }

    fn encode_within(rows: &[TestRow], _: OffsetLimit) -> Result<RecordBatch, EncodeError> {
        let keys = UInt64Array::from_iter_values(rows.iter().map(TestRow::key));
        let bodies = StringArray::from_iter_values(rows.iter().map(TestRow::body));
        Ok(RecordBatch::try_new(
            Self::schema(),
            vec![Arc::new(keys) as ArrayRef, Arc::new(bodies)],
        )?)
    }

    fn decode(batch: &RecordBatch) -> Result<Vec<TestRow>, DecodeError> {
        let keys = batch.column(0).as_primitive_opt::<UInt64Type>();
        let bodies = batch.column(1).as_string_opt::<i32>();
        let (Some(keys), Some(bodies)) = (keys, bodies) else {
            return Err(ColumnError::Shape {
                expected: "u64, utf8",
                found: batch.schema(),
            }
            .into());
        };
        Ok((0..batch.num_rows())
            .map(|i| TestRow::new(keys.value(i), bodies.value(i)))
            .collect())
    }

    /// Parquet stores UInt64 as INT64 bits, compared unsigned.
    fn key_range(stats: &Statistics) -> Option<(u64, u64)> {
        let Statistics::Int64(s) = stats else {
            return None;
        };
        let bits = |v: &i64| u64::from_ne_bytes(v.to_ne_bytes());
        Some((bits(s.min_opt()?), bits(s.max_opt()?)))
    }
}

pub(super) fn test_rows(keys: &[u64]) -> Vec<TestRow> {
    keys.iter().map(|&k| TestRow::new(k, "x")).collect()
}

struct TempLog {
    dir: TempDir,
}

impl TempLog {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn log(&self) -> ParquetLog<TestTable> {
        ParquetLog::open(self.dir.path())
    }

    /// The one directory every test row lands in.
    fn partition(&self) -> std::path::PathBuf {
        self.part_dir().path().to_path_buf()
    }

    fn part_dir(&self) -> PartitionDir {
        let p = TestTable::partition(&TestRow::new(0, "x"));
        partition_dir(self.dir.path(), TestTable::NAME.as_str(), &p)
    }
}

#[async_trait]
impl Harness for TempLog {
    async fn sink(&self, config: SinkConfig) -> Result<Box<dyn LogSink<TestTable>>, LogError> {
        // The contract's one partition is on a past day, so with a fan-in of
        // 2 every seal after the first merges: the contract then also proves
        // merging never changes what a reader sees.
        Ok(Box::new(self.log().sink(
            config,
            Some(CompactRule::new(2)),
            &Handle::current(),
        )))
    }

    async fn source(&self) -> Result<Box<dyn LogSource<TestTable>>, LogError> {
        Ok(Box::new(self.log()))
    }
}

contract_suite!(TempLog::new());

fn config(max_rows: usize, grace: Duration) -> SinkConfig {
    SinkConfig {
        seal: SealPolicy {
            max_rows,
            max_bytes: usize::MAX,
            max_age: Duration::from_secs(3600),
            grace,
        },
        capacity: NonZeroUsize::new(64).unwrap(),
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

async fn seal_one(t: &TempLog, key: u64, grace: Duration) {
    let sink = t.log().sink(config(1, grace), None, &Handle::current());
    sink.append(TestRow::new(key, "x")).await.unwrap();
    sink.close().await.unwrap();
}

#[tokio::test]
async fn crash_before_publish_shows_nothing_and_residue_goes_after_grace() {
    let t = TempLog::new();
    let part = t.part_dir();
    part.create().unwrap();
    // A writer killed after fsync, before the locked rename: a full file,
    // never published. And one killed between rename and registration.
    file::write::<TestTable>(&part.tmp_path(&tmp("dead-0")), &row_groups(test_rows(&[7]))).unwrap();
    file::write::<TestTable>(
        &t.partition().join("dead-1.parquet"),
        &row_groups(test_rows(&[8])),
    )
    .unwrap();
    assert!(t.log().scan(&scope()).await.unwrap().is_empty());

    seal_one(&t, 1, Duration::from_secs(3600)).await;
    assert!(
        names(&t.partition()).contains(&"dead-0.tmp".to_owned()),
        "a .tmp younger than grace may be a live writer's"
    );
    assert!(!names(&t.partition()).contains(&"dead-1.parquet".to_owned()));

    seal_one(&t, 2, Duration::ZERO).await;
    assert!(!names(&t.partition()).iter().any(|n| n.ends_with(".tmp")));
    let mut rows = t.log().scan(&scope()).await.unwrap();
    rows.sort_by_key(TestRow::key);
    assert_eq!(rows, test_rows(&[1, 2]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sinks_on_one_partition_lose_no_registration() {
    const PER_SINK: u64 = 40;
    let t = TempLog::new();
    let a = t.log().sink(
        config(1, Duration::from_secs(3600)),
        None,
        &Handle::current(),
    );
    let b = t.log().sink(
        config(1, Duration::from_secs(3600)),
        None,
        &Handle::current(),
    );

    async fn fill(sink: &BufferedSink<TestTable>, keys: std::ops::Range<u64>) {
        for key in keys {
            sink.append(TestRow::new(key, "x")).await.unwrap();
        }
    }
    tokio::join!(fill(&a, 0..PER_SINK), fill(&b, 1000..1000 + PER_SINK));
    a.close().await.unwrap();
    b.close().await.unwrap();

    let parquet_files = names(&t.partition())
        .into_iter()
        .filter(|n| n.ends_with(".parquet"))
        .count();
    assert_eq!(parquet_files as u64, 2 * PER_SINK);
    assert_eq!(
        t.log().scan(&scope()).await.unwrap().len() as u64,
        2 * PER_SINK
    );
}

// ── Compaction ────────────────────────────────────────────────────

const HOUR: Duration = Duration::from_secs(3600);

impl TempLog {
    fn part(&self) -> PartitionDir {
        let part = self.part_dir();
        part.create().unwrap();
        part
    }

    async fn rows(&self) -> Vec<TestRow> {
        let mut rows = self.log().scan(&scope()).await.unwrap();
        rows.sort_by_key(TestRow::key);
        rows
    }

    fn manifest_bytes(&self) -> Vec<u8> {
        std::fs::read(self.partition().join("_live.json")).unwrap()
    }
}

/// Returns the `.tmp` named `<stem>.tmp`.
fn tmp(stem: &str) -> TmpFile {
    TmpFile::parse(&format!("{stem}.tmp")).unwrap()
}

/// Seal `keys` as the file `<stem>.parquet` and publish it, as a sink would.
fn publish(part: &PartitionDir, stem: &str, keys: &[u64]) {
    file::write::<TestTable>(&part.tmp_path(&tmp(stem)), &row_groups(test_rows(keys))).unwrap();
    part.publish(&tmp(stem), DataFile::of_stem(stem)).unwrap();
}

/// Four files, six rows: one merge's worth at a fan-in of 4.
fn publish_four(part: &PartitionDir) {
    publish(part, "f0", &[3, 1]);
    publish(part, "f1", &[2]);
    publish(part, "f2", &[9, 4]);
    publish(part, "f3", &[5]);
}

const FOUR_INPUTS: [&str; 4] = ["f0.parquet", "f1.parquet", "f2.parquet", "f3.parquet"];

fn upkeep(fan_in: usize, grace: Duration, now: SystemTime) -> Round {
    Round {
        compact: Some(CompactRule::new(fan_in)),
        uploads: false,
        grace,
        now,
        writer: WriterId::new(),
        in_flight: InFlight::default(),
    }
}

fn live_names(part: &PartitionDir) -> Vec<String> {
    part.live_files()
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn a_merge_changes_the_files_but_not_the_rows_a_reader_sees() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let before = t.rows().await;

    part.maintain::<TestTable>(&upkeep(4, HOUR, SystemTime::now()), DayAge::Current)
        .unwrap();

    assert_eq!(t.rows().await, before);
    assert_eq!(before, test_rows(&[1, 2, 3, 4, 5, 9]));
    let live = live_names(&part);
    assert!(live.len() == 1 && live[0].starts_with("L1-"), "{live:?}");
    assert_eq!(
        t.log().scan_key(&scope(), &4).await.unwrap(),
        test_rows(&[4])
    );
}

#[tokio::test]
async fn a_merge_that_fails_midway_leaves_the_manifest_and_rows_untouched() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let (manifest, rows) = (t.manifest_bytes(), t.rows().await);
    let merger = upkeep(4, HOUR, SystemTime::now());
    // The merge's output cannot be created: a directory sits at its path.
    std::fs::create_dir(part.tmp_path(&merger.writer.merge_tmp(&four_inputs_output()))).unwrap();

    let failed = part.maintain::<TestTable>(&merger, DayAge::Current);

    assert!(matches!(failed, Err(LogError::Io { .. })), "{failed:?}");
    assert_eq!(t.manifest_bytes(), manifest, "no half-replaced manifest");
    assert_eq!(t.rows().await, rows);
    assert!(
        !names(&t.partition())
            .iter()
            .any(|n| n.starts_with("L1-") && n.ends_with(".parquet"))
    );
}

#[tokio::test]
async fn merged_away_files_stay_on_disk_until_grace_has_passed() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let t0 = SystemTime::now();
    let grace = Duration::from_secs(600);
    let on_disk = |t: &TempLog| {
        let found = names(&t.partition());
        FOUR_INPUTS
            .iter()
            .filter(|f| found.contains(&format!("{f}.retired")))
            .count()
    };

    part.maintain::<TestTable>(&upkeep(4, grace, t0), DayAge::Current)
        .unwrap();
    assert_eq!(on_disk(&t), 4, "retired, not deleted");
    assert!(
        FOUR_INPUTS
            .iter()
            .all(|f| !names(&t.partition()).contains(&f.to_string())),
        "a retired file keeps no name a glob of *.parquet finds"
    );
    part.maintain::<TestTable>(
        &upkeep(4, grace, t0 + grace - Duration::from_secs(1)),
        DayAge::Current,
    )
    .unwrap();
    assert_eq!(on_disk(&t), 4, "still inside grace");

    part.maintain::<TestTable>(&upkeep(4, grace, t0 + grace), DayAge::Current)
        .unwrap();
    assert_eq!(on_disk(&t), 0, "past grace: deleted");
    assert_eq!(t.rows().await, test_rows(&[1, 2, 3, 4, 5, 9]));
}

#[tokio::test]
async fn a_reader_holding_a_listing_from_before_the_merge_rereads_the_manifest() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let stale = part.live_files().unwrap();

    // Grace zero: the inputs are merged, retired and deleted in one pass.
    part.maintain::<TestTable>(
        &upkeep(4, Duration::ZERO, SystemTime::now()),
        DayAge::Current,
    )
    .unwrap();
    assert!(!stale[0].exists());

    let mut rows = read_partition::<TestTable>(&part, &stale, &Scan::All).unwrap();
    rows.sort_by_key(TestRow::key);
    assert_eq!(rows, test_rows(&[1, 2, 3, 4, 5, 9]));
}

#[test]
fn a_reader_holding_a_listing_from_before_the_merge_reads_the_retired_files_inside_grace() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let stale = part.live_files().unwrap();

    part.maintain::<TestTable>(&upkeep(4, HOUR, SystemTime::now()), DayAge::Current)
        .unwrap();
    assert!(!stale[0].exists());

    let mut rows = read_files::<TestTable>(&stale, &Scan::All).unwrap();
    rows.sort_by_key(TestRow::key);
    assert_eq!(rows, test_rows(&[1, 2, 3, 4, 5, 9]));
}

// ── A glob of `*.parquet` reads what the manifest lists ───────────

/// Reads every `*.parquet` file under `root`, at any depth, as an outside
/// tool's glob of `tenant=T/<table>/**/*.parquet` does.
fn globbed_rows(root: &Path) -> Vec<TestRow> {
    fn parquet_under(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                parquet_under(&path, found);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                found.push(path);
            }
        }
    }
    let mut files = Vec::new();
    parquet_under(root, &mut files);
    let mut rows: Vec<TestRow> = files
        .iter()
        .flat_map(|f| file::read::<TestTable>(f, &Scan::All).unwrap())
        .collect();
    rows.sort_by_key(TestRow::key);
    rows
}

#[tokio::test]
async fn a_glob_reads_the_manifests_rows_after_a_merge_inside_grace() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);

    part.maintain::<TestTable>(&upkeep(4, HOUR, SystemTime::now()), DayAge::Current)
        .unwrap();

    assert_eq!(globbed_rows(t.dir.path()), t.rows().await);
    assert_eq!(t.rows().await, test_rows(&[1, 2, 3, 4, 5, 9]));
}

/// A writer died landing a merge, after renaming its output in and before
/// replacing the manifest: the output is on disk under its own name, and
/// the manifest still lists the inputs.
#[tokio::test]
async fn a_glob_reads_the_manifests_rows_once_a_pass_follows_a_crash_before_the_manifest() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let output = part.file_path(&four_inputs_output());
    file::merge::<TestTable>(&part.live_files().unwrap(), &output).unwrap();
    assert_ne!(
        globbed_rows(t.dir.path()),
        t.rows().await,
        "the glob counts the merged rows twice"
    );

    part.maintain::<TestTable>(
        &Round {
            compact: None,
            ..upkeep(4, HOUR, SystemTime::now())
        },
        DayAge::Current,
    )
    .unwrap();

    assert!(!output.exists());
    assert_eq!(globbed_rows(t.dir.path()), t.rows().await);
    assert_eq!(t.rows().await, test_rows(&[1, 2, 3, 4, 5, 9]));
}

/// A writer died landing a merge after replacing the manifest and before
/// renaming the inputs out; a manifest written before retired files were
/// renamed leaves the same directory.
#[tokio::test]
async fn a_glob_reads_the_manifests_rows_once_a_pass_follows_a_crash_after_the_manifest() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    part.maintain::<TestTable>(&upkeep(4, HOUR, SystemTime::now()), DayAge::Current)
        .unwrap();
    for input in FOUR_INPUTS {
        let at = t.partition().join(input);
        std::fs::rename(at.with_extension("parquet.retired"), &at).unwrap();
    }
    assert_ne!(
        globbed_rows(t.dir.path()),
        t.rows().await,
        "the glob counts the merged rows twice"
    );

    part.maintain::<TestTable>(&upkeep(4, HOUR, SystemTime::now()), DayAge::Current)
        .unwrap();

    assert_eq!(globbed_rows(t.dir.path()), t.rows().await);
    assert_eq!(t.rows().await, test_rows(&[1, 2, 3, 4, 5, 9]));
    assert!(
        FOUR_INPUTS
            .iter()
            .all(|f| t.partition().join(format!("{f}.retired")).exists()),
        "hidden, not deleted, inside grace"
    );
}

#[tokio::test]
async fn merges_cascade_up_the_levels_and_keep_every_row() {
    let t = TempLog::new();
    let part = t.part();
    let now = SystemTime::now();

    for key in 0..20u64 {
        publish(&part, &format!("s{key}"), &[key]);
        part.maintain::<TestTable>(&upkeep(4, HOUR, now), DayAge::Current)
            .unwrap();
    }

    // 20 = 1·16 + 1·4: one level-2 file and one level-1 file.
    let live = live_names(&part);
    let mut levels: Vec<&str> = live.iter().map(|n| &n[..2]).collect();
    levels.sort();
    assert_eq!(levels, vec!["L1", "L2"], "{live:?}");
    assert_eq!(t.rows().await, test_rows(&(0..20).collect::<Vec<_>>()));
}

#[tokio::test]
async fn a_past_day_merges_into_one_file_below_the_fan_in() {
    let t = TempLog::new();
    let part = t.part();
    publish(&part, "f0", &[2]);
    publish(&part, "f1", &[1]);
    publish(&part, "f2", &[3]);

    let past = upkeep(16, HOUR, SystemTime::now());
    part.maintain::<TestTable>(&past, DayAge::Past { freeze: false })
        .unwrap();

    assert_eq!(live_names(&part).len(), 1);
    assert_eq!(t.rows().await, test_rows(&[1, 2, 3]));
}

#[tokio::test]
async fn a_sink_merges_the_partitions_it_seals_into() {
    let t = TempLog::new();
    let sink = t.log().sink(
        config(1, HOUR),
        Some(CompactRule::new(16)),
        &Handle::current(),
    );
    for key in 0..20u64 {
        sink.append(TestRow::new(key, "x")).await.unwrap();
    }
    sink.close().await.unwrap();

    // The test partition's day (1970-01-01) is past: one file, every row.
    assert_eq!(live_names(&t.part_dir()).len(), 1);
    assert_eq!(t.rows().await, test_rows(&(0..20).collect::<Vec<_>>()));
}

// ── Past-day sweep and upkeep health ────────────────────────

/// Test rows partitioned by their key read as epoch ms, so one sink writes
/// into several days.
struct DayTable;

impl LogTable for DayTable {
    type Row = TestRow;
    type Key = u64;

    const NAME: TableName = TableName::new("day_test");
    const ROW_GROUP_ROWS: NonZeroUsize = TestTable::ROW_GROUP_ROWS;

    fn sort_key(row: &TestRow) -> u64 {
        row.key()
    }

    fn approx_bytes(row: &TestRow) -> usize {
        TestTable::approx_bytes(row)
    }
}

impl Partitioned for DayTable {
    fn partition(row: &TestRow) -> Partition {
        let at = Millis::try_from(row.key()).unwrap();
        Partition {
            tenant: tars_types::TenantId::new(TENANT),
            day: UtcDay::of(at).unwrap(),
        }
    }
}

impl ParquetTable for DayTable {
    const KEY_COLUMN: &'static str = TestTable::KEY_COLUMN;

    fn schema() -> SchemaRef {
        TestTable::schema()
    }

    fn encode_within(rows: &[TestRow], limit: OffsetLimit) -> Result<RecordBatch, EncodeError> {
        TestTable::encode_within(rows, limit)
    }

    fn decode(batch: &RecordBatch) -> Result<Vec<TestRow>, DecodeError> {
        TestTable::decode(batch)
    }

    fn key_range(stats: &Statistics) -> Option<(u64, u64)> {
        TestTable::key_range(stats)
    }
}

const DAY_MS: u64 = 86_400_000;
/// Noon of an arbitrary day; `NEXT_DAY` is just past the following midnight.
const DAY: u64 = 20_000 * DAY_MS + DAY_MS / 2;
const NEXT_DAY: u64 = 20_001 * DAY_MS + 1_000;

/// A clock reading `times` (epoch ms) in turn, then staying on the last. A writer reads it once
/// as its sink opens and once per seal, but the opening round and the sealer read it from two
/// loops in no fixed order, so readings that differ go to whichever comes first; [`opens_on`]
/// tells them apart.
fn scripted(times: &[u64]) -> Clock {
    let next = AtomicUsize::new(0);
    let times = times.to_vec();
    Arc::new(move || {
        let i = next.fetch_add(1, Ordering::SeqCst).min(times.len() - 1);
        UNIX_EPOCH + Duration::from_millis(times[i])
    })
}

/// A clock the sink's opening round reads as `opened` and the seals after it read as `seals` in
/// turn, staying on the last; the receiver hears once the opening round has read it. Until a row
/// is appended nothing but the opening round reads the clock, so a test that awaits the receiver
/// before appending knows which reading goes where.
fn opens_on(opened: u64, seals: &[u64]) -> (Clock, oneshot::Receiver<()>) {
    let (tx, rx) = oneshot::channel();
    let first = Mutex::new(Some(tx));
    let seals = scripted(seals);
    let clock: Clock = Arc::new(move || match first.lock().unwrap().take() {
        Some(tx) => {
            tx.send(()).unwrap();
            UNIX_EPOCH + Duration::from_millis(opened)
        }
        None => seals(),
    });
    (clock, rx)
}

struct DayLog {
    dir: TempDir,
}

impl DayLog {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn log(&self) -> ParquetLog<DayTable> {
        ParquetLog::open(self.dir.path())
    }

    fn sink(&self, clock: Clock) -> BufferedSink<DayTable> {
        self.log().sink_on(
            config(1, HOUR),
            Some(CompactRule::new(16)),
            clock,
            &Handle::current(),
        )
    }

    /// The partition holding rows keyed on `day_ms`'s day, created.
    fn part(&self, day_ms: u64) -> PartitionDir {
        let p = DayTable::partition(&TestRow::new(day_ms, "x"));
        let part = partition_dir(self.dir.path(), DayTable::NAME.as_str(), &p);
        part.create().unwrap();
        part
    }

    async fn rows(&self) -> Vec<TestRow> {
        let mut rows = self.log().scan(&scope()).await.unwrap();
        rows.sort_by_key(TestRow::key);
        rows
    }
}

/// Three files on `DAY`, below the fan-in of 16.
fn publish_three_on_day(part: &PartitionDir) {
    publish(part, "f0", &[DAY]);
    publish(part, "f1", &[DAY + 1]);
    publish(part, "f2", &[DAY + 2]);
}

#[tokio::test]
async fn past_midnight_the_first_seal_merges_yesterday_into_one_file() {
    let t = DayLog::new();
    // Opens on `DAY`, seals two rows on it, then seals one past midnight.
    let (clock, opened) = opens_on(DAY, &[DAY, DAY, NEXT_DAY]);
    let sink = t.sink(clock);
    opened.await.unwrap();
    for key in [DAY, DAY + 1, NEXT_DAY] {
        sink.append(TestRow::new(key, "x")).await.unwrap();
    }
    sink.close().await.unwrap();

    assert_eq!(live_names(&t.part(DAY)).len(), 1, "yesterday is one file");
    assert_eq!(t.rows().await, test_rows(&[DAY, DAY + 1, NEXT_DAY]));
}

#[tokio::test]
async fn opening_a_sink_merges_every_past_day_into_one_file() {
    let t = DayLog::new();
    publish_three_on_day(&t.part(DAY));
    let today = t.part(NEXT_DAY);
    publish(&today, "g0", &[NEXT_DAY]);
    publish(&today, "g1", &[NEXT_DAY + 1]);

    let sink = t.sink(scripted(&[NEXT_DAY]));
    sink.close().await.unwrap();

    assert_eq!(
        live_names(&t.part(DAY)).len(),
        1,
        "the past day is one file"
    );
    assert_eq!(live_names(&today).len(), 2, "today is left to the tiers");
    assert_eq!(
        t.rows().await,
        test_rows(&[DAY, DAY + 1, DAY + 2, NEXT_DAY, NEXT_DAY + 1])
    );
}

#[tokio::test]
async fn a_failed_merge_shows_in_health_with_its_error_and_loses_no_row() {
    let t = DayLog::new();
    let part = t.part(DAY);
    publish_three_on_day(&part);
    // The past day's merge output cannot be published: a directory sits at
    // its path. (Its `.tmp` is named by the sink's writer, unknown here.)
    let output = CompactionPlan {
        inputs: ["f0.parquet", "f1.parquet", "f2.parquet"]
            .map(|f| DataFile::parse(f).unwrap())
            .to_vec(),
        level: 1,
    }
    .output();
    let blocked = part.file_path(&output);
    std::fs::create_dir(&blocked).unwrap();

    let sink = t.sink(scripted(&[NEXT_DAY]));
    sink.append(TestRow::new(NEXT_DAY, "x")).await.unwrap();
    let closed = sink.close().await;

    let health = sink.health();
    let failed = health.upkeep_failed.expect("the failed merge is reported");
    assert_eq!(failed.times, 1);
    assert!(
        matches!(&*failed.last, LogError::Io { path, .. } if path == &blocked),
        "{:?}",
        failed.last
    );
    assert!(health.dropped.is_none(), "{:?}", health.dropped);
    assert!(closed.is_ok(), "{closed:?}");
    assert_eq!(live_names(&part).len(), 3, "the manifest is left as it was");
    assert_eq!(
        t.rows().await,
        test_rows(&[DAY, DAY + 1, DAY + 2, NEXT_DAY])
    );
}

#[test]
fn a_partition_directory_name_reads_back_as_its_day() {
    let p = DayTable::partition(&TestRow::new(DAY, "x"));
    let dir = partition_dir(Path::new("/log"), DayTable::NAME.as_str(), &p);
    assert_eq!(dir_day(dir.path()), Some(p.day));
    assert_eq!(dir_day(Path::new("/log/date=2026-13-01")), None);
    assert_eq!(dir_day(Path::new("/log/date=yesterday")), None);
}

#[test]
fn only_a_day_before_today_is_past() {
    let day = |ms: u64| UtcDay::of(Millis::try_from(ms).unwrap()).unwrap();
    let today = day(NEXT_DAY);

    assert!(is_past(today, day(DAY)), "yesterday");
    assert!(!is_past(today, day(NEXT_DAY)), "today");
    assert!(!is_past(day(DAY), day(NEXT_DAY)), "tomorrow");
}

// ── Upkeep never stalls sealing ───────────────────────────────────

/// Rows `try_append`ed one at a time while the past-day merge is held up:
/// the sealer must keep draining the channel, so none is dropped. The merge
/// is held up by holding its partition's lock — what another writer's long
/// merge of that day does — and let go only before `close`. Each row is read
/// back before the next is sent, however long its seal takes; a sealer that
/// waits on the merge never shows the first row, and the test hangs.
async fn try_append_while_the_past_day_merge_is_held(
    (clock, opened): (Clock, oneshot::Receiver<()>),
) -> (SinkHealth, Vec<TestRow>) {
    const CAPACITY: usize = 4;
    const ROWS: u64 = 3 * CAPACITY as u64;

    let t = DayLog::new();
    let past = t.part(DAY);
    publish_three_on_day(&past);
    let merge_held = past.lock().unwrap();

    let sink = t.log().sink_on(
        SinkConfig {
            capacity: NonZeroUsize::new(CAPACITY).unwrap(),
            ..config(1, HOUR)
        },
        Some(CompactRule::new(16)),
        clock,
        &Handle::current(),
    );
    opened.await.unwrap();
    for key in (0..ROWS).map(|i| NEXT_DAY + i) {
        // A full channel is what this test is about; its count is in health.
        let _full = sink.try_append(TestRow::new(key, "x"));
        visible(&t, key).await;
    }

    drop(merge_held);
    let _dropped = sink.close().await;
    (sink.health(), t.rows().await)
}

async fn visible(t: &DayLog, key: u64) {
    while t.log().scan_key(&scope(), &key).await.unwrap().is_empty() {}
}

#[tokio::test]
async fn rows_keep_sealing_while_the_sweep_on_open_is_held_up() {
    let (health, rows) =
        try_append_while_the_past_day_merge_is_held(opens_on(NEXT_DAY, &[NEXT_DAY])).await;

    assert!(health.dropped.is_none(), "{:?}", health.dropped);
    assert_eq!(rows.len(), 3 + 3 * 4, "every row written reads back");
}

#[tokio::test]
async fn rows_keep_sealing_while_the_first_sweep_past_midnight_is_held_up() {
    let (health, rows) =
        try_append_while_the_past_day_merge_is_held(opens_on(DAY, &[NEXT_DAY])).await;

    assert!(health.dropped.is_none(), "{:?}", health.dropped);
    assert_eq!(rows.len(), 3 + 3 * 4, "every row written reads back");
}

// ── A writer's upkeep beside its own sealer ───────────────────────

/// The sealer writes its `.tmp` outside the lock and publishes it under the
/// lock; the same writer's upkeep may take the lock in between. Here it
/// does, with the `.tmp` older than grace — what Linux's coarse file clock
/// makes of a grace of zero, or a long wait for the lock makes of any grace.
#[tokio::test]
async fn a_writers_own_tmp_on_its_way_to_publication_survives_its_upkeep() {
    let t = TempLog::new();
    let part = t.part();
    let writer = WriterId::new();
    let stem = writer.seal_tmp(0);
    let long_after = Round {
        compact: None,
        writer,
        ..upkeep(16, HOUR, SystemTime::now() + 2 * HOUR)
    };
    // The sealer claims its `.tmp` before writing it, as `publish_by` does.
    let _sealing = long_after.in_flight.claim(stem.clone());
    file::write::<TestTable>(&part.tmp_path(&stem), &row_groups(test_rows(&[1]))).unwrap();

    part.maintain::<TestTable>(&long_after, DayAge::Current)
        .unwrap();
    let published = part.publish(&stem, writer.sealed_file(0));

    assert!(published.is_ok(), "the row was lost: {published:?}");
    assert_eq!(t.rows().await, test_rows(&[1]));
}

// ── A merge runs outside the partition lock ───────────────────────

/// A merge on its own thread, held once its output `.tmp` is written —
/// outside the lock, before it lands.
struct HeldMerge {
    go: mpsc::Sender<()>,
    merger: std::thread::JoinHandle<Result<(), LogError>>,
}

impl HeldMerge {
    /// Returns once the merge is held.
    fn start(part: &PartitionDir, round: Round) -> Self {
        let (held_tx, held) = mpsc::channel();
        let (go, go_rx) = mpsc::channel::<()>();
        let part = part.clone();
        let merger = std::thread::spawn(move || {
            part.maintain_by(&round, DayAge::Current, |inputs, out| {
                file::merge::<TestTable>(inputs, out)?;
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                Ok(())
            })
        });
        held.recv().unwrap();
        Self { go, merger }
    }

    fn release(self) -> Result<(), LogError> {
        self.go.send(()).unwrap();
        self.merger.join().unwrap()
    }
}

fn rows_of(part: &PartitionDir) -> Vec<TestRow> {
    let mut rows =
        read_partition::<TestTable>(part, &part.live_files().unwrap(), &Scan::All).unwrap();
    rows.sort_by_key(TestRow::key);
    rows
}

fn four_inputs_output() -> DataFile {
    CompactionPlan {
        inputs: FOUR_INPUTS.map(|f| DataFile::parse(f).unwrap()).to_vec(),
        level: 1,
    }
    .output()
}

/// The merge is released only after the publish returns, so a publish that waits on the merge
/// hangs the test.
#[test]
fn a_publish_lands_while_a_merge_of_its_partition_is_in_flight() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let merge = HeldMerge::start(&part, upkeep(4, HOUR, SystemTime::now()));

    file::write::<TestTable>(&part.tmp_path(&tmp("late")), &row_groups(test_rows(&[42]))).unwrap();
    let published = part.publish(&tmp("late"), DataFile::of_stem("late"));
    let seen = rows_of(&part);
    let landed = merge.release();

    assert!(published.is_ok(), "{published:?}");
    assert_eq!(
        seen,
        test_rows(&[1, 2, 3, 4, 5, 9, 42]),
        "visible before the merge lands"
    );
    assert!(landed.is_ok(), "{landed:?}");
    assert_eq!(rows_of(&part), test_rows(&[1, 2, 3, 4, 5, 9, 42]));
    assert_eq!(
        live_names(&part),
        vec![
            "late.parquet".to_owned(),
            four_inputs_output().as_str().to_owned()
        ]
    );
}

#[test]
fn a_merge_whose_inputs_another_merge_landed_first_lands_nothing() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let slow = upkeep(4, HOUR, SystemTime::now());
    let slow_tmp = part.tmp_path(&slow.writer.merge_tmp(&four_inputs_output()));
    let merge = HeldMerge::start(&part, slow);

    // Another writer planned the same merge, and lands it first.
    part.maintain::<TestTable>(&upkeep(4, HOUR, SystemTime::now()), DayAge::Current)
        .unwrap();
    let (manifest, rows) = (t.manifest_bytes(), rows_of(&part));
    let landed = merge.release();

    assert!(landed.is_ok(), "{landed:?}");
    assert_eq!(rows, test_rows(&[1, 2, 3, 4, 5, 9]));
    assert_eq!(
        rows_of(&part),
        rows,
        "the stale merge landed its rows again"
    );
    assert_eq!(
        t.manifest_bytes(),
        manifest,
        "the stale merge changed the manifest"
    );
    assert!(!slow_tmp.exists(), "the stale merge's .tmp is left behind");
}

/// The merger's own `.tmp` is older than grace when its writer's next
/// sweep runs — what a grace of zero makes of any `.tmp`.
#[test]
fn a_merges_own_tmp_survives_its_writers_sweep_while_it_waits_to_land() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let merger = upkeep(4, HOUR, SystemTime::now());
    let merge = HeldMerge::start(&part, merger.clone());

    let sweep = Round {
        compact: None,
        grace: Duration::ZERO,
        now: SystemTime::now() + HOUR,
        ..merger
    };
    part.maintain::<TestTable>(&sweep, DayAge::Current).unwrap();
    let landed = merge.release();

    assert!(landed.is_ok(), "the merge's output was swept: {landed:?}");
    assert_eq!(
        live_names(&part),
        vec![four_inputs_output().as_str().to_owned()]
    );
    assert_eq!(rows_of(&part), test_rows(&[1, 2, 3, 4, 5, 9]));
}

// ── Deferred expiry ───────────────────────────────────────────────

/// A past day gets no more seals, so nothing but the deferral brings its
/// upkeep back once grace has passed. Time is tokio's, paused: it moves only
/// when every task waits on a timer, never while file IO is running.
#[tokio::test(start_paused = true)]
async fn a_past_days_merged_away_files_go_once_grace_passes_without_another_seal() {
    let t = DayLog::new();
    let past = t.part(DAY);
    publish_three_on_day(&past);
    let past_dir = partition_dir(
        t.dir.path(),
        DayTable::NAME.as_str(),
        &DayTable::partition(&TestRow::new(DAY, "x")),
    )
    .path()
    .to_path_buf();
    let on_disk = || {
        let found = names(&past_dir);
        ["f0.parquet", "f1.parquet", "f2.parquet"]
            .iter()
            .filter(|f| found.contains(&format!("{f}.retired")))
            .count()
    };

    // The sweep on open merges the past day, retiring its inputs.
    let sink = t.sink(scripted(&[NEXT_DAY]));
    tokio::time::sleep(HOUR - Duration::from_secs(1)).await;
    assert_eq!(live_names(&past).len(), 1, "the past day is one file");
    assert_eq!(on_disk(), 3, "retired, still inside grace");

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(on_disk(), 0, "past grace: deleted, with no seal since");

    sink.close().await.unwrap();
    assert!(sink.health().upkeep_failed.is_none());
    assert_eq!(t.rows().await, test_rows(&[DAY, DAY + 1, DAY + 2]));
}

// ── A merge's leftovers ──────────────────────────────────────────

#[test]
fn a_writers_failed_merge_leaves_no_tmp_behind_its_next_pass() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let merger = upkeep(4, HOUR, SystemTime::now());
    let tmp = part.tmp_path(&merger.writer.merge_tmp(&four_inputs_output()));

    let failed = part.maintain_by(&merger, DayAge::Current, |inputs, out| {
        file::merge::<TestTable>(inputs, out)?;
        Err(LogError::RowCountChanged {
            path: out.to_path_buf(),
            expected: 6,
            found: 5,
        })
    });
    assert!(
        matches!(failed, Err(LogError::RowCountChanged { .. })),
        "{failed:?}"
    );
    assert!(tmp.exists(), "the failed merge wrote its .tmp");

    // The same writer's next pass, well inside grace, with nothing to merge.
    let next = Round {
        compact: None,
        ..merger
    };
    part.maintain::<TestTable>(&next, DayAge::Current).unwrap();

    assert!(!tmp.exists(), "the failed merge's .tmp is left behind");
    assert_eq!(live_names(&part), FOUR_INPUTS.map(String::from).to_vec());
}

/// The merge is held between planning and reading its inputs — outside the
/// lock — while another writer lands the same merge and, with a grace
/// shorter than the merge, deletes the inputs.
#[test]
fn a_merge_whose_inputs_were_deleted_under_it_is_given_up_not_failed() {
    let t = TempLog::new();
    let part = t.part();
    publish_four(&part);
    let now = SystemTime::now();
    let slow = upkeep(4, Duration::ZERO, now);
    let slow_tmp = part.tmp_path(&slow.writer.merge_tmp(&four_inputs_output()));

    let given_up = part.maintain_by(&slow, DayAge::Current, |inputs, out| {
        part.maintain::<TestTable>(&upkeep(4, Duration::ZERO, now), DayAge::Current)?;
        assert!(!inputs[0].exists(), "the other writer deleted the inputs");
        file::merge::<TestTable>(inputs, out)
    });

    assert!(given_up.is_ok(), "{given_up:?}");
    assert_eq!(
        live_names(&part),
        vec![four_inputs_output().as_str().to_owned()]
    );
    assert_eq!(rows_of(&part), test_rows(&[1, 2, 3, 4, 5, 9]));
    assert!(!slow_tmp.exists());
}

// ── A seal's leftovers ───────────────────────────────────────────

/// A sink's writer over `t`, its id and in-flight `.tmp`s known to the test.
fn writer_over(t: &TempLog, grace: Duration) -> ParquetWriter<TestTable> {
    writer_in(t.dir.path().to_path_buf(), grace)
}

/// A sink's writer rooted at `root`, never merging.
fn writer_in(root: PathBuf, grace: Duration) -> ParquetWriter<TestTable> {
    ParquetWriter {
        root,
        grace,
        compact: None,
        clock: Arc::new(SystemTime::now),
        swept: Mutex::new(None),
        uploader: None,
        id: WriterId::new(),
        in_flight: InFlight::default(),
        seq: AtomicU64::new(0),
        table: PhantomData,
    }
}

#[tokio::test]
async fn a_writers_failed_seal_leaves_no_tmp_behind_its_next_upkeep() {
    let t = TempLog::new();
    let writer = writer_over(&t, HOUR);
    let tmp = t.part().tmp_path(&writer.id.seal_tmp(0));

    let failed = writer
        .publish_by(row_groups(test_rows(&[1])), |path, groups| {
            file::write::<TestTable>(path, groups)?;
            Err(LogError::Io {
                path: path.to_path_buf(),
                source: io::Error::other("fsync failed"),
            })
        })
        .await;
    assert!(matches!(failed, Err(LogError::Io { .. })), "{failed:?}");
    assert!(tmp.exists(), "the failed seal wrote its .tmp");

    // The same writer's next upkeep, well inside grace.
    let report = writer.maintain(vec![t.part_dir()], SystemTime::now()).await;

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!tmp.exists(), "the failed seal's .tmp is left behind");
    assert!(t.rows().await.is_empty());
}

#[tokio::test]
async fn a_seal_whose_task_panics_says_it_was_sealing_and_which_table() {
    let t = TempLog::new();
    let writer = writer_over(&t, HOUR);

    let failed = writer
        .publish_by(row_groups(test_rows(&[1])), |_, _| {
            panic!("the disk went away")
        })
        .await
        .unwrap_err();

    assert!(
        matches!(&failed, LogError::IoTask { work: IoWork::Seal(table), .. } if *table == TestTable::NAME),
        "{failed:?}"
    );
    assert_eq!(
        failed.to_string(),
        "sealing a batch of `contract_test`: the file IO task failed"
    );
}

/// The writer's own upkeep runs between the seal writing its `.tmp` and
/// publishing it, with a grace of zero and a clock an hour on: only the
/// claim keeps the `.tmp`.
#[tokio::test]
async fn a_seal_in_flight_survives_its_writers_upkeep_at_grace_zero() {
    let t = TempLog::new();
    let writer = writer_over(&t, Duration::ZERO);
    let dir = t.part_dir();
    let round = writer.round(SystemTime::now() + HOUR);

    let published = writer
        .publish_by(row_groups(test_rows(&[1])), move |path, groups| {
            file::write::<TestTable>(path, groups)?;
            dir.maintain::<TestTable>(&round, round.age(&dir))
        })
        .await;

    assert!(
        published.is_ok(),
        "the seal's .tmp was swept: {published:?}"
    );
    assert_eq!(t.rows().await, test_rows(&[1]));
}

// ── A writer killed mid-seal ──────────────────────────────

/// Where [`killed_writer_child`] writes; set only on the child
/// [`a_killed_writer_loses_only_its_unsealed_batch_and_its_tmp_goes_after_grace`]
/// spawns.
const KILLED_DIR_VAR: &str = "TARS_MELT_KILLED_WRITER_DIR";
const SEALED_LINE: &str = "sealed=";
const HELD_LINE: &str = "held=";
/// The child's seal policy cuts a batch every this many rows: with
/// `max_age` an hour, the most a crash may lose is the batch being sealed.
const ROWS_PER_SEAL: u64 = 3;
/// Seals the child sees succeed before the one it is killed in.
const SEALS_BEFORE_KILL: u64 = 3;
const WRITTEN: u64 = (SEALS_BEFORE_KILL + 1) * ROWS_PER_SEAL;

/// The child's writer: a real [`ParquetWriter`] under a real sink, which
/// reports each seal it sees succeed — except seal `SEALS_BEFORE_KILL`,
/// held once its `.tmp` is written until the parent kills the process.
struct HoldsASeal {
    inner: ParquetWriter<TestTable>,
    seals: AtomicU64,
}

#[async_trait]
impl SealWriter<TestTable> for HoldsASeal {
    type Due = DirtyPartitions;
    type Deferred = ExpiringDirs;

    async fn upkeep_on_open(&self) -> UpkeepReport<ExpiringDirs> {
        self.inner.upkeep_on_open().await
    }

    fn wait(&self, expiring: &ExpiringDirs) -> Duration {
        self.inner.wait(expiring)
    }

    #[expect(
        clippy::print_stdout,
        reason = "stdout is how this child tells its parent which seals succeeded before the kill"
    )]
    async fn seal(&self, groups: Vec<RowGroup<TestTable>>) -> Result<DirtyPartitions, LogError> {
        if self.seals.fetch_add(1, Ordering::SeqCst) == SEALS_BEFORE_KILL {
            let now = (self.inner.clock)();
            let partitions = self.inner.publish_by(groups, hold_unpublished).await?;
            return Ok(DirtyPartitions { partitions, now });
        }
        let keys = keys_of(&groups);
        let due = self.inner.seal(groups).await?;
        println!("{SEALED_LINE}{keys}");
        Ok(due)
    }

    async fn upkeep(&self, due: DirtyPartitions) -> UpkeepReport<ExpiringDirs> {
        self.inner.upkeep(due).await
    }

    async fn upkeep_deferred(&self, due: ExpiringDirs) -> UpkeepReport<ExpiringDirs> {
        self.inner.upkeep_deferred(due).await
    }
}

fn keys_of(groups: &[RowGroup<TestTable>]) -> String {
    let keys: Vec<String> = groups
        .iter()
        .flat_map(|g| g.rows().iter().map(|r| r.key().to_string()))
        .collect();
    keys.join(",")
}

/// Write the `.tmp`, say where, and wait: the parent kills this process
/// here. Should the parent go away instead, stdin closes and the seal fails
/// unpublished.
#[expect(
    clippy::print_stdout,
    reason = "stdout is how this child tells its parent which .tmp it holds, so the parent kills it there"
)]
fn hold_unpublished(path: &Path, groups: &[RowGroup<TestTable>]) -> Result<(), LogError> {
    file::write::<TestTable>(path, groups)?;
    println!("{HELD_LINE}{}", path.display());
    let mut never = String::new();
    std::io::stdin()
        .read_line(&mut never)
        .map_err(io_at(path))?;
    Err(LogError::Io {
        path: path.to_path_buf(),
        source: io::Error::other("the parent never killed the held seal"),
    })
}

/// The killed writer: appends `WRITTEN` rows through a sink whose seal
/// `SEALS_BEFORE_KILL` is held.
#[tokio::test]
#[ignore = "a child of a_killed_writer_loses_only_its_unsealed_batch_and_its_tmp_goes_after_grace"]
async fn killed_writer_child() {
    let Some(root) = std::env::var_os(KILLED_DIR_VAR) else {
        return;
    };
    let writer = HoldsASeal {
        inner: writer_in(PathBuf::from(root), HOUR),
        seals: AtomicU64::new(0),
    };
    let sink = BufferedSink::spawn(
        writer,
        Arc::default(),
        config(ROWS_PER_SEAL as usize, HOUR),
        &Handle::current(),
    );
    for key in 0..WRITTEN {
        sink.append(TestRow::new(key, "x")).await.unwrap();
    }
    sink.close().await.unwrap();
}

/// What the child said before it was killed: the keys of every seal it saw
/// succeed, and the `.tmp` of the seal it was killed in.
struct Held {
    sealed: Vec<u64>,
    tmp: PathBuf,
}

fn read_until_held(stdout: &mut impl BufRead) -> Held {
    let mut sealed = Vec::new();
    let mut said = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "the child exited before holding a seal: {said:?}"
        );
        let line = line.trim_end();
        if let Some(tmp) = line.strip_prefix(HELD_LINE) {
            return Held {
                sealed,
                tmp: PathBuf::from(tmp),
            };
        }
        if let Some(keys) = line.strip_prefix(SEALED_LINE) {
            sealed.extend(keys.split(',').map(|k| k.parse::<u64>().unwrap()));
        }
        said.push(line.to_owned());
    }
}

/// One sweep by a writer whose clock reads `at`: a new sink sweeps the past
/// days as it opens, and the test table's one partition is on a past day.
async fn sweep_at(t: &TempLog, at: SystemTime) {
    let sink = t.log().sink_on(
        config(1, HOUR),
        None,
        Arc::new(move || at),
        &Handle::current(),
    );
    sink.close().await.unwrap();
    let health = sink.health();
    assert!(health.upkeep_failed.is_none(), "{:?}", health.upkeep_failed);
}

fn file_names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// A writer process killed between writing a seal's `.tmp` and
/// publishing it. The kill lands at a point the child reports, not after a
/// sleep.
#[tokio::test]
async fn a_killed_writer_loses_only_its_unsealed_batch_and_its_tmp_goes_after_grace() {
    let t = TempLog::new();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "event::log::parquet::tests::killed_writer_child",
            "--ignored",
            "--nocapture",
        ])
        .env(KILLED_DIR_VAR, t.dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let held = read_until_held(&mut BufReader::new(child.stdout.take().unwrap()));
    child.kill().unwrap();
    child.wait().unwrap();

    let written: Vec<u64> = (0..WRITTEN).collect();
    let (committed, in_flight) = written.split_at((SEALS_BEFORE_KILL * ROWS_PER_SEAL) as usize);
    assert_eq!(
        held.sealed, committed,
        "every seal before the held one succeeded"
    );
    let rows = t.rows().await;
    assert_eq!(
        rows,
        test_rows(&held.sealed),
        "a reader sees exactly the sealed rows"
    );
    assert!(
        written.len() - rows.len() <= ROWS_PER_SEAL as usize,
        "the crash lost more than the batch being sealed: {rows:?}"
    );

    assert_eq!(
        file::read::<TestTable>(&held.tmp, &Scan::All).unwrap(),
        test_rows(in_flight),
        "the held seal's .tmp is on disk, holding the batch in flight"
    );
    let tmp_name = file_names(std::slice::from_ref(&held.tmp));
    let listed = file_names(&t.part_dir().live_files().unwrap());
    assert!(!listed.contains(&tmp_name[0]), "{listed:?}");

    let written_at = std::fs::metadata(&held.tmp).unwrap().modified().unwrap();
    sweep_at(&t, written_at + HOUR - Duration::from_secs(1)).await;
    assert!(
        held.tmp.exists(),
        "a .tmp younger than grace may be a live writer's"
    );
    sweep_at(&t, written_at + HOUR).await;
    assert!(
        !held.tmp.exists(),
        "the killed writer's .tmp outlived grace"
    );
    assert_eq!(t.rows().await, test_rows(committed));
}

#[test]
fn a_file_whose_rows_do_not_decode_is_named_by_read_and_by_merge() {
    use ::parquet::arrow::ArrowWriter;

    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: ArrayRef| {
        let schema = Arc::new(Schema::new(vec![
            Field::new("key", DataType::UInt64, false),
            Field::new("body", body.data_type().clone(), false),
        ]));
        let keys: ArrayRef = Arc::new(UInt64Array::from_iter_values([1]));
        let batch = RecordBatch::try_new(schema.clone(), vec![keys, body]).unwrap();
        let path = dir.path().join(name);
        let mut w =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        path
    };
    let good = write("good.parquet", Arc::new(StringArray::from(vec!["x"])));
    let bad = write("bad.parquet", Arc::new(UInt64Array::from_iter_values([7])));

    assert_eq!(
        file::read::<TestTable>(&good, &file::Scan::All)
            .unwrap()
            .len(),
        1
    );
    let read = file::read::<TestTable>(&bad, &file::Scan::All).unwrap_err();
    assert!(
        matches!(&read, LogError::Decode { path, .. } if *path == bad),
        "{read}"
    );
    let merged = file::merge::<TestTable>(&[good, bad.clone()], &dir.path().join("out.parquet"))
        .unwrap_err();
    assert!(
        matches!(&merged, LogError::Decode { path, .. } if *path == bad),
        "{merged}"
    );
}

#[test]
fn a_file_whose_footer_is_damaged_is_named_by_every_read() {
    use crate::event::call_log::LlmCallTable;

    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.parquet");
    file::write::<TestTable>(&good, &row_groups(test_rows(&[1, 2]))).unwrap();
    let bad = dir.path().join("bad.parquet");
    let mut bytes = std::fs::read(&good).unwrap();
    // The four bytes before the trailing magic state the footer's length.
    let at = bytes.len() - 8;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&bad, bytes).unwrap();

    let names_bad = |e: &LogError| matches!(e, LogError::Open { path, .. } if *path == bad);
    let read = file::read::<TestTable>(&bad, &file::Scan::All).unwrap_err();
    assert!(names_bad(&read), "{read}");
    let summary = file::read_partial::<LlmCallTable>(&bad).unwrap_err();
    assert!(names_bad(&summary), "{summary}");
    let counted = file::row_count(&bad).unwrap_err();
    assert!(names_bad(&counted), "{counted}");
    let merged = file::merge::<TestTable>(&[good, bad.clone()], &dir.path().join("out.parquet"))
        .unwrap_err();
    assert!(names_bad(&merged), "{merged}");
}

#[test]
fn the_next_midnight_is_the_start_of_the_following_utc_day() {
    let at = |ms: u64| UNIX_EPOCH + Duration::from_millis(ms);

    assert_eq!(next_midnight(at(DAY)), Some(at(20_001 * DAY_MS)));
    assert_eq!(
        next_midnight(at(20_001 * DAY_MS - 1)),
        Some(at(20_001 * DAY_MS))
    );
    assert_eq!(
        next_midnight(at(20_001 * DAY_MS)),
        Some(at(20_002 * DAY_MS)),
        "midnight itself starts a day"
    );
    assert_eq!(next_midnight(UNIX_EPOCH - Duration::from_secs(1)), None);
}

#[test]
fn a_clock_with_no_utc_day_is_reported_by_the_round_not_dropped() {
    let before = UNIX_EPOCH - Duration::from_secs(1);
    let report = midnight(before);
    assert!(report.deferred.is_none());
    assert!(
        matches!(
            report.failures.as_slice(),
            [LogError::ClockOutOfRange { reading }] if *reading == before
        ),
        "{:?}",
        report.failures
    );

    let report = midnight(UNIX_EPOCH);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(report.deferred.is_some());
}

#[test]
fn tenants_differing_only_in_case_get_directories_that_differ_ignoring_case() {
    let folded = |tenant: &str| {
        let tenant = tenant.parse::<TenantId>().unwrap();
        table_dir(Path::new("root"), "t", &tenant)
            .to_str()
            .unwrap()
            .to_ascii_lowercase()
    };

    assert_ne!(folded("Acme"), folded("acme"));
    assert_ne!(folded("J"), folded("j"));
    assert_eq!(
        folded("acme-1_b"),
        "root/tenant=acme-1_b/t",
        "lower case stays readable"
    );
}

/// Call-log files whose rows sit in row groups of 16, one row holding what `damage` puts there.
mod call_log_files {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use ::parquet::arrow::ArrowWriter;
    use ::parquet::file::properties::WriterProperties;
    use arrow_array::builder::{ListBuilder, StringBuilder};
    use arrow_array::{ArrayRef, RecordBatch};
    use arrow_schema::{Field, Schema};

    use super::super::file;
    use crate::event::call_log::tests::row;
    use crate::event::call_log::{CallOutcome, LlmCallTable};
    use crate::event::log::{LogError, ParquetTable};

    const T0: i64 = 1_790_553_600_000;
    const GROUP_ROWS: usize = 16;

    /// Writes `rows` call rows to `name`, with row `at` of the text list column `column` holding
    /// the one item `item`.
    fn written(
        dir: &Path,
        name: &str,
        rows: usize,
        column: &str,
        at: usize,
        item: Option<&str>,
    ) -> PathBuf {
        let calls: Vec<_> = (0..rows)
            .map(|i| row("t", T0 + i as i64, CallOutcome::Completed))
            .collect();
        let batch = LlmCallTable::encode(&calls).unwrap();
        let mut list = ListBuilder::new(StringBuilder::new());
        for i in 0..rows {
            if i == at {
                list.values().append_option(item);
            }
            list.append(true);
        }
        let list: ArrayRef = Arc::new(list.finish());

        let schema = batch.schema();
        let index = schema.index_of(column).unwrap();
        let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
        let mut columns = batch.columns().to_vec();
        fields[index] = Field::new(column, list.data_type().clone(), true);
        columns[index] = list;
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap();

        let path = dir.join(name);
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(GROUP_ROWS))
            .build();
        let mut w = ArrowWriter::try_new(
            std::fs::File::create(&path).unwrap(),
            batch.schema(),
            Some(props),
        )
        .unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        path
    }

    #[test]
    fn a_summary_read_names_the_file_and_row_a_row_does_not_decode_in() {
        let dir = tempfile::tempdir().unwrap();
        let holed = written(dir.path(), "holed.parquet", 20, "tags", 18, None);

        let full = file::read::<LlmCallTable>(&holed, &file::Scan::All).unwrap_err();
        assert!(
            matches!(&full, LogError::DecodeRow { path, row: 18, .. } if *path == holed),
            "{full}"
        );
        let summary = file::read_partial::<LlmCallTable>(&holed).unwrap_err();
        assert!(
            matches!(&summary, LogError::DecodeRow { path, row: 18, .. } if *path == holed),
            "{summary}"
        );
    }

    #[test]
    fn a_row_that_does_not_decode_is_named_by_its_row_in_the_file_on_either_side_of_a_group_boundary()
     {
        let dir = tempfile::tempdir().unwrap();
        for at in [GROUP_ROWS - 1, GROUP_ROWS] {
            let path = written(
                dir.path(),
                &format!("at-{at}.parquet"),
                20,
                "degraded",
                at,
                Some("tool_choice"),
            );

            let merged = file::merge::<LlmCallTable>(
                std::slice::from_ref(&path),
                &dir.path().join(format!("out-{at}.parquet")),
            )
            .unwrap_err();
            assert!(
                matches!(&merged, LogError::DecodeRow { row, .. } if *row == at as u64),
                "{merged}"
            );
            let opened = std::fs::File::open(&path).unwrap();
            let one_group =
                file::read_row_group::<LlmCallTable>(&path, &opened, (at / GROUP_ROWS) as u32)
                    .unwrap_err();
            assert!(
                matches!(&one_group, LogError::DecodeRow { row, .. } if *row == at as u64),
                "{one_group}"
            );
        }
    }
}

mod ship;
