//! The port contract every log adapter passes. An adapter implements
//! [`Harness`] and invokes `contract_suite!(<fresh harness expr>)` in its
//! test module; each case runs on tokio's paused clock, so `max_age` is
//! exercised in virtual time.

use std::num::NonZeroUsize;
use std::time::Duration;

use async_trait::async_trait;

use super::sink::SinkConfig;
use super::{
    AppendError, LogError, LogSink, LogSource, LogTable, Loss, Partition, Partitioned, SealPolicy,
    TableName, TenantScope, TimeWindow,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TestRow {
    key: u64,
    body: String,
}

impl TestRow {
    pub(crate) fn new(key: u64, body: &str) -> Self {
        Self {
            key,
            body: body.to_owned(),
        }
    }

    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

pub(crate) struct TestTable;

impl LogTable for TestTable {
    type Row = TestRow;
    type Key = u64;

    const NAME: TableName = TableName::new("contract_test");
    const ROW_GROUP_ROWS: NonZeroUsize = NonZeroUsize::new(2).unwrap();

    fn sort_key(row: &TestRow) -> u64 {
        row.key
    }

    fn approx_bytes(row: &TestRow) -> usize {
        row.body.len()
    }
}

/// The one tenant every test row belongs to.
pub(crate) const TENANT: &str = "contract";

impl Partitioned for TestTable {
    /// Every test row in one partition: the contract is about sealing, not layout.
    fn partition(_: &TestRow) -> Partition {
        Partition {
            tenant: tars_types::TenantId::new(TENANT),
            day: tars_types::CallId::new_v7(0).unwrap().day(),
        }
    }
}

/// Returns the scope of every day of [`TENANT`]: all the rows a test table holds.
pub(crate) fn scope() -> TenantScope {
    TenantScope::new(tars_types::TenantId::new(TENANT), TimeWindow::all())
}

/// An adapter under test: one store, from which sinks and fresh readers open.
#[async_trait]
pub(crate) trait Harness: Send + Sync {
    async fn sink(&self, config: SinkConfig) -> Result<Box<dyn LogSink<TestTable>>, LogError>;
    /// Opens a reader now, sharing nothing with a sink but the store.
    async fn source(&self) -> Result<Box<dyn LogSource<TestTable>>, LogError>;
}

/// Expands to one `#[tokio::test]` per contract case over `$harness`.
macro_rules! contract_suite {
    ($harness:expr) => {
        $crate::event::log::contract::contract_suite!(@cases $harness;
            seals_by_max_rows,
            seals_by_max_bytes,
            seals_by_max_age,
            close_seals_the_remainder,
            point_lookup_by_sort_key,
            full_buffer_drops_are_counted_and_fail_close,
        );
    };
    (@cases $harness:expr; $($case:ident,)+) => {
        $(
            #[tokio::test(start_paused = true)]
            async fn $case() {
                $crate::event::log::contract::$case(&$harness).await;
            }
        )+
    };
}
pub(crate) use contract_suite;

const EVER: Duration = Duration::from_secs(3600);

fn config(max_rows: usize, max_bytes: usize, max_age: Duration, capacity: usize) -> SinkConfig {
    SinkConfig {
        seal: SealPolicy {
            max_rows,
            max_bytes,
            max_age,
            grace: EVER,
        },
        capacity: NonZeroUsize::new(capacity).unwrap(),
    }
}

async fn sealed(h: &impl Harness) -> Vec<TestRow> {
    let mut rows = h.source().await.unwrap().scan(&scope()).await.unwrap();
    rows.sort_by_key(TestRow::key);
    rows
}

/// Polls in 1 ms virtual steps until `n` rows are sealed; `max_age` in these cases is far beyond
/// the 100 ms polled, so it cannot be what sealed them.
async fn await_sealed(h: &impl Harness, n: usize) -> Vec<TestRow> {
    for _ in 0..100 {
        let rows = sealed(h).await;
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("expected {n} sealed rows, have {:?}", sealed(h).await);
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

fn rows(keys: &[u64]) -> Vec<TestRow> {
    keys.iter().map(|&k| TestRow::new(k, "x")).collect()
}

async fn append_all(sink: &dyn LogSink<TestTable>, rows: Vec<TestRow>) {
    for row in rows {
        sink.append(row).await.unwrap();
    }
}

pub(crate) async fn seals_by_max_rows(h: &impl Harness) {
    let sink = h.sink(config(2, usize::MAX, EVER, 8)).await.unwrap();

    append_all(sink.as_ref(), rows(&[2, 1, 3])).await;

    assert_eq!(await_sealed(h, 2).await, rows(&[1, 2]));
    settle().await;
    assert_eq!(
        sealed(h).await.len(),
        2,
        "third row waits for the next seal"
    );
    sink.close().await.unwrap();
    assert_eq!(sealed(h).await, rows(&[1, 2, 3]));
}

pub(crate) async fn seals_by_max_bytes(h: &impl Harness) {
    let sink = h.sink(config(usize::MAX, 10, EVER, 8)).await.unwrap();

    // 4 + 4 fit; the third 4-byte row would pass 10, so the first two seal.
    for (k, body) in [(1, "aaaa"), (2, "bbbb"), (3, "cccc")] {
        sink.append(TestRow::new(k, body)).await.unwrap();
    }
    assert_eq!(await_sealed(h, 2).await.len(), 2);
    settle().await;
    assert_eq!(sealed(h).await.len(), 2);

    // An oversized row seals the pending one, then seals alone.
    sink.append(TestRow::new(4, &"z".repeat(20))).await.unwrap();
    assert_eq!(await_sealed(h, 4).await.len(), 4);
    sink.close().await.unwrap();
}

pub(crate) async fn seals_by_max_age(h: &impl Harness) {
    let max_age = Duration::from_secs(1);
    let sink = h
        .sink(config(usize::MAX, usize::MAX, max_age, 8))
        .await
        .unwrap();

    sink.append(TestRow::new(1, "x")).await.unwrap();
    tokio::time::sleep(max_age - Duration::from_millis(1)).await;
    assert!(sealed(h).await.is_empty(), "sealed before max_age");

    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(await_sealed(h, 1).await, rows(&[1]));
    sink.close().await.unwrap();
}

pub(crate) async fn close_seals_the_remainder(h: &impl Harness) {
    let sink = h
        .sink(config(usize::MAX, usize::MAX, EVER, 8))
        .await
        .unwrap();

    append_all(sink.as_ref(), rows(&[3, 1, 2])).await;
    settle().await;
    assert!(sealed(h).await.is_empty(), "visible before any seal");

    sink.close().await.unwrap();
    assert_eq!(sealed(h).await, rows(&[1, 2, 3]));
    assert!(matches!(
        sink.append(TestRow::new(9, "x")).await,
        Err(AppendError::Closed)
    ));
    assert!(matches!(
        sink.try_append(TestRow::new(9, "x")),
        Err(AppendError::Closed)
    ));
    assert!(sink.health().dropped.is_none());
}

pub(crate) async fn point_lookup_by_sort_key(h: &impl Harness) {
    // Three rows per seal across several row groups of two.
    let sink = h.sink(config(3, usize::MAX, EVER, 16)).await.unwrap();
    let mut written = rows(&[7, 3, 1, 5, 3, 6, 2, 4]);
    written.push(TestRow::new(3, "second"));
    append_all(sink.as_ref(), written).await;
    sink.close().await.unwrap();

    let src = h.source().await.unwrap();
    let mut threes = src.scan_key(&scope(), &3).await.unwrap();
    threes.sort_by(|a, b| a.body().cmp(b.body()));
    assert_eq!(
        threes,
        vec![
            TestRow::new(3, "second"),
            TestRow::new(3, "x"),
            TestRow::new(3, "x")
        ]
    );
    assert_eq!(src.scan_key(&scope(), &5).await.unwrap(), rows(&[5]));
    assert!(src.scan_key(&scope(), &8).await.unwrap().is_empty());
}

pub(crate) async fn full_buffer_drops_are_counted_and_fail_close(h: &impl Harness) {
    let sink = h
        .sink(config(usize::MAX, usize::MAX, EVER, 1))
        .await
        .unwrap();

    // No await between the two: the sealer cannot drain the one slot.
    sink.try_append(TestRow::new(1, "x")).unwrap();
    let refused = sink.try_append(TestRow::new(2, "x"));
    assert!(
        matches!(refused, Err(AppendError::Full { .. })),
        "{refused:?}"
    );
    assert!(matches!(sink.health().dropped, Some(Loss { rows: 1, .. })));

    let closed = sink.close().await;
    assert!(
        matches!(&closed, Err(LogError::Dropped(Loss { rows: 1, last })) if matches!(**last, LogError::Append(AppendError::Full { .. }))),
        "{closed:?}"
    );
    assert_eq!(sealed(h).await, rows(&[1]), "accepted row still sealed");
}
