//! A SQLite store that seals on its own schedule: it seals finished journals with nobody asking,
//! closing runs one last pass, a failing pass shows in its health and in `close`, and a tool
//! that opens someone else's store does not seal it.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tars_storage::durable::journal::{
    JournalError, JournalFileName, JournalRow, MemTrajectoryLog, StagedFile, TrajectoryLog,
};
use tars_storage::durable::{
    DurableStore, DurableStoreError, ExistingDurableStore, FiberStore, JournalSealPolicy,
    SealHealth, Sealing, SqliteDurableStore, StopSealingError, Writer,
};
use tars_types::{AgentKey, JobId, Millis, TaskId, TaskSpec, TenantId};
use tempfile::TempDir;

/// Past the lease `finished` leaves, so the task's runner has let go by this clock.
const NOW: Millis = 1_000;

/// A journal store over memory whose `stage` can be made to fail.
struct Journal {
    inner: MemTrajectoryLog,
    fail_stage: AtomicBool,
}

impl Journal {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemTrajectoryLog::new(),
            fail_stage: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl TrajectoryLog for Journal {
    async fn stage(
        &self,
        name: &JournalFileName,
        rows: Vec<JournalRow>,
    ) -> Result<StagedFile, JournalError> {
        if self.fail_stage.load(Ordering::SeqCst) {
            return Err(JournalError::Store("the disk is full".into()));
        }
        self.inner.stage(name, rows).await
    }

    async fn read_group(
        &self,
        file: &JournalFileName,
        row_group: u32,
    ) -> Result<Vec<JournalRow>, JournalError> {
        self.inner.read_group(file, row_group).await
    }

    async fn remove(&self, name: &JournalFileName) -> Result<(), JournalError> {
        self.inner.remove(name).await
    }

    async fn stored(&self) -> Result<Vec<JournalFileName>, JournalError> {
        self.inner.stored().await
    }
}

fn sealing(every: Duration) -> Sealing {
    Sealing {
        policy: JournalSealPolicy {
            every,
            ..JournalSealPolicy::default()
        },
        clock: Arc::new(|| NOW),
        rt: tokio::runtime::Handle::current(),
    }
}

/// A wait between passes no test here reaches, so only `close` seals.
const HOURLY: Duration = Duration::from_secs(3_600);

async fn open(dir: &TempDir, journal: &Arc<Journal>, every: Duration) -> SqliteDurableStore {
    SqliteDurableStore::open(
        dir.path().join("fiber.db"),
        TenantId::new("test"),
        journal.clone(),
        sealing(every),
    )
    .await
    .unwrap()
}

/// `id` claimed, journalled once, committed and let go: terminal, owing nothing.
async fn finished(b: &SqliteDurableStore, id: &'static str) {
    let id = TaskId::new(id);
    b.enqueue(
        &TaskSpec::new(id.clone(), JobId::new("j"), AgentKey::new("k"), json!(1)),
        0,
    )
    .await
    .unwrap();
    let token = b.claim_ready("r", 0, 10).await.unwrap().unwrap().token;
    b.append_delta(Writer::Claim(token), &id, json!(1), 0, 1)
        .await
        .unwrap();
    b.commit(&id, Writer::Claim(token), json!("done"), 0)
        .await
        .unwrap();
    b.release_lease(&id, token).await.unwrap();
}

async fn is_sealed(b: &SqliteDurableStore, id: &'static str) -> bool {
    matches!(
        b.append_delta(Writer::Unclaimed, &TaskId::new(id), json!(2), 0, 2)
            .await,
        Err(DurableStoreError::JournalSealed { .. })
    )
}

#[tokio::test]
async fn a_store_seals_a_finished_journal_with_nobody_asking() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::new();
    let store = open(&dir, &journal, Duration::from_millis(5)).await;
    finished(&store, "t").await;

    let sealed = tokio::time::timeout(Duration::from_secs(10), async {
        while !is_sealed(&store, "t").await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;

    assert!(sealed.is_ok(), "the schedule never sealed `t`");
    assert!(matches!(store.seal_health(), Some(SealHealth::Healthy)));
    store.close().await.unwrap();
}

#[tokio::test]
async fn closing_seals_what_the_schedule_has_not_reached_yet() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::new();
    let store = open(&dir, &journal, HOURLY).await;
    finished(&store, "t").await;

    store.close().await.unwrap();

    let reopened = open(&dir, &journal, HOURLY).await;
    assert!(is_sealed(&reopened, "t").await, "close sealed `t`");
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn a_failing_pass_shows_in_the_health_and_the_last_one_in_close() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::new();
    journal.fail_stage.store(true, Ordering::SeqCst);
    let store = open(&dir, &journal, Duration::from_millis(5)).await;
    finished(&store, "t").await;

    let failing = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(SealHealth::Failing { last, .. }) = store.seal_health() {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a failing pass reaches the health");

    assert!(
        format!("{:?}", failing).contains("the disk is full"),
        "the health carries the failure as raised: {failing:?}"
    );
    assert!(!is_sealed(&store, "t").await, "a failed pass seals nothing");
    assert!(matches!(
        store.close().await,
        Err(StopSealingError::LastPass(_))
    ));
}

#[tokio::test]
async fn a_store_opened_by_a_tool_that_does_not_own_it_has_no_health_and_closes_clean() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::new();
    let owner = open(&dir, &journal, HOURLY).await;
    finished(&owner, "t").await;
    // Dropped without closing, so the owner's schedule ends with no last pass.
    drop(owner);
    let ExistingDurableStore::Claimed(store) =
        SqliteDurableStore::open_existing(dir.path().join("fiber.db"), journal.clone())
            .await
            .unwrap()
    else {
        panic!("the owner claimed the store");
    };

    assert!(store.seal_health().is_none());
    store.close().await.unwrap();
    let reopened = open(&dir, &journal, HOURLY).await;
    assert!(!is_sealed(&reopened, "t").await, "nobody sealed `t`");
    reopened.close().await.unwrap();
}
