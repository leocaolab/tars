//! `store_meta`: the runner that first opens a durable store records its
//! tenant and a durable store id there, a later open for another tenant is refused, and a tool
//! with no tenant of its own reads them back through `open_existing` without changing
//! the durable store — or, on a durable store no current runner has claimed, opens it read-only.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tars_storage::durable::journal::{MemTrajectoryLog, TrajectoryLog};
use tars_storage::durable::{
    Cursor, DurableStore, DurableStoreInitError, DurableStoreRead, ExistingDurableStore,
    JournalSealPolicy, MemDurableStore, ReadOnlyDurableStore, Sealing, SqliteDurableStore,
    Unclaimed, recorded_tenant, retenant,
};
use tars_types::{AgentKey, DurableStoreId, JobId, TaskId, TaskSpec, TenantId};

/// Returns a fresh in-memory journal store: no test here seals a journal.
fn journal() -> Arc<dyn TrajectoryLog> {
    Arc::new(MemTrajectoryLog::new())
}

/// Returns a schedule that never comes round within a test, so only `close` seals, by a clock
/// stopped at 0: no test here seals a journal.
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

fn tenant(name: &str) -> TenantId {
    name.parse::<TenantId>().unwrap()
}

async fn read_only(path: &Path) -> SqlitePool {
    let opts = SqliteConnectOptions::new().filename(path).read_only(true);
    SqlitePoolOptions::new().connect_with(opts).await.unwrap()
}

async fn stored(path: &Path) -> Option<TenantId> {
    let pool = read_only(path).await;
    let t = recorded_tenant(&pool).await.unwrap();
    pool.close().await;
    t
}

async fn stored_store_id(path: &Path) -> Option<String> {
    let pool = read_only(path).await;
    let id = sqlx::query_scalar("SELECT store_id FROM store_meta")
        .fetch_optional(&pool)
        .await
        .unwrap();
    pool.close().await;
    id
}

async fn execute(path: &Path, sql: &str) {
    let opts = SqliteConnectOptions::new().filename(path);
    let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn read_only_store(path: &Path) -> ReadOnlyDurableStore {
    match SqliteDurableStore::open_existing(path, journal())
        .await
        .unwrap()
    {
        ExistingDurableStore::Unclaimed(store) => store,
        ExistingDurableStore::Claimed(_) => panic!("expected a read-only durable store"),
    }
}

/// A durable store no runner has claimed, with one task and one journal row, its `Enqueued`:
/// written by a runner, then stripped of what `store_meta` records.
async fn unclaimed_store_with_one_task(path: &Path) {
    let runner = SqliteDurableStore::open(path, tenant("acme"), journal(), sealing())
        .await
        .unwrap();
    runner
        .enqueue(
            &TaskSpec::new(
                TaskId::new("t"),
                JobId::new("job"),
                AgentKey::new("agent"),
                serde_json::Value::Null,
            ),
            1,
        )
        .await
        .unwrap();
    runner.close().await.unwrap();
    execute(path, "DELETE FROM store_meta").await;
}

async fn open_and_close(path: &Path, t: TenantId) {
    SqliteDurableStore::open(path, t, journal(), sealing())
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

/// Every object in the file and every recorded migration — what a migration would change.
async fn schema(path: &Path) -> Vec<String> {
    let pool = read_only(path).await;
    let mut all: Vec<String> =
        sqlx::query_scalar("SELECT type || ' ' || name FROM sqlite_master ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    all.extend(versions.iter().map(|v| format!("migration {v}")));
    pool.close().await;
    all
}

/// A durable store with its schema and no claim: what a runner leaves when it dies between
/// migrating the file and recording its tenant.
async fn unclaimed_store(path: &Path) {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
    sqlx::migrate!("migrations/durable")
        .run(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn the_first_open_records_the_runners_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");

    let store = SqliteDurableStore::open(&path, tenant("acme"), journal(), sealing())
        .await
        .unwrap();
    assert_eq!(store.tenant(), &tenant("acme"));
    store.close().await.unwrap();

    assert_eq!(stored(&path).await, Some(tenant("acme")));
}

#[tokio::test]
async fn reopening_for_the_same_tenant_opens_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    open_and_close(&path, tenant("acme")).await;
    let before = schema(&path).await;

    open_and_close(&path, tenant("acme")).await;

    assert_eq!(stored(&path).await, Some(tenant("acme")));
    assert_eq!(schema(&path).await, before);
}

#[tokio::test]
async fn reopening_for_another_tenant_is_refused_and_the_store_keeps_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    open_and_close(&path, tenant("acme")).await;

    match SqliteDurableStore::open(&path, tenant("globex"), journal(), sealing()).await {
        Err(DurableStoreInitError::TenantMismatch { stored, given }) => {
            assert_eq!((stored, given), (tenant("acme"), tenant("globex")));
        }
        other => panic!("expected TenantMismatch, got {:?}", other.map(|_| ())),
    }
    assert_eq!(stored(&path).await, Some(tenant("acme")));
}

#[tokio::test]
async fn open_existing_reads_the_tenant_the_runner_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    let runner = SqliteDurableStore::open(&path, tenant("acme"), journal(), sealing())
        .await
        .unwrap();
    runner
        .enqueue(
            &TaskSpec::new(
                TaskId::new("t"),
                JobId::new("job"),
                AgentKey::new("agent"),
                serde_json::Value::Null,
            ),
            0,
        )
        .await
        .unwrap();
    runner.close().await.unwrap();

    let ExistingDurableStore::Claimed(store) = SqliteDurableStore::open_existing(&path, journal())
        .await
        .unwrap()
    else {
        panic!("a durable store its runner claimed opens writable");
    };

    assert_eq!(store.tenant(), &tenant("acme"));
    assert!(
        store.get(&TaskId::new("t")).await.unwrap().is_some(),
        "the same durable store"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn open_existing_on_a_store_with_no_tenant_opens_read_only_and_migrates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    unclaimed_store(&path).await;
    let before = schema(&path).await;

    let store = read_only_store(&path).await;
    let Unclaimed { path: p } = store.why();
    assert_eq!(p, &path);
    store.close().await;
    assert_eq!(schema(&path).await, before, "no migration ran");
    assert_eq!(stored(&path).await, None);
}

/// The durable store id is written once, by the first runner, and every later open — the
/// runner's own or a tool's — reads back that same id.
#[tokio::test]
async fn the_first_open_records_a_store_id_and_every_reopen_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");

    let first = SqliteDurableStore::open(&path, tenant("acme"), journal(), sealing())
        .await
        .unwrap();
    let id = first.id().clone();
    first.close().await.unwrap();
    let again = SqliteDurableStore::open(&path, tenant("acme"), journal(), sealing())
        .await
        .unwrap();
    assert_eq!(again.id(), &id, "a reopen keeps the id");
    again.close().await.unwrap();
    let ExistingDurableStore::Claimed(tool) = SqliteDurableStore::open_existing(&path, journal())
        .await
        .unwrap()
    else {
        panic!("a durable store its runner claimed opens writable");
    };
    assert_eq!(tool.id(), &id, "a tool reads the same id");
    tool.close().await.unwrap();

    assert_eq!(stored_store_id(&path).await, Some(id.as_str().to_owned()));
    let other = SqliteDurableStore::in_memory(tenant("acme")).await.unwrap();
    assert_ne!(other.id(), &id, "another durable store, another id");
}

/// `DurableStore::tenant()` answers the tenant each backend was built for, through the trait.
#[tokio::test]
async fn every_store_answers_whose_work_it_holds() {
    let stores: Vec<(Arc<dyn DurableStore>, &str)> = vec![
        (
            Arc::new(SqliteDurableStore::in_memory(tenant("acme")).await.unwrap()),
            "acme",
        ),
        (Arc::new(MemDurableStore::new(tenant("globex"))), "globex"),
    ];
    for (store, want) in &stores {
        assert_eq!(store.tenant(), &tenant(want));
    }
    let ids: std::collections::HashSet<DurableStoreId> =
        stores.iter().map(|(b, _)| b.id().clone()).collect();
    assert_eq!(
        ids.len(),
        stores.len(),
        "every durable store has its own id"
    );
}

/// A durable store no runner has claimed is browsable, and opening it changes not one byte of it.
#[tokio::test]
async fn a_read_only_store_reads_tasks_and_journals_and_leaves_the_file_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    unclaimed_store_with_one_task(&path).await;
    let before = std::fs::read(&path).unwrap();

    let store = read_only_store(&path).await;
    assert!(store.get(&TaskId::new("t")).await.unwrap().is_some());
    assert_eq!(store.tasks().await.unwrap().len(), 1);
    assert_eq!(store.trajectory(&TaskId::new("t")).await.unwrap().len(), 1);
    assert_eq!(store.since(Cursor::START).await.unwrap().len(), 1);
    store.close().await;

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "not one byte changed"
    );
}

/// The tenant and the id are one row: the schema takes no tenant without an id, and no second
/// claim beside the first.
#[tokio::test]
async fn the_schema_records_a_tenant_only_with_an_id_and_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    unclaimed_store(&path).await;
    let pool = writable(&path).await;
    let insert = |sql: &'static str| sqlx::query(sql).execute(&pool);

    let refused = insert("INSERT INTO store_meta (one, tenant) VALUES (1, 'acme')")
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("NOT NULL constraint failed"), "{refused}");

    insert("INSERT INTO store_meta (one, tenant, store_id) VALUES (1, 'acme', 'x')")
        .await
        .unwrap();
    let refused =
        insert("INSERT INTO store_meta (one, tenant, store_id) VALUES (2, 'globex', 'y')")
            .await
            .unwrap_err()
            .to_string();
    assert!(refused.contains("CHECK constraint failed"), "{refused}");
    pool.close().await;
}

#[tokio::test]
async fn open_existing_does_not_create_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");

    assert!(
        SqliteDurableStore::open_existing(&path, journal())
            .await
            .is_err()
    );
    assert!(!path.exists());
}

async fn writable(path: &Path) -> SqlitePool {
    let opts = SqliteConnectOptions::new().filename(path);
    SqlitePoolOptions::new().connect_with(opts).await.unwrap()
}

async fn retenant_at(
    path: &Path,
    from: &[TenantId],
    to: &TenantId,
) -> Result<Option<TenantId>, DurableStoreInitError> {
    let pool = writable(path).await;
    let got = retenant(&pool, from, to).await;
    pool.close().await;
    got
}

#[tokio::test]
async fn retenant_moves_a_store_from_an_old_value_and_keeps_its_id_and_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    let runner = SqliteDurableStore::open(&path, tenant("o/old"), journal(), sealing())
        .await
        .unwrap();
    runner
        .enqueue(
            &TaskSpec::new(
                TaskId::new("t"),
                JobId::new("job"),
                AgentKey::new("agent"),
                serde_json::Value::Null,
            ),
            0,
        )
        .await
        .unwrap();
    runner.close().await.unwrap();
    let id = stored_store_id(&path).await;

    let was = retenant_at(&path, &[tenant("o/new"), tenant("o/old")], &tenant("id"))
        .await
        .unwrap();

    assert_eq!(was, Some(tenant("o/old")));
    assert_eq!(stored(&path).await, Some(tenant("id")));
    assert_eq!(stored_store_id(&path).await, id);
    let store = SqliteDurableStore::open(&path, tenant("id"), journal(), sealing())
        .await
        .unwrap();
    assert!(
        store.get(&TaskId::new("t")).await.unwrap().is_some(),
        "the same durable store"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn retenant_run_again_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    open_and_close(&path, tenant("o/old")).await;
    retenant_at(&path, &[tenant("o/old")], &tenant("id"))
        .await
        .unwrap();

    let again = retenant_at(&path, &[tenant("o/old")], &tenant("id"))
        .await
        .unwrap();

    assert_eq!(again, None);
    assert_eq!(stored(&path).await, Some(tenant("id")));
}

#[tokio::test]
async fn retenant_refuses_a_store_another_tenant_holds_and_leaves_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    open_and_close(&path, tenant("o/other")).await;

    match retenant_at(&path, &[tenant("o/old")], &tenant("id")).await {
        Err(DurableStoreInitError::TenantMismatch { stored, given }) => {
            assert_eq!((stored, given), (tenant("o/other"), tenant("id")));
        }
        other => panic!("expected TenantMismatch, got {other:?}"),
    }
    assert_eq!(stored(&path).await, Some(tenant("o/other")));
}

#[tokio::test]
async fn retenant_on_a_store_no_runner_claimed_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiber.db");
    unclaimed_store(&path).await;
    let before = schema(&path).await;

    let was = retenant_at(&path, &[tenant("o/old")], &tenant("id"))
        .await
        .unwrap();

    assert_eq!(was, None);
    assert_eq!(schema(&path).await, before);
    assert_eq!(stored(&path).await, None);
}
