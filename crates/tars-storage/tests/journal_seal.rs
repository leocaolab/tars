//! Sealing finished tasks' journals into a file, with the same answer from both durable stores.
//!
//! A journal row is, at any moment, in exactly one place: the durable store's live journal or
//! a live sealed file. Sealing moves it in one commit that re-checks the seal
//! conditions; a refusal changes nothing anyone can see; an append to a sealed task is
//! refused.
use std::backtrace::Backtrace;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use serde_json::json;
use tars_storage::durable::journal::{
    JournalError, JournalFileName, JournalRow, MemTrajectoryLog, StagedFile, TrajectoryLog,
};
use tars_storage::durable::{
    Cursor, DurableStore, DurableStoreError, JournalSealPolicy, MemDurableStore, SealJournals,
    SealedJournal, Sealing, SqliteDurableStore, Unsealable, Writer,
};
use tars_types::{
    AgentKey, Event, JobId, JournaledReason, Reason, Status, TaskId, TaskSpec, TenantId,
};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;
type Sweep = JoinHandle<Result<usize, DurableStoreError>>;

/// A journal store whose `stage` can run one hook after the file is written and
/// before the durable store commits — the window the commit's re-check guards — or fail right
/// there as a crash would, and whose reads can be made to fail. Every read attempt
/// is recorded with the stack that made it, so a test can fail on any read it forbids.
struct Store {
    inner: Arc<dyn TrajectoryLog>,
    after_stage: Mutex<Option<Hook>>,
    /// Runs inside `remove`, before the file goes.
    before_remove: Mutex<Option<Hook>>,
    crash_after_stage: AtomicBool,
    fail_reads: AtomicBool,
    reads: Mutex<Vec<Backtrace>>,
    removes: AtomicUsize,
}

impl Store {
    fn new(inner: Arc<dyn TrajectoryLog>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            after_stage: Mutex::new(None),
            before_remove: Mutex::new(None),
            crash_after_stage: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: Mutex::new(Vec::new()),
            removes: AtomicUsize::new(0),
        })
    }

    fn between_stage_and_commit(&self, hook: Hook) {
        *self.after_stage.lock().unwrap() = Some(hook);
    }

    fn reads(&self) -> usize {
        self.reads.lock().unwrap().len()
    }
}

#[async_trait]
impl TrajectoryLog for Store {
    async fn stage(
        &self,
        name: &JournalFileName,
        rows: Vec<JournalRow>,
    ) -> Result<StagedFile, JournalError> {
        let staged = self.inner.stage(name, rows).await?;
        let hook = self.after_stage.lock().unwrap().take();
        if let Some(hook) = hook {
            hook().await;
        }
        if self.crash_after_stage.load(Ordering::SeqCst) {
            return Err(JournalError::Store("crashed after staging".into()));
        }
        Ok(staged)
    }

    async fn read_group(
        &self,
        file: &JournalFileName,
        row_group: u32,
    ) -> Result<Vec<JournalRow>, JournalError> {
        self.reads.lock().unwrap().push(Backtrace::force_capture());
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(JournalError::NoGroup {
                file: file.clone(),
                row_group,
            });
        }
        self.inner.read_group(file, row_group).await
    }

    async fn remove(&self, name: &JournalFileName) -> Result<(), JournalError> {
        self.removes.fetch_add(1, Ordering::SeqCst);
        let hook = self.before_remove.lock().unwrap().take();
        if let Some(hook) = hook {
            hook().await;
        }
        self.inner.remove(name).await
    }

    async fn stored(&self) -> Result<Vec<JournalFileName>, JournalError> {
        self.inner.stored().await
    }
}

struct Fixture {
    name: &'static str,
    durable: Arc<dyn DurableStore>,
    archive: Arc<dyn SealJournals>,
    store: Arc<Store>,
    /// Keeps the SQLite file alive for the fixture's life.
    _dir: Option<TempDir>,
}

/// Returns a schedule that never comes round within a test, so the tests here seal by hand,
/// by a clock stopped at 0.
fn sealing() -> Sealing {
    Sealing {
        policy: JournalSealPolicy {
            every: Duration::from_secs(3_600),
            ..JournalSealPolicy::default()
        },
        clock: Arc::new(|| 0),
        rt: tokio::runtime::Handle::current(),
    }
}

/// The in-memory durable store and the SQLite durable store, each over a journal store in memory.
async fn fixtures() -> Vec<Fixture> {
    let mem_store = Store::new(Arc::new(MemTrajectoryLog::new()));
    let mem = Arc::new(MemDurableStore::new(TenantId::new("test")).with_journal(mem_store.clone()));
    let sqlite_store = Store::new(Arc::new(MemTrajectoryLog::new()));
    let dir = tempfile::tempdir().unwrap();
    let sqlite = Arc::new(
        SqliteDurableStore::open(
            dir.path().join("fiber.db"),
            TenantId::new("tenant-test"),
            sqlite_store.clone(),
            sealing(),
        )
        .await
        .unwrap(),
    );
    vec![
        Fixture {
            name: "mem",
            durable: mem.clone(),
            archive: mem,
            store: mem_store,
            _dir: None,
        },
        Fixture {
            name: "sqlite",
            durable: sqlite.clone(),
            archive: sqlite,
            store: sqlite_store,
            _dir: Some(dir),
        },
    ]
}

fn delta(n: i64) -> Value {
    json!({ "z": n, "a": [n, "x"] })
}

/// `id` claimed, journalled three times, committed — terminal, owing nothing.
async fn finished(b: &Arc<dyn DurableStore>, id: &TaskId) -> Vec<Event> {
    b.enqueue(
        &TaskSpec::new(id.clone(), JobId::new("j"), AgentKey::new("k"), json!(id)),
        0,
    )
    .await
    .unwrap();
    let claim = b
        .claim_ready("r", 0, 10)
        .await
        .unwrap()
        .expect("claimable")
        .token;
    for n in 0..3 {
        b.append_delta(Writer::Claim(claim), id, delta(n), 0, 100 + n)
            .await
            .unwrap();
    }
    b.commit(id, Writer::Claim(claim), json!({ "summary": "done" }), 0)
        .await
        .unwrap();
    b.trajectory(id).await.unwrap()
}

async fn seal(
    b: &Arc<dyn SealJournals>,
    id: &TaskId,
    now: i64,
) -> Result<SealedJournal, DurableStoreError> {
    b.seal_tasks(&[TaskId::try_from(id.to_string()).unwrap()], now)
        .await
        .map(|mut sealed| sealed.remove(0))
}

fn live_rows_of(events: Vec<Event>, id: &TaskId) -> Vec<Event> {
    events
        .into_iter()
        .filter(|e| e.task_id == id.clone())
        .collect()
}

fn refused(durable: &str, r: Result<SealedJournal, DurableStoreError>) -> Unsealable {
    match r {
        Err(DurableStoreError::Unsealable { why, .. }) => why,
        other => panic!("{durable}: expected Unsealable, got {other:?}"),
    }
}

#[tokio::test]
async fn sealing_a_finished_task_moves_its_journal_whole_into_the_file() {
    for f in fixtures().await {
        let before = finished(&f.durable, &TaskId::new("t")).await;
        assert_eq!(
            before.len(),
            6,
            "{}: premise: enqueued, running, three deltas, done",
            f.name
        );

        let sealed = seal(&f.archive, &TaskId::new("t"), 1_000).await.unwrap();

        assert_eq!(
            live_rows_of(
                f.durable.since(Cursor::START).await.unwrap(),
                &TaskId::new("t")
            ),
            before,
            "{}: the stream yields every row exactly once, now read from the file",
            f.name
        );
        let in_file = f
            .store
            .read_group(&sealed.file, sealed.row_group)
            .await
            .unwrap();
        let seqs: Vec<u64> = in_file.iter().map(|r| r.seq).collect();
        assert_eq!(
            seqs,
            before.iter().map(|e| e.seq).collect::<Vec<_>>(),
            "{}",
            f.name
        );
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t")).await.unwrap(),
            before,
            "{}: the journal reads back identical",
            f.name
        );
        assert_eq!(
            refused(f.name, seal(&f.archive, &TaskId::new("t"), 1_000).await),
            Unsealable::AlreadySealed,
            "{}: the index says it is sealed",
            f.name
        );
    }
}

#[tokio::test]
async fn a_task_s_t_refuses_is_left_exactly_as_it_was() {
    for f in fixtures().await {
        f.durable
            .enqueue(
                &TaskSpec::new(
                    TaskId::new("open"),
                    JobId::new("j"),
                    AgentKey::new("k"),
                    json!(1),
                ),
                0,
            )
            .await
            .unwrap();
        f.durable
            .append_delta(Writer::Unclaimed, &TaskId::new("open"), delta(0), 0, 1)
            .await
            .unwrap();
        assert_eq!(
            refused(f.name, seal(&f.archive, &TaskId::new("open"), 1_000).await),
            Unsealable::NotTerminal(Status::Ready),
            "{}",
            f.name
        );
        assert_eq!(
            f.durable
                .trajectory(&TaskId::new("open"))
                .await
                .unwrap()
                .len(),
            2
        );

        f.durable
            .enqueue(
                &TaskSpec::new(
                    TaskId::new("owes"),
                    JobId::new("j"),
                    AgentKey::new("k"),
                    json!(2),
                ),
                0,
            )
            .await
            .unwrap();
        f.durable
            .append_delta(Writer::Unclaimed, &TaskId::new("owes"), delta(1), 1, 2)
            .await
            .unwrap();
        f.durable
            .commit(&TaskId::new("owes"), Writer::Unclaimed, json!(null), 0)
            .await
            .unwrap();
        assert_eq!(
            refused(f.name, seal(&f.archive, &TaskId::new("owes"), 1_000).await),
            Unsealable::Holds(1),
            "{}",
            f.name
        );
        f.durable
            .append_delta(Writer::Unclaimed, &TaskId::new("owes"), delta(2), 0, 3)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_row_appended_while_the_file_was_written_voids_the_seal() {
    for f in fixtures().await {
        let before = finished(&f.durable, &TaskId::new("t")).await;
        let durable = f.durable.clone();
        f.store.between_stage_and_commit(Box::new(move || {
            Box::pin(async move {
                durable
                    .append_delta(Writer::Unclaimed, &TaskId::new("t"), delta(9), 0, 999)
                    .await
                    .unwrap();
            })
        }));

        let why = refused(f.name, seal(&f.archive, &TaskId::new("t"), 1_000).await);

        assert!(
            matches!(why, Unsealable::JournalMoved { staged, now: Some(now) } if now > staged),
            "{}: {why:?}",
            f.name
        );
        let after = f.durable.trajectory(&TaskId::new("t")).await.unwrap();
        assert_eq!(after.len(), before.len() + 1, "{}", f.name);
        assert_eq!(after[..before.len()], before[..], "{}", f.name);
        assert_eq!(
            live_rows_of(
                f.durable.since(Cursor::START).await.unwrap(),
                &TaskId::new("t")
            ),
            after,
            "{}: every row still in the live journal, none in a file",
            f.name
        );
        f.durable
            .append_delta(Writer::Unclaimed, &TaskId::new("t"), delta(10), 0, 1_001)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn an_append_to_a_sealed_task_is_refused_with_its_holds_move() {
    for f in fixtures().await {
        let before = finished(&f.durable, &TaskId::new("t")).await;
        seal(&f.archive, &TaskId::new("t"), 1_000).await.unwrap();

        let plain = f
            .durable
            .append_delta(Writer::Unclaimed, &TaskId::new("t"), delta(7), 0, 2_000)
            .await;
        assert!(
            matches!(&plain, Err(DurableStoreError::JournalSealed { task_id }) if task_id.as_str() == "t"),
            "{}: {plain:?}",
            f.name
        );

        let holding = f
            .durable
            .append_delta(Writer::Unclaimed, &TaskId::new("t"), delta(8), 1, 2_001)
            .await;
        assert!(
            matches!(holding, Err(DurableStoreError::JournalSealed { .. })),
            "{}: {holding:?}",
            f.name
        );
        assert_eq!(
            f.durable
                .get(&TaskId::new("t"))
                .await
                .unwrap()
                .unwrap()
                .holds,
            0,
            "{}: the holds did not move with the refused append",
            f.name
        );
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t")).await.unwrap(),
            before,
            "{}",
            f.name
        );
    }
}

#[tokio::test]
async fn a_batch_seals_several_tasks_into_one_file_and_since_merges_them_back() {
    for f in fixtures().await {
        let t1 = finished(&f.durable, &TaskId::new("t1")).await;
        let t2 = finished(&f.durable, &TaskId::new("t2")).await;
        let stream = f.durable.since(Cursor::START).await.unwrap();

        let batch = f
            .archive
            .seal_candidates(10, u64::MAX, 1_000)
            .await
            .unwrap();
        assert_eq!(
            batch,
            vec![TaskId::new("t1"), TaskId::new("t2")],
            "{}",
            f.name
        );
        let sealed = f.archive.seal_tasks(&batch, 1_000).await.unwrap();

        assert_eq!(sealed.len(), 2, "{}", f.name);
        assert_eq!(sealed[0].file, sealed[1].file, "{}: one file", f.name);
        assert_ne!(sealed[0].row_group, sealed[1].row_group, "{}", f.name);
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t1")).await.unwrap(),
            t1,
            "{}",
            f.name
        );
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t2")).await.unwrap(),
            t2,
            "{}",
            f.name
        );
        assert_eq!(
            f.durable.since(Cursor::START).await.unwrap(),
            stream,
            "{}: the stream reads the same, sealed or not",
            f.name
        );
        let mid = t1[1].seq;
        assert_eq!(
            f.durable.since(Cursor::after(mid).unwrap()).await.unwrap(),
            stream
                .into_iter()
                .filter(|e| e.seq > mid)
                .collect::<Vec<_>>(),
            "{}",
            f.name
        );
        assert!(
            f.archive
                .seal_candidates(10, u64::MAX, 1_000)
                .await
                .unwrap()
                .is_empty(),
            "{}: nothing left to pick",
            f.name
        );
    }
}

#[tokio::test]
async fn the_byte_cap_cuts_the_batch() {
    for f in fixtures().await {
        finished(&f.durable, &TaskId::new("t1")).await;
        finished(&f.durable, &TaskId::new("t2")).await;
        assert_eq!(
            f.archive.seal_candidates(10, 1, 1_000).await.unwrap(),
            vec![TaskId::new("t1")],
            "{}: over the cap, one task alone",
            f.name
        );
        assert_eq!(
            f.archive.seal_candidates(1, u64::MAX, 1_000).await.unwrap(),
            vec![TaskId::new("t1")],
            "{}",
            f.name
        );
    }
}

/// `id` claimed, journalled, then failed for good with a reason of its own.
async fn failed(b: &Arc<dyn DurableStore>, id: &TaskId) {
    b.enqueue(
        &TaskSpec::new(id.clone(), JobId::new("j"), AgentKey::new("k"), json!(id)),
        0,
    )
    .await
    .unwrap();
    let claim = b
        .claim_ready("r", 0, 10)
        .await
        .unwrap()
        .expect("claimable")
        .token;
    b.append_delta(Writer::Claim(claim), id, delta(0), 0, 100)
        .await
        .unwrap();
    let reason = JournaledReason::from(&Reason::Execution(format!("{id}: the disk said no")));
    b.fail(id, Writer::Claim(claim), 1, reason, 101)
        .await
        .unwrap();
}

/// A reader's answer as bytes: each event's `kind` as the durable store encodes it, and the
/// failure reason as a caller would serialize it.
#[derive(Debug, PartialEq)]
struct Readings {
    trajectory: Vec<(u64, TaskId, i64, String)>,
    since: Vec<(u64, TaskId, i64, String)>,
    failure: String,
}

fn event_bytes(events: Vec<Event>) -> Vec<(u64, TaskId, i64, String)> {
    events
        .into_iter()
        .map(|e| {
            (
                e.seq,
                e.task_id,
                e.at,
                serde_json::to_string(&e.kind).unwrap(),
            )
        })
        .collect()
}

async fn readings(b: &Arc<dyn DurableStore>, id: &TaskId) -> Readings {
    Readings {
        trajectory: event_bytes(b.trajectory(id).await.unwrap()),
        since: event_bytes(b.since(Cursor::START).await.unwrap()),
        failure: serde_json::to_string(&b.failure(id).await.unwrap()).unwrap(),
    }
}

/// What a task said reads back byte for byte after its journal is sealed —
/// its own journal, the global stream, and why it failed.
#[tokio::test]
async fn every_reader_gets_the_same_bytes_before_and_after_the_seal() {
    for f in fixtures().await {
        failed(&f.durable, &TaskId::new("t")).await;
        let before = readings(&f.durable, &TaskId::new("t")).await;
        assert!(
            before.failure.contains("the disk said no"),
            "{}: premise: {}",
            f.name,
            before.failure
        );

        seal(&f.archive, &TaskId::new("t"), 1_000).await.unwrap();

        assert_eq!(
            readings(&f.durable, &TaskId::new("t")).await,
            before,
            "{}",
            f.name
        );
        assert!(
            f.store.reads() > 0,
            "{}: premise: the second reading came from the file",
            f.name
        );
    }
}

/// A point read of a sealed journal goes to the one row group the index names — not
/// the other tasks' groups in the same file, not the other files.
#[tokio::test]
async fn a_point_read_of_a_sealed_journal_opens_exactly_one_row_group() {
    for f in fixtures().await {
        for id in ["a", "b", "c"] {
            finished(&f.durable, &id.parse::<TaskId>().unwrap()).await;
        }
        let batch = f
            .archive
            .seal_tasks(
                &[
                    TaskId::try_from("a".to_string()).unwrap(),
                    TaskId::try_from("b".to_string()).unwrap(),
                ],
                1_000,
            )
            .await
            .unwrap();
        let alone = seal(&f.archive, &TaskId::new("c"), 1_000).await.unwrap();
        assert_eq!(batch[0].file, batch[1].file, "{}: premise", f.name);
        assert_ne!(batch[0].file, alone.file, "{}: premise", f.name);

        for id in ["a", "b", "c"] {
            let reads = f.store.reads();
            f.durable
                .trajectory(&id.parse::<TaskId>().unwrap())
                .await
                .unwrap();
            assert_eq!(
                f.store.reads() - reads,
                1,
                "{}: `{id}` opened one row group",
                f.name
            );
        }
    }
}

#[tokio::test]
async fn one_failing_recheck_leaves_the_whole_batch_as_it_was() {
    for f in fixtures().await {
        let t1 = finished(&f.durable, &TaskId::new("t1")).await;
        finished(&f.durable, &TaskId::new("t2")).await;
        let durable = f.durable.clone();
        f.store.between_stage_and_commit(Box::new(move || {
            Box::pin(async move {
                durable
                    .append_delta(Writer::Unclaimed, &TaskId::new("t2"), delta(9), 0, 999)
                    .await
                    .unwrap();
            })
        }));

        let r = f
            .archive
            .seal_tasks(
                &[
                    TaskId::try_from("t1".to_string()).unwrap(),
                    TaskId::try_from("t2".to_string()).unwrap(),
                ],
                1_000,
            )
            .await;

        assert!(
            matches!(&r, Err(DurableStoreError::Unsealable { task_id, why: Unsealable::JournalMoved { .. } }) if task_id.as_str() == "t2"),
            "{}: {r:?}",
            f.name
        );
        assert_eq!(
            live_rows_of(
                f.durable.since(Cursor::START).await.unwrap(),
                &TaskId::new("t1")
            ),
            t1,
            "{}: t1 passed its own re-check and is still live",
            f.name
        );
        f.durable
            .append_delta(Writer::Unclaimed, &TaskId::new("t1"), delta(10), 0, 1_001)
            .await
            .expect("t1 is not sealed");
        assert_eq!(f.store.reads(), 0, "{}: no file was made live", f.name);
    }
}

#[tokio::test]
async fn a_crash_after_staging_leaves_a_staged_file_the_grace_sweep_removes() {
    for f in fixtures().await {
        let before = finished(&f.durable, &TaskId::new("t")).await;
        f.store.crash_after_stage.store(true, Ordering::SeqCst);

        let crashed = seal(&f.archive, &TaskId::new("t"), 1_000).await;

        assert!(
            matches!(crashed, Err(DurableStoreError::Journal(_))),
            "{}: {crashed:?}",
            f.name
        );
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t")).await.unwrap(),
            before,
            "{}",
            f.name
        );
        assert_eq!(
            f.archive.remove_staged(999).await.unwrap(),
            0,
            "{}: within its grace a staged file may still be a seal in progress",
            f.name
        );
        assert_eq!(
            f.archive.remove_staged(1_000).await.unwrap(),
            1,
            "{}",
            f.name
        );
        assert_eq!(f.store.removes.load(Ordering::SeqCst), 1, "{}", f.name);
        assert_eq!(
            f.archive.remove_staged(1_000).await.unwrap(),
            0,
            "{}: the registration went with the file",
            f.name
        );

        f.store.crash_after_stage.store(false, Ordering::SeqCst);
        seal(&f.archive, &TaskId::new("t"), 2_000).await.unwrap();
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t")).await.unwrap(),
            before,
            "{}",
            f.name
        );
    }
}

/// The race the cleanup order closes: the grace sweep takes a staged file while its
/// seal is still between staging and commit, and the commit lands in the middle of
/// the sweep — after its first step, before its second. Deleting the registration
/// first makes that commit fail, so nothing live ever names the deleted file.
#[tokio::test]
async fn a_seal_whose_staged_file_is_swept_mid_flight_is_refused_whole() {
    for f in fixtures().await {
        let before = finished(&f.durable, &TaskId::new("t")).await;
        let (swept_half, sealed) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (half, done) = (swept_half.clone(), sealed.clone());
        f.store
            .before_remove
            .lock()
            .unwrap()
            .replace(Box::new(move || {
                Box::pin(async move {
                    half.notify_one();
                    done.notified().await;
                })
            }));
        let (archive, half) = (f.archive.clone(), swept_half.clone());
        let sweep: Arc<Mutex<Option<Sweep>>> = Arc::default();
        let slot = sweep.clone();
        f.store.between_stage_and_commit(Box::new(move || {
            Box::pin(async move {
                *slot.lock().unwrap() =
                    Some(tokio::spawn(
                        async move { archive.remove_staged(1_000).await },
                    ));
                half.notified().await;
            })
        }));

        let r = seal(&f.archive, &TaskId::new("t"), 1_000).await;
        sealed.notify_one();
        let swept = sweep.lock().unwrap().take().expect("the sweep ran");
        assert_eq!(swept.await.unwrap().unwrap(), 1, "{}", f.name);

        assert!(
            matches!(
                &r,
                Err(DurableStoreError::Unsealable {
                    why: Unsealable::StagedFileGone(_),
                    ..
                })
            ),
            "{}: {r:?}",
            f.name
        );
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t")).await.unwrap(),
            before,
            "{}: still live, readable, and not pointing at the deleted file",
            f.name
        );
        assert_eq!(
            live_rows_of(
                f.durable.since(Cursor::START).await.unwrap(),
                &TaskId::new("t")
            ),
            before,
            "{}",
            f.name
        );
        seal(&f.archive, &TaskId::new("t"), 2_000).await.unwrap();
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t")).await.unwrap(),
            before,
            "{}",
            f.name
        );
    }
}

#[tokio::test]
async fn a_file_with_no_registration_is_removed_and_a_registered_one_is_kept() {
    for f in fixtures().await {
        let before = finished(&f.durable, &TaskId::new("t")).await;
        seal(&f.archive, &TaskId::new("t"), 1_000).await.unwrap();
        let orphan = JournalFileName::fresh();
        f.store
            .inner
            .stage(
                &orphan,
                vec![JournalRow {
                    task_id: "x".into(),
                    seq: 1,
                    at: 1,
                    payload: "{}".into(),
                }],
            )
            .await
            .unwrap();

        assert_eq!(
            f.archive.remove_unregistered().await.unwrap(),
            1,
            "{}",
            f.name
        );
        assert_eq!(f.store.inner.stored().await.unwrap().len(), 1, "{}", f.name);
        assert_eq!(
            f.durable.trajectory(&TaskId::new("t")).await.unwrap(),
            before,
            "{}",
            f.name
        );
        assert_eq!(
            f.archive.remove_unregistered().await.unwrap(),
            0,
            "{}",
            f.name
        );
    }
}
