//! A writer given a bucket: past days frozen and uploaded, today left alone, failures retried, and
//! the bucket's glob reading every row once.

use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::oneshot;

use super::*;
use crate::event::log::parquet::compact::{Manifest, Upload};
use crate::event::log::parquet::ship::{Bucket, StoreId};

const PREFIX: &str = "prod";
/// Rows sealed on `DAY` before the next day's first seal.
const ON_DAY: u64 = 10;

/// A bucket two stores may share.
struct LocalBucket {
    store: Arc<dyn ObjectStore>,
}

impl LocalBucket {
    fn in_memory() -> Self {
        Self {
            store: Arc::new(InMemory::new()),
        }
    }

    /// The objects of the partition holding rows keyed on `day_ms`'s day, by name.
    async fn objects(&self, day_ms: u64) -> Vec<ObjectPath> {
        let listed = self
            .store
            .list_with_delimiter(Some(&object_dir(day_ms)))
            .await
            .unwrap();
        let mut names: Vec<ObjectPath> = listed.objects.into_iter().map(|o| o.location).collect();
        names.sort();
        names
    }

    async fn bytes(&self, object: &ObjectPath) -> Vec<u8> {
        let got = self.store.get(object).await.unwrap();
        got.bytes().await.unwrap().to_vec()
    }

    /// Reads every object of `day_ms`'s partition as Parquet, as a glob of the bucket would.
    async fn rows(&self, day_ms: u64) -> Vec<TestRow> {
        let scratch = tempfile::tempdir().unwrap();
        let mut rows = Vec::new();
        for (i, object) in self.objects(day_ms).await.iter().enumerate() {
            let path = scratch.path().join(format!("{i}.parquet"));
            std::fs::write(&path, self.bytes(object).await).unwrap();
            rows.extend(file::read::<DayTable>(&path, &Scan::All).unwrap());
        }
        rows.sort_by_key(TestRow::key);
        rows
    }
}

/// The bucket directory of the partition holding rows keyed on `day_ms`'s day: the local
/// partition path below the root, under `PREFIX`.
fn object_dir(day_ms: u64) -> ObjectPath {
    let p = DayTable::partition(&TestRow::new(day_ms, "x"));
    let rel = partition_dir(Path::new(""), DayTable::NAME.as_str(), &p);
    ObjectPath::parse(format!("{PREFIX}/{}", rel.path().to_str().unwrap())).unwrap()
}

impl DayLog {
    async fn target(&self, bucket: &LocalBucket) -> Bucket {
        let id = StoreId::read_or_create(self.dir.path().to_path_buf())
            .await
            .unwrap();
        Bucket::new(Arc::clone(&bucket.store), ObjectPath::from(PREFIX), id)
    }

    async fn shipping_sink(&self, bucket: &LocalBucket, clock: Clock) -> BufferedSink<DayTable> {
        self.log()
            .uploading_to(Some(self.target(bucket).await))
            .sink_on(
                config(1, HOUR),
                Some(CompactRule::new(16)),
                clock,
                &Handle::current(),
            )
    }

    fn manifest(&self, day_ms: u64) -> Manifest {
        let bytes = std::fs::read(self.part(day_ms).path().join("_live.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn ships(&self, day_ms: u64) -> Vec<Option<Upload>> {
        self.manifest(day_ms)
            .live
            .iter()
            .map(|f| f.upload)
            .collect()
    }
}

/// Waits for the sink to open, then seals `ON_DAY` rows on `DAY` and one past midnight, whose
/// upkeep sweeps `DAY`.
async fn fill_day_then_cross_midnight(
    sink: &BufferedSink<DayTable>,
    opened: oneshot::Receiver<()>,
    first_key: u64,
) {
    opened.await.unwrap();
    for key in (0..ON_DAY).map(|i| DAY + first_key + i) {
        sink.append(TestRow::new(key, "x")).await.unwrap();
    }
    sink.append(TestRow::new(NEXT_DAY + first_key, "x"))
        .await
        .unwrap();
}

/// A clock that opens the sink on `DAY`, seals `ON_DAY` rows on it, then reads `NEXT_DAY`; the
/// receiver hears once the sink has opened.
fn day_then_next_day() -> (Clock, oneshot::Receiver<()>) {
    let mut seals = vec![DAY; ON_DAY as usize];
    seals.push(NEXT_DAY);
    opens_on(DAY, &seals)
}

fn keys(from: u64, n: u64) -> Vec<u64> {
    (from..from + n).collect()
}

#[tokio::test]
async fn a_past_day_ships_as_one_object_once_the_next_day_begins() {
    let t = DayLog::new();
    let bucket = LocalBucket::in_memory();

    let (clock, opened) = day_then_next_day();
    let sink = t.shipping_sink(&bucket, clock).await;
    fill_day_then_cross_midnight(&sink, opened, 0).await;
    sink.close().await.unwrap();

    assert_eq!(
        bucket.objects(DAY).await.len(),
        1,
        "the merged day is one object"
    );
    assert_eq!(bucket.rows(DAY).await, test_rows(&keys(DAY, ON_DAY)));
    assert_eq!(t.ships(DAY), vec![Some(Upload::Done)]);
    assert!(sink.health().upkeep_failed.is_none());
}

#[tokio::test]
async fn today_is_neither_frozen_nor_shipped() {
    let t = DayLog::new();
    let bucket = LocalBucket::in_memory();

    let (clock, opened) = day_then_next_day();
    let sink = t.shipping_sink(&bucket, clock).await;
    fill_day_then_cross_midnight(&sink, opened, 0).await;
    sink.close().await.unwrap();

    assert_eq!(t.ships(NEXT_DAY), vec![None]);
    assert!(bucket.objects(NEXT_DAY).await.is_empty());
}

#[tokio::test]
async fn two_stores_ship_one_day_into_one_bucket_without_repeating_a_row() {
    let (a, b) = (DayLog::new(), DayLog::new());
    let bucket = LocalBucket::in_memory();

    let (clock_a, opened_a) = day_then_next_day();
    let (clock_b, opened_b) = day_then_next_day();
    let sink_a = a.shipping_sink(&bucket, clock_a).await;
    let sink_b = b.shipping_sink(&bucket, clock_b).await;
    tokio::join!(
        fill_day_then_cross_midnight(&sink_a, opened_a, 0),
        fill_day_then_cross_midnight(&sink_b, opened_b, 1_000)
    );
    sink_a.close().await.unwrap();
    sink_b.close().await.unwrap();

    assert_eq!(bucket.objects(DAY).await.len(), 2, "one object per store");
    let both: Vec<u64> = keys(DAY, ON_DAY)
        .into_iter()
        .chain(keys(DAY + 1_000, ON_DAY))
        .collect();
    assert_eq!(bucket.rows(DAY).await, test_rows(&both));
}

#[tokio::test]
async fn a_late_row_ships_as_a_second_object_and_the_first_is_untouched() {
    let t = DayLog::new();
    let bucket = LocalBucket::in_memory();
    let (clock, opened) = day_then_next_day();
    let sink = t.shipping_sink(&bucket, clock).await;
    fill_day_then_cross_midnight(&sink, opened, 0).await;
    sink.close().await.unwrap();
    let first = bucket.objects(DAY).await.remove(0);
    let first_bytes = bucket.bytes(&first).await;

    let late = t.shipping_sink(&bucket, scripted(&[NEXT_DAY])).await;
    late.append(TestRow::new(DAY + 500, "late")).await.unwrap();
    late.close().await.unwrap();

    let objects = bucket.objects(DAY).await;
    assert_eq!(objects.len(), 2, "{objects:?}");
    let local: Vec<TestRow> = t
        .rows()
        .await
        .into_iter()
        .filter(|r| r.key() < NEXT_DAY)
        .collect();
    assert_eq!(
        bucket.rows(DAY).await,
        local,
        "the bucket holds the day's rows once"
    );
    assert_eq!(bucket.bytes(&first).await, first_bytes);
    assert_eq!(t.ships(DAY), vec![Some(Upload::Done), Some(Upload::Done)]);
}

/// `DAY`'s three files, frozen as a writer with a bucket leaves them after merging; returns the one
/// file left.
fn frozen_day(t: &DayLog) -> String {
    let part = t.part(DAY);
    publish_three_on_day(&part);
    let past = upkeep(16, HOUR, SystemTime::now());
    part.maintain::<DayTable>(&past, DayAge::Past { freeze: true })
        .unwrap();
    let [file] = live_names(&part).try_into().unwrap();
    file
}

#[tokio::test]
async fn an_upload_already_in_the_bucket_counts_as_shipped() {
    let t = DayLog::new();
    let bucket = LocalBucket::in_memory();
    let file = frozen_day(&t);
    let target = t.target(&bucket).await;
    // What a writer leaves that died after its upload landed and before it marked the file.
    let id = std::fs::read_to_string(t.dir.path().join("_store_id")).unwrap();
    let object = object_dir(DAY).join(format!("{id}.{file}"));
    let bytes = std::fs::read(t.part(DAY).file_path(&DataFile::parse(&file).unwrap())).unwrap();
    bucket
        .store
        .put(&object, bytes.clone().into())
        .await
        .unwrap();
    assert_eq!(t.ships(DAY), vec![Some(Upload::Pending)]);

    let sink = t.log().uploading_to(Some(target)).sink_on(
        config(1, HOUR),
        Some(CompactRule::new(16)),
        scripted(&[NEXT_DAY]),
        &Handle::current(),
    );
    sink.close().await.unwrap();

    assert!(sink.health().upkeep_failed.is_none(), "{:?}", sink.health());
    assert_eq!(t.ships(DAY), vec![Some(Upload::Done)]);
    assert_eq!(bucket.objects(DAY).await, vec![object.clone()]);
    assert_eq!(bucket.bytes(&object).await, bytes);
}

/// A bucket whose first upload fails, reporting on `refused` that it did; every other call reaches
/// `inner`.
#[derive(Debug)]
struct RefusesFirstUpload {
    inner: InMemory,
    refused: Mutex<Option<oneshot::Sender<()>>>,
}

impl RefusesFirstUpload {
    fn new() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let store = Self {
            inner: InMemory::new(),
            refused: Mutex::new(Some(tx)),
        };
        (store, rx)
    }
}

impl fmt::Display for RefusesFirstUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RefusesFirstUpload({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for RefusesFirstUpload {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let first = self.refused.lock().unwrap().take();
        match first {
            Some(refused) => {
                refused.send(()).unwrap();
                Err(object_store::Error::Generic {
                    store: "RefusesFirstUpload",
                    source: "the first upload is refused".into(),
                })
            }
            None => self.inner.put_opts(location, payload, opts).await,
        }
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn a_failed_upload_stays_pending_and_ships_on_the_next_round() {
    let t = DayLog::new();
    let file = frozen_day(&t);
    let (store, refused) = RefusesFirstUpload::new();
    let bucket = LocalBucket {
        store: Arc::new(store),
    };

    // The day is frozen before the sink opens, so the opening round is the one whose upload is
    // refused, and no round is owed until the row below is sealed. The clock reads `NEXT_DAY`
    // throughout: the opening round reads it on one loop while seals read it on another, so a
    // script whose readings differ would depend on which loop got there first.
    let sink = t.shipping_sink(&bucket, scripted(&[NEXT_DAY])).await;
    refused.await.unwrap();
    assert_eq!(t.ships(DAY), vec![Some(Upload::Pending)]);
    sink.append(TestRow::new(NEXT_DAY, "x")).await.unwrap();
    let closed = sink.close().await;

    assert!(closed.is_ok(), "{closed:?}");
    let failed = sink
        .health()
        .upkeep_failed
        .expect("the failed upload is reported");
    let local = t.part(DAY).file_path(&DataFile::parse(&file).unwrap());
    assert!(
        matches!(&*failed.last, LogError::Upload { path, .. } if path == &local),
        "{:?}",
        failed.last
    );
    assert_eq!(failed.times, 1);
    assert_eq!(t.ships(DAY), vec![Some(Upload::Done)]);
    assert_eq!(bucket.rows(DAY).await, test_rows(&[DAY, DAY + 1, DAY + 2]));
}

#[tokio::test]
async fn a_writer_with_no_target_writes_no_ship_field() {
    let t = DayLog::new();

    let (clock, opened) = day_then_next_day();
    let sink = t.sink(clock);
    fill_day_then_cross_midnight(&sink, opened, 0).await;
    sink.close().await.unwrap();

    assert_eq!(t.ships(DAY), vec![None]);
    let bytes = std::fs::read(t.part(DAY).path().join("_live.json")).unwrap();
    let manifest = String::from_utf8(bytes).unwrap();
    assert!(!manifest.contains("ship"), "{manifest}");
    assert!(!t.dir.path().join("_store_id").exists());
}

#[tokio::test]
async fn a_writer_with_no_target_never_merges_a_frozen_file() {
    let t = DayLog::new();
    frozen_day(&t);
    publish(&t.part(DAY), "late", &[DAY + 9]);

    let sink = t.sink(scripted(&[NEXT_DAY]));
    sink.close().await.unwrap();

    assert_eq!(
        t.ships(DAY),
        vec![Some(Upload::Pending), None],
        "the late file is left on its own, and nothing is frozen without a bucket"
    );
}

#[tokio::test]
async fn a_date_directory_naming_no_day_is_reported_and_the_other_days_still_ship() {
    let t = DayLog::new();
    let bucket = LocalBucket::in_memory();
    frozen_day(&t);
    let stray = t
        .dir
        .path()
        .join("tenant=other")
        .join(DayTable::NAME.as_str())
        .join("date=2026-11-07.bak");
    std::fs::create_dir_all(&stray).unwrap();

    let sink = t.shipping_sink(&bucket, scripted(&[NEXT_DAY])).await;
    sink.close().await.unwrap();

    assert_eq!(t.ships(DAY), vec![Some(Upload::Done)]);
    let failed = sink.health().upkeep_failed.expect("the stray is reported");
    assert!(
        matches!(&*failed.last, LogError::PartitionDay { path } if path == &stray),
        "{:?}",
        failed.last
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_sweep_whose_listing_failed_is_tried_again_by_the_next_round_of_the_same_day() {
    use std::os::unix::fs::PermissionsExt;
    let t = DayLog::new();
    let bucket = LocalBucket::in_memory();
    frozen_day(&t);
    let shut = t.dir.path().join("tenant=other");
    std::fs::create_dir_all(shut.join(DayTable::NAME.as_str())).unwrap();
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();

    let sink = t.shipping_sink(&bucket, scripted(&[NEXT_DAY])).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while sink.health().upkeep_failed.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the listing fails");
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
    sink.append(TestRow::new(NEXT_DAY, "x")).await.unwrap();
    sink.close().await.unwrap();

    assert_eq!(t.ships(DAY), vec![Some(Upload::Done)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stuck_upload_does_not_hold_up_todays_merges() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    let t = DayLog::new();
    frozen_day(&t);
    let stuck = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_secs(3600),
            ..ThrottleConfig::default()
        },
    );
    let bucket = LocalBucket {
        store: Arc::new(stuck),
    };

    let sink = t.shipping_sink(&bucket, scripted(&[NEXT_DAY])).await;
    for k in 0..16 {
        sink.append(TestRow::new(NEXT_DAY + k, "x")).await.unwrap();
    }
    // Sixteen one-row seals make a merge due, whose output is named `L<n>-…`. The wait only
    // bounds a run that would otherwise never end; a busy machine finds the merge long before.
    let merged = tokio::time::timeout(Duration::from_secs(120), async {
        while !live_names(&t.part(NEXT_DAY))
            .iter()
            .any(|name| name.starts_with('L'))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    assert!(
        merged.is_ok(),
        "today's {} files are left unmerged behind the upload",
        live_names(&t.part(NEXT_DAY)).len()
    );
    assert_eq!(t.ships(DAY), vec![Some(Upload::Pending)]);
}

#[tokio::test(start_paused = true)]
async fn a_day_with_nothing_sealed_after_it_ships_once_midnight_passes() {
    let t = DayLog::new();
    let bucket = LocalBucket::in_memory();
    let sink = t.shipping_sink(&bucket, scripted(&[DAY])).await;
    sink.append(TestRow::new(DAY, "x")).await.unwrap();

    tokio::time::sleep(Duration::from_secs(86_400)).await;
    sink.close().await.unwrap();

    assert_eq!(t.ships(DAY), vec![Some(Upload::Done)]);
    assert_eq!(bucket.rows(DAY).await, test_rows(&[DAY]));
}
