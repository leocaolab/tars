use std::num::NonZeroUsize;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use parquet::file::statistics::Statistics;
use tars_types::{Millis, UtcDay};
use tempfile::TempDir;
use tokio::runtime::Handle;

use super::super::super::contract::{TestRow, TestTable};
use super::super::super::{
    LogSink, LogSource, LogTable, MemLog, PartialSource, Partition, Projected, SealPolicy,
    SinkConfig, TableName, TimeWindow,
};
use super::super::{
    CompactRule, DecodeError, EncodeError, OffsetLimit, ParquetLog, ParquetProjected, partition_dir,
};
use super::*;

/// Test rows keyed on their time (epoch ms) and carrying their tenant as
/// the body, so one sink writes into several tenants and days.
struct TenantTable;

impl LogTable for TenantTable {
    type Row = TestRow;
    type Key = u64;

    const NAME: TableName = TableName::new("tenant_test");
    const ROW_GROUP_ROWS: NonZeroUsize = TestTable::ROW_GROUP_ROWS;

    fn sort_key(row: &TestRow) -> u64 {
        row.key()
    }

    fn approx_bytes(row: &TestRow) -> usize {
        TestTable::approx_bytes(row)
    }
}

impl Partitioned for TenantTable {
    fn partition(row: &TestRow) -> Partition {
        Partition {
            tenant: row.body().parse::<TenantId>().unwrap(),
            day: UtcDay::of(at(row.key())).unwrap(),
        }
    }
}

impl ParquetTable for TenantTable {
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

/// A test row without its body: its key, and the tenant its body names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Keyed {
    key: u64,
    tenant: TenantId,
}

/// Test filter: every row, or only odd keys.
#[derive(Debug, Clone, Copy)]
enum Keys {
    All,
    Odd,
}

impl Projected for TenantTable {
    type Partial = Keyed;
    type Filter = Keys;

    fn partial(row: &TestRow) -> Keyed {
        Keyed {
            key: row.key(),
            tenant: row.body().parse::<TenantId>().unwrap(),
        }
    }

    fn partial_key(k: &Keyed) -> u64 {
        k.key
    }

    fn partial_tenant(k: &Keyed) -> &TenantId {
        &k.tenant
    }

    fn keeps(filter: &Keys, k: &Keyed) -> bool {
        match filter {
            Keys::All => true,
            Keys::Odd => k.key % 2 == 1,
        }
    }
}

impl ParquetProjected for TenantTable {
    const PARTIAL_COLUMNS: &'static [&'static str] = &["key", "body"];

    fn decode_partial(batch: &RecordBatch) -> Result<Vec<Keyed>, DecodeError> {
        Ok(TestTable::decode(batch)?
            .iter()
            .map(TenantTable::partial)
            .collect())
    }
}

fn keyed(key: u64, tenant: &str) -> Keyed {
    TenantTable::partial(&TestRow::new(key, tenant))
}

const DAY_MS: u64 = 86_400_000;
/// Noon of an arbitrary day, and noon of the day after.
const D1: u64 = 20_000 * DAY_MS + DAY_MS / 2;
const D2: u64 = D1 + DAY_MS;
const HOUR: Duration = Duration::from_secs(3600);

fn at(key: u64) -> Millis {
    Millis::try_from(key).unwrap()
}

fn config(max_rows: usize) -> SinkConfig {
    SinkConfig {
        seal: SealPolicy {
            max_rows,
            max_bytes: usize::MAX,
            max_age: HOUR,
            grace: HOUR,
        },
        capacity: NonZeroUsize::new(64).unwrap(),
    }
}

fn scope(tenant: &str, window: TimeWindow) -> TenantScope {
    TenantScope::new(tenant.parse::<TenantId>().unwrap(), window)
}

async fn append(sink: &dyn LogSink<TenantTable>, rows: &[TestRow]) {
    for row in rows {
        sink.append(row.clone()).await.unwrap();
    }
    sink.close().await.unwrap();
}

fn sorted(mut rows: Vec<TestRow>) -> Vec<TestRow> {
    rows.sort_by_key(TestRow::key);
    rows
}

struct Store {
    dir: TempDir,
}

impl Store {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn log(&self) -> ParquetLog<TenantTable> {
        ParquetLog::open(self.dir.path())
    }

    async fn write(&self, max_rows: usize, compact: Option<CompactRule>, rows: &[TestRow]) {
        let sink = self
            .log()
            .sink(config(max_rows), compact, &Handle::current());
        append(&sink, rows).await;
    }

    fn part(&self, tenant: &str, day_ms: u64) -> PathBuf {
        self.part_dir(tenant, day_ms).path().to_path_buf()
    }

    fn part_dir(&self, tenant: &str, day_ms: u64) -> PartitionDir {
        let p = TenantTable::partition(&TestRow::new(day_ms, tenant));
        partition_dir(self.dir.path(), TenantTable::NAME.as_str(), &p)
    }
}

/// Every file in `dir` — data, manifest, lock — overwritten with bytes no
/// reader can parse: reading anything there fails.
fn poison(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        std::fs::write(entry.unwrap().path(), b"not parquet, not json").unwrap();
    }
}

fn rows_of_tenants_and_days() -> Vec<TestRow> {
    vec![
        TestRow::new(D1, "a"),
        TestRow::new(D1 + 1, "a"),
        TestRow::new(D2, "a"),
        TestRow::new(D2 + 1, "a"),
        TestRow::new(D1 + 2, "b"),
        TestRow::new(D2 + 2, "b"),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_read_opens_only_the_tenants_covered_days_and_returns_the_rows_written_there() {
    let s = Store::new();
    let rows = rows_of_tenants_and_days();
    s.write(64, None, &rows).await;
    let a_since_d2 = scope("a", TimeWindow::since(at(D2)).unwrap());
    let written_there: Vec<TestRow> = sorted(
        rows.iter()
            .filter(|r| r.body() == "a" && r.key() >= D2)
            .cloned()
            .collect(),
    );

    for dir in [s.part("a", D1), s.part("b", D1), s.part("b", D2)] {
        poison(&dir);
    }

    let scanned = sorted(s.log().scan(&a_since_d2).await.unwrap());
    let looked_up = s.log().scan_key(&a_since_d2, &(D2 + 1)).await.unwrap();
    assert_eq!(
        written_there,
        [TestRow::new(D2, "a"), TestRow::new(D2 + 1, "a")]
    );
    assert_eq!(scanned, written_there, "no poisoned partition was opened");
    assert_eq!(looked_up, [TestRow::new(D2 + 1, "a")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_scans_the_same_rows_parquet_does() {
    let s = Store::new();
    let rows = rows_of_tenants_and_days();
    s.write(64, None, &rows).await;
    let mem = MemLog::<TenantTable>::new();
    append(&mem.sink(config(64), &Handle::current()), &rows).await;

    for window in [
        TimeWindow::all(),
        TimeWindow::since(at(D2)).unwrap(),
        TimeWindow::day(UtcDay::of(at(D1)).unwrap()),
    ] {
        let scope = scope("a", window);
        assert_eq!(
            sorted(mem.scan(&scope).await.unwrap()),
            sorted(s.log().scan(&scope).await.unwrap()),
            "{window:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_the_newest_day_meets_never_opens_an_older_day() {
    let s = Store::new();
    s.write(64, None, &rows_of_tenants_and_days()).await;
    poison(&s.part("a", D1));

    let newest = s
        .log()
        .scan_partial(&scope("a", TimeWindow::all()), &Keys::All, Some(2))
        .await
        .unwrap();

    assert_eq!(newest, [keyed(D2 + 1, "a"), keyed(D2, "a")]);
}

/// Tenant `a`: odd and even keys on D2, one odd key on D1.
fn odd_and_even_rows() -> Vec<TestRow> {
    vec![
        TestRow::new(D1 + 1, "a"),
        TestRow::new(D2, "a"),
        TestRow::new(D2 + 1, "a"),
        TestRow::new(D2 + 2, "a"),
        TestRow::new(D2 + 3, "a"),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filtered_limit_the_newest_days_kept_rows_meet_never_opens_an_older_day() {
    let s = Store::new();
    s.write(64, None, &odd_and_even_rows()).await;
    poison(&s.part("a", D1));
    let a = scope("a", TimeWindow::all());

    let summaries = s.log().scan_partial(&a, &Keys::Odd, Some(2)).await.unwrap();
    let rows = s.log().scan_newest(&a, &Keys::Odd, Some(2)).await.unwrap();

    assert_eq!(summaries, [keyed(D2 + 3, "a"), keyed(D2 + 1, "a")]);
    assert_eq!(rows, [TestRow::new(D2 + 3, "a"), TestRow::new(D2 + 1, "a")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_row_limit_the_newest_day_meets_never_opens_an_older_day() {
    let s = Store::new();
    s.write(64, None, &rows_of_tenants_and_days()).await;
    poison(&s.part("a", D1));

    let newest = s
        .log()
        .scan_newest(&scope("a", TimeWindow::all()), &Keys::All, Some(2))
        .await
        .unwrap();

    assert_eq!(newest, [TestRow::new(D2 + 1, "a"), TestRow::new(D2, "a")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limit_counts_only_kept_rows_so_a_short_day_reads_the_next_as_memory_does() {
    let s = Store::new();
    let rows = odd_and_even_rows();
    s.write(64, None, &rows).await;
    let mem = MemLog::<TenantTable>::new();
    append(&mem.sink(config(64), &Handle::current()), &rows).await;
    let a = scope("a", TimeWindow::all());

    for (filter, limit, want) in [
        (Keys::Odd, Some(3), vec![D2 + 3, D2 + 1, D1 + 1]),
        (Keys::Odd, None, vec![D2 + 3, D2 + 1, D1 + 1]),
        (Keys::All, Some(4), vec![D2 + 3, D2 + 2, D2 + 1, D2]),
        (Keys::Odd, Some(0), vec![]),
    ] {
        let want_rows: Vec<TestRow> = want.iter().map(|k| TestRow::new(*k, "a")).collect();
        let want_summaries: Vec<Keyed> = want.iter().map(|k| keyed(*k, "a")).collect();
        let case = format!("{filter:?} {limit:?}");

        assert_eq!(
            s.log().scan_partial(&a, &filter, limit).await.unwrap(),
            want_summaries,
            "{case}"
        );
        assert_eq!(
            mem.scan_partial(&a, &filter, limit).await.unwrap(),
            want_summaries,
            "{case}"
        );
        assert_eq!(
            s.log().scan_newest(&a, &filter, limit).await.unwrap(),
            want_rows,
            "{case}"
        );
        assert_eq!(
            mem.scan_newest(&a, &filter, limit).await.unwrap(),
            want_rows,
            "{case}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_row_the_filter_would_drop_still_fails_the_read() {
    let s = Store::new();
    s.write(64, None, &[TestRow::new(D1, "b")]).await;
    let (theirs, ours) = (s.part("b", D1), s.part("a", D1));
    std::fs::create_dir_all(&ours).unwrap();
    for entry in std::fs::read_dir(&theirs).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), ours.join(entry.file_name())).unwrap();
    }
    let a = scope("a", TimeWindow::all());

    let summarized = s
        .log()
        .scan_partial(&a, &Keys::Odd, None)
        .await
        .unwrap_err();
    let scanned = s.log().scan_newest(&a, &Keys::Odd, None).await.unwrap_err();

    for err in [summarized, scanned] {
        assert!(matches!(err, LogError::ForeignRow { .. }), "{err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_keeps_the_greatest_keys_across_days_as_memory_does() {
    let s = Store::new();
    let rows = rows_of_tenants_and_days();
    s.write(64, None, &rows).await;
    let mem = MemLog::<TenantTable>::new();
    append(&mem.sink(config(64), &Handle::current()), &rows).await;
    let a = scope("a", TimeWindow::all());

    for (limit, want) in [
        (Some(0), vec![]),
        (
            Some(3),
            vec![keyed(D2 + 1, "a"), keyed(D2, "a"), keyed(D1 + 1, "a")],
        ),
        (
            None,
            vec![
                keyed(D2 + 1, "a"),
                keyed(D2, "a"),
                keyed(D1 + 1, "a"),
                keyed(D1, "a"),
            ],
        ),
    ] {
        let parquet = s.log().scan_partial(&a, &Keys::All, limit).await.unwrap();
        let memory = mem.scan_partial(&a, &Keys::All, limit).await.unwrap();
        assert_eq!(parquet, want, "{limit:?}");
        assert_eq!(memory, want, "{limit:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_partitions_parquet_files_on_disk_are_its_live_files_while_merged_away_ones_wait_out_grace()
 {
    let s = Store::new();
    // One row per seal and a fan-in of 2 on a past day: seals merge, and
    // the merged-away files stay on disk for the hour of grace, under their
    // retired names.
    s.write(
        1,
        Some(CompactRule::new(2)),
        &[
            TestRow::new(D2, "a"),
            TestRow::new(D2 + 1, "a"),
            TestRow::new(D2 + 2, "a"),
            TestRow::new(D1, "a"),
            TestRow::new(D2, "b"),
        ],
    )
    .await;
    let d2 = s.part_dir("a", D2);
    let unpublished = super::super::partition::TmpFile::parse("unpublished.tmp").unwrap();
    std::fs::write(d2.tmp_path(&unpublished), b"half a file").unwrap();

    let entries: Vec<PathBuf> = std::fs::read_dir(s.part("a", D2))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let mut on_disk: Vec<PathBuf> = entries
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .cloned()
        .collect();
    on_disk.sort();
    let mut live = d2.live_files().unwrap();
    live.sort();

    assert_eq!(on_disk, live, "a glob of *.parquet finds the live files");
    assert!(
        entries
            .iter()
            .any(|p| p.extension().is_some_and(|x| x == "retired")),
        "merged-away files are still on disk: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_row_of_another_tenant_in_the_tenants_partition_fails_the_read() {
    let s = Store::new();
    s.write(64, None, &[TestRow::new(D1, "b")]).await;
    let (theirs, ours) = (s.part("b", D1), s.part("a", D1));
    std::fs::create_dir_all(&ours).unwrap();
    for entry in std::fs::read_dir(&theirs).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), ours.join(entry.file_name())).unwrap();
    }
    let a = scope("a", TimeWindow::all());

    let scanned = s.log().scan(&a).await.unwrap_err();
    let looked_up = s.log().scan_key(&a, &D1).await.unwrap_err();
    let summarized = s
        .log()
        .scan_partial(&a, &Keys::All, None)
        .await
        .unwrap_err();

    for err in [scanned, looked_up, summarized] {
        let LogError::ForeignRow {
            path,
            expected,
            found,
        } = &err
        else {
            panic!("expected ForeignRow, got {err}");
        };
        assert!(path.starts_with(&ours), "{err}");
        assert_eq!(expected.as_str(), "a");
        assert_eq!(found.as_str(), "b");
    }
}
