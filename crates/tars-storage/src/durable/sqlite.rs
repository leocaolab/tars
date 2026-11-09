//! Keeps a [`DurableStore`] in one SQLite file, so the queue and every task's trajectory survive a
//! restart.
//!
//! A write of several statements runs in one transaction; one that reads before it writes
//! begins `IMMEDIATE`, so a claim's pick of a ready id and its flip of that row are one step
//! and runners sharing the file never both win one task. A stored value the durable store never
//! writes (an unknown status, half an interruption record, an unreadable lease token, a
//! negative holds count, an unknown event kind, a negative seq) is a typed `Err` carrying what was stored, never read
//! as a default.
//!
//! The pool and its pragmas belong to `tars-storage`; the format of a sealed journal file
//! belongs to the [`TrajectoryLog`] the durable store is handed.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::durable::journal::{
    JournalFileName, JournalRow, MemTrajectoryLog, StagedFile, TrajectoryLog,
};
use crate::{Access, Db};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::{AssertSqlSafe, Row, SqliteConnection, SqlitePool};

use crate::durable::meta::{StoreIdentity, claim, recorded_identity};
use crate::durable::seal::{
    Candidate, IndexEntry, Pending, SealFacts, SealPlan, SealedJournal, event_of, fit, merged_page,
    recheck_batch, remove_files, sealed_journals,
};
use crate::durable::sealed::{index_entry, journal_rows, read_journal_raw, seq_column, stored_seq};
use crate::durable::sealing::{SealHealth, SealJournals, Sealer, Sealing, StopSealingError};
use crate::durable::{
    Cursor, DurableStore, DurableStoreError, DurableStoreRead, FailedAttempt, FiberStore, Park,
    Reply, Wake, Writer, admit, admit_append, answers, failed_attempt, park_of, reply_of,
    stored_id, wake_of,
};
use tars_types::{
    AgentKey, ClaimedTask, DurableStoreId, Event, EventKind, InterruptionStreak, JobId,
    JournaledReason, Lease, LeaseToken, Millis, Status, Task, TaskId, TaskSpec, TenantId,
};
use tars_types::{AgentReply, Wait};
use uuid::Uuid;

/// Caps the events one [`since`](DurableStoreRead::since) call returns, so a long trajectory is
/// read in pages rather than into memory at once.
const SINCE_PAGE_LIMIT: i64 = 1_000;

/// The durable store schema, embedded at compile time and applied at open; `_sqlx_migrations`
/// records which versions a file has.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/durable");

/// Why opening a [`SqliteDurableStore`] failed.
#[derive(Debug, thiserror::Error)]
pub enum DurableStoreInitError {
    #[error("durable store sqlite")]
    Sqlx(#[from] sqlx::Error),
    #[error("durable store database")]
    Db(#[from] crate::DbError),
    /// A durable store holds one tenant's work for good; it is never silently handed to another.
    #[error(
        "the durable store holds tenant `{stored}`'s work and cannot be opened for tenant `{given}`"
    )]
    TenantMismatch { stored: TenantId, given: TenantId },
}

/// Why [`SqliteDurableStore::open_existing`] opened a durable store read-only: no current runner
/// has claimed it, so the facts every write stands on are not recorded — and a tool with no
/// tenant of its own does not get to supply them.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "the durable store at {} records no tenant, so it is read-only: a tenant is only \
     ever written by the runner whose work it is, and no current runner has opened it",
    .path.display()
)]
pub struct Unclaimed {
    pub path: PathBuf,
}

/// A durable store somebody else's runner made, as [`SqliteDurableStore::open_existing`] found it.
pub enum ExistingDurableStore {
    /// Claimed by a runner: read and write it.
    Claimed(SqliteDurableStore),
    /// Never claimed by a current runner: only [`DurableStoreRead`] is there to call.
    Unclaimed(ReadOnlyDurableStore),
}

/// An existing durable store opened read-only on a read-only connection. It implements
/// [`DurableStoreRead`] and not [`DurableStore`], so no write can be routed to it.
pub struct ReadOnlyDurableStore {
    db: Db,
    journal: Arc<dyn TrajectoryLog>,
    why: Unclaimed,
}

impl ReadOnlyDurableStore {
    /// Returns why the durable store is read-only, with its path: the words to refuse a write with.
    pub fn why(&self) -> &Unclaimed {
        &self.why
    }

    /// Closes the read-only pool.
    pub async fn close(&self) {
        self.db.pool().close().await;
    }
}

/// A [`DurableStore`] persisted in a SQLite file, or in memory for tests.
pub struct SqliteDurableStore {
    db: Db,
    tenant: TenantId,
    id: DurableStoreId,
    journal: Arc<dyn TrajectoryLog>,
    /// `None` when whoever opened the store does not seal it.
    sealer: Option<Sealer>,
}

impl SqliteDurableStore {
    /// Opens the durable store file at `path` (creating it if missing) with its sealed journals
    /// kept in `journal`, and starts sealing its finished journals on `sealing`'s schedule: a
    /// task spawned on `sealing.rt` that runs until [`close`](Self::close) or until the store is
    /// dropped.
    ///
    /// The first open records `tenant` in `store_meta`; a later open for a different
    /// tenant is refused with [`DurableStoreInitError::TenantMismatch`].
    pub async fn open(
        path: impl AsRef<Path>,
        tenant: TenantId,
        journal: Arc<dyn TrajectoryLog>,
        sealing: Sealing,
    ) -> Result<Self, DurableStoreInitError> {
        let db = Db::open(path.as_ref(), Access::Create).await?;
        let store = Self::claim_on(db, tenant, journal).await?;
        let sealer = Sealer::spawn(store.unsealed(), sealing);
        Ok(Self {
            sealer: Some(sealer),
            ..store
        })
    }

    /// Migrates the durable store schema on `db` and claims it for `tenant`, sealing nothing.
    async fn claim_on(
        db: Db,
        tenant: TenantId,
        journal: Arc<dyn TrajectoryLog>,
    ) -> Result<Self, DurableStoreInitError> {
        db.migrate(&MIGRATOR).await?;
        let id = claim(db.pool(), &tenant).await?;
        Ok(Self {
            db,
            tenant,
            id,
            journal,
            sealer: None,
        })
    }

    /// Returns how sealing has gone lately, or `None` when this store does not seal.
    pub fn seal_health(&self) -> Option<SealHealth> {
        self.sealer.as_ref().map(Sealer::health)
    }

    /// Returns this store without its seal schedule, sharing its pool and journal store: what
    /// the schedule itself seals through, so that dropping the store ends the schedule.
    fn unsealed(&self) -> Self {
        Self {
            db: self.db.clone(),
            tenant: self.tenant.clone(),
            id: self.id.clone(),
            journal: self.journal.clone(),
            sealer: None,
        }
    }

    /// Open a durable store a runner already made, for a tool that has no tenant of its own
    /// (the debugger): no file is created, no migration runs, nothing is written at
    /// open, and the tenant and id are the ones `store_meta` records — never ones the
    /// tool's user types.
    ///
    /// A claimed durable store is opened on a pool that can write, because the debugger answers
    /// a parked task through it. A durable store no current runner has claimed is opened on a
    /// read-only connection as a [`ReadOnlyDurableStore`].
    pub async fn open_existing(
        path: impl AsRef<Path>,
        journal: Arc<dyn TrajectoryLog>,
    ) -> Result<ExistingDurableStore, DurableStoreInitError> {
        let path = path.as_ref();
        let probe = Db::open(path, Access::ReadOnly).await?;

        match recorded_identity(probe.pool()).await? {
            Some(StoreIdentity { tenant, id }) => {
                probe.pool().close().await;
                Ok(ExistingDurableStore::Claimed(Self {
                    db: Db::open(path, Access::Existing).await?,
                    tenant,
                    id,
                    journal,
                    sealer: None,
                }))
            }
            None => Ok(ExistingDurableStore::Unclaimed(ReadOnlyDurableStore {
                db: probe,
                journal,
                why: Unclaimed {
                    path: path.to_path_buf(),
                },
            })),
        }
    }

    /// Opens a durable store in an in-memory SQLite database, its sealed journals in memory too.
    /// It seals nothing on its own: it is for tests, which seal when they ask.
    ///
    /// Its database hands out one connection, because each connection to an in-memory
    /// database is a separate, empty database.
    pub async fn in_memory(tenant: TenantId) -> Result<Self, DurableStoreInitError> {
        let db = Db::open_in_memory().await?;
        Self::claim_on(db, tenant, Arc::new(MemTrajectoryLog::new())).await
    }

    /// Begins a transaction that reads, then writes. `IMMEDIATE` takes the write lock up
    /// front: a deferred one that read first fails when it goes to write — with
    /// `SQLITE_BUSY` while another writer holds the lock, `SQLITE_BUSY_SNAPSHOT` when one
    /// committed in between — and `busy_timeout` waits out neither.
    async fn begin_immediate(&self) -> Result<ImmediateTx, DurableStoreError> {
        Ok(ImmediateTx(
            self.db.pool().begin_with("BEGIN IMMEDIATE").await?,
        ))
    }

    /// Pre-checks S(T) for a batch and returns the rows to stage if it passes, in one
    /// read snapshot, so the rows are exactly the journals the facts describe.
    async fn plan_seal(
        &self,
        task_ids: &[TaskId],
        now: Millis,
    ) -> Result<SealPlan, DurableStoreError> {
        let mut tx = self.db.pool().begin().await?;
        let mut journals = Vec::with_capacity(task_ids.len());
        for id in task_ids {
            journals.push((
                seal_facts(&mut tx, id).await?,
                journal_rows(&mut tx, id).await?,
            ));
        }
        SealPlan::of(journals, now)
    }

    /// Registers the file name before the file exists, so a crash can only leave a
    /// named file.
    async fn register_staged(
        &self,
        name: &JournalFileName,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        sqlx::query("INSERT INTO journal_file (name, state, created_at) VALUES (?, 'staged', ?)")
            .bind(name.as_str())
            .bind(now)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Makes a staged file live in the seal's one commit; any failed re-check drops the
    /// transaction, so nothing visible changes.
    async fn commit_seal(
        &self,
        pending: &[Pending],
        now: Millis,
        staged: &StagedFile,
    ) -> Result<Vec<SealedJournal>, DurableStoreError> {
        let file = staged.name.as_str();
        let mut tx = self.begin_immediate().await?;

        let mut facts = Vec::with_capacity(pending.len());
        for p in pending {
            facts.push(seal_facts(&mut tx, &p.task_id).await?);
        }
        let file_staged: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM journal_file WHERE name = ? AND state = 'staged')",
        )
        .bind(file)
        .fetch_one(&mut *tx)
        .await?;
        let entries = recheck_batch(pending, facts, file_staged, staged, now)?;

        sqlx::query("UPDATE journal_file SET state = 'live' WHERE name = ?")
            .bind(file)
            .execute(&mut *tx)
            .await?;
        for (task_id, entry) in &entries {
            index_and_delete(&mut tx, task_id, entry).await?;
        }
        tx.commit().await?;

        Ok(sealed_journals(&entries))
    }

    /// Stops sealing after one last pass, then closes the pool, waiting for in-flight
    /// connections, so a later open of the same file sees every write; after this the durable
    /// store's methods return `Err`.
    ///
    /// # Errors
    ///
    /// Returns [`StopSealingError`] when that last pass failed or the schedule was lost; the
    /// pool is closed all the same, and what was not sealed is sealed on a later open.
    pub async fn close(&self) -> Result<(), StopSealingError> {
        let stopped = match &self.sealer {
            Some(sealer) => sealer.stop().await,
            None => Ok(()),
        };
        self.db.pool().close().await;
        stopped
    }
}

/// The `tasks` columns [`row_to_task`] decodes.
const TASK_COLUMNS: &str = "id, job_id, agent_key, intent, judge, status, lease_owner, lease_expires, \
     lease_token, attempt, continuation, wait, wake_pending, reply, result, \
     holds, interrupted_at, interrupted_times";

/// Decodes one `tasks` row selected by [`TASK_COLUMNS`]. A bad `status` or JSON column is a
/// typed [`DurableStoreError`].
fn row_to_task(r: &sqlx::sqlite::SqliteRow) -> Result<Task, DurableStoreError> {
    let id: TaskId = stored_id("id", r.try_get("id")?)?;
    let job_id: JobId = stored_id("job_id", r.try_get("job_id")?)?;
    let agent_key: AgentKey = stored_id("agent_key", r.try_get("agent_key")?)?;
    let intent_str: String = r.try_get("intent")?;
    let judge: Option<AgentKey> = r
        .try_get::<Option<String>, _>("judge")?
        .map(|text| stored_id("judge", text))
        .transpose()?;
    let status_str: String = r.try_get("status")?;
    let lease_owner: Option<String> = r.try_get("lease_owner")?;
    let lease_expires: Option<i64> = r.try_get("lease_expires")?;
    let lease_token: Option<String> = r.try_get("lease_token")?;
    let attempt: i64 = r.try_get("attempt")?;
    let continuation: Option<TaskId> = r
        .try_get::<Option<String>, _>("continuation")?
        .map(|text| stored_id("continuation", text))
        .transpose()?;
    let wait_str: Option<String> = r.try_get("wait")?;
    let holds: i64 = r.try_get("holds")?;
    let interrupted_at: Option<i64> = r.try_get("interrupted_at")?;
    let interrupted_times: Option<i64> = r.try_get("interrupted_times")?;
    let wake_pending: i64 = r.try_get("wake_pending")?;
    let reply_str: Option<String> = r.try_get("reply")?;
    let result_str: Option<String> = r.try_get("result")?;

    let status = Status::parse(&status_str).ok_or_else(|| DurableStoreError::CorruptStatus {
        id: id.clone(),
        status: status_str.clone(),
    })?;
    let intent: Value = serde_json::from_str(&intent_str)?;
    let lease = match (lease_owner, lease_expires) {
        (Some(owner), Some(expires_at)) => Some(Lease {
            owner,
            expires_at,
            token: lease_token_of(&id, lease_token)?,
        }),
        _ => None,
    };
    let reply: Option<AgentReply> = match reply_str {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    };
    let wait: Option<Wait> = match wait_str {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    };
    let result: Option<Value> = match result_str {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    };
    let interrupted = interruptions(&id, interrupted_at, interrupted_times)?;
    let holds = holds_of(&id, holds)?;
    Ok(Task {
        spec: TaskSpec {
            id,
            job_id,
            agent_key,
            intent,
            judge,
            continuation,
        },
        status,
        lease,
        attempt: attempt as u32,
        wait,
        holds,
        interrupted,
        wake_pending: wake_pending != 0,
        reply,
        result,
    })
}

/// Reads a held lease's token. Every claim writes one beside the owner and expiry, so a lease
/// without a readable token is not something this durable store wrote and is an error, not
/// "no lease".
fn lease_token_of(id: &TaskId, token: Option<String>) -> Result<LeaseToken, DurableStoreError> {
    token
        .as_deref()
        .and_then(|t| Uuid::parse_str(t).ok())
        .map(LeaseToken::from)
        .ok_or_else(|| DurableStoreError::CorruptLeaseToken {
            id: id.clone(),
            token,
        })
}

/// Reads the `holds` count, which the schema keeps at or above zero; a negative one is a
/// corrupt row, not "holds nothing".
fn holds_of(id: &TaskId, holds: i64) -> Result<u64, DurableStoreError> {
    u64::try_from(holds).map_err(|_| DurableStoreError::CorruptHolds {
        id: id.clone(),
        holds,
    })
}

/// Spells a lease token as the `lease_token` column holds it, the hyphenated UUID that
/// every write and comparison uses.
fn stored(token: LeaseToken) -> String {
    token.as_uuid().to_string()
}

/// Reads the two `interrupted_*` columns, which are written together. A row holding one
/// without the other, or a negative value, is not something this durable store writes and is an
/// error, not "never interrupted".
fn interruptions(
    id: &TaskId,
    at: Option<i64>,
    times: Option<i64>,
) -> Result<Option<InterruptionStreak>, DurableStoreError> {
    let corrupt = || DurableStoreError::CorruptInterruptions {
        id: id.clone(),
        at,
        times,
    };
    match (at, times) {
        (None, None) => Ok(None),
        (Some(a), Some(t)) => Ok(Some(InterruptionStreak {
            at: u64::try_from(a).map_err(|_| corrupt())?,
            times: u32::try_from(t).map_err(|_| corrupt())?,
        })),
        _ => Err(corrupt()),
    }
}

/// Counts the journal entries (`delta` events) of the row being updated: its position
/// for [`InterruptionStreak`]. Indexed by `idx_trajectory_task`.
const JOURNAL_LEN: &str = "(SELECT COUNT(*) FROM trajectory j \
     WHERE j.task_id = tasks.id AND json_extract(j.kind, '$.kind') = 'delta')";

/// Returns the `SET` half of a reclaim: back to `ready` with one more interruption
/// counted, in one statement, so a crash cannot lose the count.
///
/// [`InterruptionStreak::after`] in SQL: the same position extends the streak, anything else
/// starts a new one. Every expression reads the row as it was BEFORE the update, which
/// is what lets `interrupted_times` compare against the old `interrupted_at`.
fn reclaim_set() -> String {
    format!(
        "status='ready', lease_owner=NULL, lease_expires=NULL, lease_token=NULL,
         interrupted_times = CASE WHEN interrupted_at = {JOURNAL_LEN}
                                  THEN interrupted_times + 1 ELSE 1 END,
         interrupted_at = {JOURNAL_LEN}"
    )
}

// ── primitive ops, each run on a pooled connection or inside a caller's transaction ──────

/// A transaction begun `IMMEDIATE` ([`SqliteDurableStore::begin_immediate`], its only maker). A
/// write that reads the row before it changes it takes one, so no other writer can change
/// the row between the read and the write.
struct ImmediateTx(sqlx::Transaction<'static, sqlx::Sqlite>);

impl ImmediateTx {
    async fn commit(self) -> Result<(), DurableStoreError> {
        Ok(self.0.commit().await?)
    }
}

impl std::ops::Deref for ImmediateTx {
    type Target = SqliteConnection;
    fn deref(&self) -> &SqliteConnection {
        &self.0
    }
}

impl std::ops::DerefMut for ImmediateTx {
    fn deref_mut(&mut self) -> &mut SqliteConnection {
        &mut self.0
    }
}

/// Inserts one `Ready` task with its `Enqueued` event; an existing id is left untouched and
/// gets no event.
async fn op_enqueue(
    conn: &mut SqliteConnection,
    spec: &TaskSpec,
    now: Millis,
) -> Result<(), DurableStoreError> {
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO tasks
            (id, job_id, agent_key, intent, judge, status, attempt, continuation, wake_pending)
         VALUES (?, ?, ?, ?, ?, 'ready', 0, ?, 0)",
    )
    .bind(spec.id.as_str())
    .bind(spec.job_id.as_str())
    .bind(spec.agent_key.as_str())
    .bind(spec.intent.to_string())
    .bind(spec.judge.as_ref().map(AgentKey::as_str))
    .bind(spec.continuation.as_ref().map(TaskId::as_str))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    match inserted {
        0 => Ok(()),
        _ => op_append_trajectory(conn, &spec.id, &EventKind::Enqueued, now).await,
    }
}

/// Leases one ready task whose `agent_key` is not in `excluded` to `owner` under a fresh
/// claim and appends its `Running` event. The caller's `IMMEDIATE` transaction makes the
/// pick and the flip one step.
async fn op_claim(
    tx: &mut ImmediateTx,
    owner: &str,
    now: Millis,
    ttl: Millis,
    excluded: &[AgentKey],
) -> Result<Option<ClaimedTask>, DurableStoreError> {
    let placeholders = std::iter::repeat_n("?", excluded.len())
        .collect::<Vec<_>>()
        .join(", ");
    let select_sql = format!(
        "SELECT id FROM tasks WHERE status='ready' AND agent_key NOT IN ({placeholders}) LIMIT 1"
    );
    let mut pick = sqlx::query_scalar::<_, String>(AssertSqlSafe(select_sql.as_str()));
    for k in excluded {
        pick = pick.bind(k.as_str());
    }
    let Some(id) = pick.fetch_optional(&mut **tx).await? else {
        return Ok(None);
    };
    let id: TaskId = stored_id("id", id)?;
    let token = LeaseToken::from(Uuid::new_v4());
    sqlx::query(
        "UPDATE tasks SET status='running', lease_owner=?, lease_expires=?, lease_token=?,
                          wake_pending=0
         WHERE id=?",
    )
    .bind(owner)
    .bind(now + ttl)
    .bind(stored(token))
    .bind(id.as_str())
    .execute(&mut **tx)
    .await?;
    op_append_trajectory(tx, &id, &EventKind::Running, now).await?;
    let task = op_get(tx, &id)
        .await?
        .ok_or_else(|| DurableStoreError::NoSuchTask(id.clone()))?;
    Ok(Some(ClaimedTask::new(&task, token)))
}

/// Reads task `id`'s row and refuses an ending [`admit`] does not let through.
async fn admitted(
    tx: &mut ImmediateTx,
    id: &TaskId,
    writer: Writer,
) -> Result<Task, DurableStoreError> {
    let t = op_get(tx, id)
        .await?
        .ok_or_else(|| DurableStoreError::NoSuchTask(id.clone()))?;
    admit(&t, writer)?;
    Ok(t)
}

/// Writes the result, flips the task `Done` and appends its `Done` event. The lease stays:
/// the runner holding it is not done until it winds the task up and releases it.
async fn op_commit(
    tx: &mut ImmediateTx,
    id: &TaskId,
    writer: Writer,
    result: &Value,
    now: Millis,
) -> Result<(), DurableStoreError> {
    let t = admitted(tx, id, writer).await?;
    sqlx::query("UPDATE tasks SET result=?, status='done' WHERE id=?")
        .bind(result.to_string())
        .bind(id.as_str())
        .execute(&mut **tx)
        .await?;
    op_append_trajectory(tx, id, &EventKind::Done, now).await?;
    op_wake_continuation(tx, &t, now).await
}

/// Counts an attempt and retries or ends the task as [`failed_attempt`] decides. A retry
/// clears the lease, since a `ready` row holds none, and appends nothing; a terminal
/// failure keeps the lease for its holder to release and appends its `Failed` event.
async fn op_fail(
    tx: &mut ImmediateTx,
    id: &TaskId,
    writer: Writer,
    failure_limit: u32,
    reason: JournaledReason,
    now: Millis,
) -> Result<FailedAttempt, DurableStoreError> {
    let t = admitted(tx, id, writer).await?;
    let attempts = t.attempt + 1;
    let failed = failed_attempt(attempts, failure_limit);
    match failed {
        FailedAttempt::Requeued => {
            sqlx::query(
                "UPDATE tasks
                    SET attempt=?, status='ready',
                        lease_owner=NULL, lease_expires=NULL, lease_token=NULL
                 WHERE id=?",
            )
            .bind(attempts)
            .bind(id.as_str())
            .execute(&mut **tx)
            .await?;
        }
        FailedAttempt::Terminal => {
            sqlx::query("UPDATE tasks SET attempt=?, status='failed' WHERE id=?")
                .bind(attempts)
                .bind(id.as_str())
                .execute(&mut **tx)
                .await?;
            op_append_trajectory(tx, id, &EventKind::Failed { reason }, now).await?;
            op_wake_continuation(tx, &t, now).await?;
        }
    }
    Ok(failed)
}

/// Fails a task for good without counting an attempt and appends its `Failed` event.
/// Keeps the lease: its holder releases it once it has wound the task up.
async fn op_fail_terminal(
    tx: &mut ImmediateTx,
    id: &TaskId,
    writer: Writer,
    reason: JournaledReason,
    now: Millis,
) -> Result<(), DurableStoreError> {
    let t = admitted(tx, id, writer).await?;
    sqlx::query("UPDATE tasks SET status='failed' WHERE id=?")
        .bind(id.as_str())
        .execute(&mut **tx)
        .await?;
    op_append_trajectory(tx, id, &EventKind::Failed { reason }, now).await?;
    op_wake_continuation(tx, &t, now).await
}

/// Parks a running task as [`park_of`] decides and appends its events.
async fn op_park(
    tx: &mut ImmediateTx,
    id: &TaskId,
    writer: Writer,
    wait: &Wait,
    now: Millis,
) -> Result<(), DurableStoreError> {
    let t = admitted(tx, id, writer).await?;
    let conn: &mut SqliteConnection = tx;
    let parked = EventKind::Parked { wait: wait.clone() };
    match park_of(t.wake_pending) {
        Park::Wait => {
            sqlx::query(
                "UPDATE tasks
                    SET status='parked', wait=?, wake_at=?,
                        lease_owner=NULL, lease_expires=NULL, lease_token=NULL
                 WHERE id=?",
            )
            .bind(serde_json::to_string(wait)?)
            .bind(wait.until())
            .bind(id.as_str())
            .execute(&mut *conn)
            .await?;
            op_append_trajectory(conn, id, &parked, now).await
        }
        Park::Bounce => {
            op_append_trajectory(conn, id, &parked, now).await?;
            op_wake_into_ready(conn, id, None, now).await
        }
    }
}

/// Makes task `id` `Ready` for a new run and appends the `Woken` event that records why,
/// carrying the human `reply` when one woke it, in the caller's transaction. Every write
/// that queues a new run (a wake, a reply, a due deadline, a bounced park) goes through
/// here, so none of them leaves the event out. It clears what only a park or a run had:
/// the question, the deadline, a pending wake and the lease.
async fn op_wake_into_ready(
    conn: &mut SqliteConnection,
    id: &TaskId,
    reply: Option<AgentReply>,
    now: Millis,
) -> Result<(), DurableStoreError> {
    let stored = match &reply {
        Some(r) => Some(serde_json::to_string(r)?),
        None => None,
    };
    sqlx::query(
        "UPDATE tasks
            SET status='ready', wait=NULL, wake_at=NULL, wake_pending=0,
                lease_owner=NULL, lease_expires=NULL, lease_token=NULL,
                reply=COALESCE(?, reply)
         WHERE id=?",
    )
    .bind(stored)
    .bind(id.as_str())
    .execute(&mut *conn)
    .await?;
    op_append_trajectory(conn, id, &EventKind::Woken { reply }, now).await
}

fn parse_status(id: &TaskId, stored: String) -> Result<Status, DurableStoreError> {
    Status::parse(&stored).ok_or_else(|| DurableStoreError::CorruptStatus {
        id: id.clone(),
        status: stored,
    })
}

/// Returns task `id`'s status; a missing row is [`DurableStoreError::NoSuchTask`].
async fn op_status(conn: &mut SqliteConnection, id: &TaskId) -> Result<Status, DurableStoreError> {
    let stored: Option<String> = sqlx::query_scalar("SELECT status FROM tasks WHERE id=?")
        .bind(id.as_str())
        .fetch_optional(&mut *conn)
        .await?;
    let stored = stored.ok_or_else(|| DurableStoreError::NoSuchTask(id.clone()))?;
    parse_status(id, stored)
}

/// Wakes a task as [`wake_of`] decides and appends its `Woken` event when it resumes.
async fn op_wake(tx: &mut ImmediateTx, id: &TaskId, now: Millis) -> Result<(), DurableStoreError> {
    let conn: &mut SqliteConnection = tx;
    let status = op_status(conn, id).await?;
    op_wake_from(conn, id, status, now).await
}

/// Wakes the continuation of `ended`, a task whose ending was just written in `tx`. A
/// continuation that has itself ended has nobody left to wake and is left as it is.
async fn op_wake_continuation(
    tx: &mut ImmediateTx,
    ended: &Task,
    now: Millis,
) -> Result<(), DurableStoreError> {
    let Some(c) = &ended.spec.continuation else {
        return Ok(());
    };
    let conn: &mut SqliteConnection = tx;
    let status = op_status(conn, c).await?;
    match wake_of(status) {
        Wake::Ended => Ok(()),
        _ => op_wake_from(conn, c, status, now).await,
    }
}

/// Writes what a wake does to task `id`, found in `status`.
async fn op_wake_from(
    conn: &mut SqliteConnection,
    id: &TaskId,
    status: Status,
    now: Millis,
) -> Result<(), DurableStoreError> {
    match wake_of(status) {
        Wake::Absorb => Ok(()),
        Wake::Defer => {
            sqlx::query("UPDATE tasks SET wake_pending=1 WHERE id=?")
                .bind(id.as_str())
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        Wake::Resume => op_wake_into_ready(conn, id, None, now).await,
        Wake::Ended => Err(DurableStoreError::Superseded {
            id: id.clone(),
            status,
        }),
    }
}

/// Stores a reply on a parked task whose wait it answers ([`answers`]), makes it `Ready` and
/// appends the `Woken` event carrying the reply.
async fn op_resume(
    tx: &mut ImmediateTx,
    id: &TaskId,
    reply: &AgentReply,
    now: Millis,
) -> Result<(), DurableStoreError> {
    let conn: &mut SqliteConnection = tx;
    let status = op_status(conn, id).await?;
    match reply_of(status) {
        Reply::Waiting => {
            let stored: Option<String> = sqlx::query_scalar("SELECT wait FROM tasks WHERE id=?")
                .bind(id.as_str())
                .fetch_one(&mut *conn)
                .await?;
            let wait: Option<Wait> = match stored {
                Some(s) => Some(serde_json::from_str(&s)?),
                None => None,
            };
            let Some(wait) = wait else {
                return Err(DurableStoreError::CorruptStatus {
                    id: id.clone(),
                    status: "parked with no wait".into(),
                });
            };
            if !answers(&wait, reply) {
                return Err(DurableStoreError::WrongReply {
                    id: id.clone(),
                    wait: Box::new(wait),
                    reply: Box::new(reply.clone()),
                });
            }
        }
        Reply::NotWaiting(status) => {
            return Err(DurableStoreError::NotParked {
                id: id.clone(),
                status,
            });
        }
        Reply::Ended => {
            return Err(DurableStoreError::Superseded {
                id: id.clone(),
                status,
            });
        }
    }
    op_wake_into_ready(conn, id, Some(reply.clone()), now).await
}

/// Appends one `Delta` event from `writer`, refused as [`admit_append`] decides.
async fn op_append_delta(
    tx: &mut ImmediateTx,
    writer: Writer,
    task_id: &TaskId,
    payload: Value,
    holds_delta: i64,
    at: Millis,
) -> Result<(), DurableStoreError> {
    let row = op_get(tx, task_id).await?;
    admit_append(task_id, row.as_ref(), writer)?;
    op_append_trajectory(tx, task_id, &EventKind::Delta { payload }, at).await?;
    if holds_delta != 0 {
        // A stray release must not leave the count negative.
        sqlx::query("UPDATE tasks SET holds = MAX(0, holds + ?) WHERE id = ?")
            .bind(holds_delta)
            .bind(task_id.as_str())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Appends one trajectory event. An append to a sealed journal is refused in the same
/// statement, so the refusal rolls back its whole transaction.
async fn op_append_trajectory(
    conn: &mut SqliteConnection,
    task_id: &TaskId,
    kind: &EventKind,
    at: Millis,
) -> Result<(), DurableStoreError> {
    let n = sqlx::query(
        "INSERT INTO trajectory (task_id, kind, at)
         SELECT ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM journal_index WHERE task_id = ?)",
    )
    .bind(task_id.as_str())
    .bind(serde_json::to_string(kind)?)
    .bind(at)
    .bind(task_id.as_str())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(DurableStoreError::JournalSealed {
            task_id: task_id.clone(),
        });
    }
    Ok(())
}

/// Points the index at a sealed task's row group and deletes its live rows.
async fn index_and_delete(
    conn: &mut SqliteConnection,
    task_id: &TaskId,
    entry: &IndexEntry,
) -> Result<(), DurableStoreError> {
    sqlx::query(
        "INSERT INTO journal_index (task_id, file, row_group, first_seq, last_seq)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(task_id.as_str())
    .bind(entry.file.as_str())
    .bind(entry.row_group)
    .bind(seq_column(entry.first_seq)?)
    .bind(seq_column(entry.last_seq)?)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM trajectory WHERE task_id = ?")
        .bind(task_id.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Reads what S(T) is judged on, in `conn`'s snapshot.
async fn seal_facts(
    conn: &mut SqliteConnection,
    task_id: &TaskId,
) -> Result<SealFacts, DurableStoreError> {
    let task = op_get(conn, task_id)
        .await?
        .ok_or_else(|| DurableStoreError::NoSuchTask(task_id.clone()))?;
    let parent = match &task.spec.continuation {
        Some(parent) => op_get(conn, parent).await?.map(|p| p.status),
        None => None,
    };
    let open_children: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks WHERE continuation = ? AND status NOT IN ('done','failed')",
    )
    .bind(task_id.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let sealed: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM journal_index WHERE task_id = ?)")
            .bind(task_id.as_str())
            .fetch_one(&mut *conn)
            .await?;
    let high_water: Option<i64> =
        sqlx::query_scalar("SELECT MAX(seq) FROM trajectory WHERE task_id = ?")
            .bind(task_id.as_str())
            .fetch_one(&mut *conn)
            .await?;
    Ok(SealFacts {
        task,
        parent,
        open_children: open_children as u64,
        sealed,
        high_water: high_water.map(|s| stored_seq(task_id, s)).transpose()?,
    })
}

/// Fetches one task row, in the caller's transaction.
async fn op_get(
    conn: &mut SqliteConnection,
    id: &TaskId,
) -> Result<Option<Task>, DurableStoreError> {
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT {TASK_COLUMNS} FROM tasks WHERE id=?"
    )))
    .bind(id.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| row_to_task(&r)).transpose()
}

async fn op_record_effect(
    conn: &mut SqliteConnection,
    key: &str,
    result: &Value,
) -> Result<(), DurableStoreError> {
    sqlx::query("INSERT OR IGNORE INTO effect_ledger (key, result) VALUES (?, ?)")
        .bind(key)
        .bind(result.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn op_effect_result(
    conn: &mut SqliteConnection,
    key: &str,
) -> Result<Option<Value>, DurableStoreError> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT result FROM effect_ledger WHERE key = ?")
            .bind(key)
            .fetch_optional(&mut *conn)
            .await?;
    // A stored result that does not decode is an error, not a miss: read as `None`, it
    // would fire the effect a second time.
    match stored {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

// Both durable stores on a durable store file read through the same free functions below.
#[async_trait]
impl DurableStoreRead for SqliteDurableStore {
    async fn get(&self, id: &TaskId) -> Result<Option<Task>, DurableStoreError> {
        read_get(self.db.pool(), id).await
    }
    async fn tasks(&self) -> Result<Vec<Task>, DurableStoreError> {
        read_tasks(self.db.pool()).await
    }
    async fn open_tasks(&self, job: &JobId) -> Result<Vec<Task>, DurableStoreError> {
        read_open_tasks(self.db.pool(), job).await
    }
    async fn since(&self, cursor: Cursor) -> Result<Vec<Event>, DurableStoreError> {
        read_since(self.db.pool(), &*self.journal, cursor).await
    }
    async fn trajectory(&self, task_id: &TaskId) -> Result<Vec<Event>, DurableStoreError> {
        read_trajectory(self.db.pool(), &*self.journal, task_id).await
    }
}

#[async_trait]
impl DurableStoreRead for ReadOnlyDurableStore {
    async fn get(&self, id: &TaskId) -> Result<Option<Task>, DurableStoreError> {
        read_get(self.db.pool(), id).await
    }
    async fn tasks(&self) -> Result<Vec<Task>, DurableStoreError> {
        read_tasks(self.db.pool()).await
    }
    async fn open_tasks(&self, job: &JobId) -> Result<Vec<Task>, DurableStoreError> {
        read_open_tasks(self.db.pool(), job).await
    }
    async fn since(&self, cursor: Cursor) -> Result<Vec<Event>, DurableStoreError> {
        read_since(self.db.pool(), &*self.journal, cursor).await
    }
    async fn trajectory(&self, task_id: &TaskId) -> Result<Vec<Event>, DurableStoreError> {
        read_trajectory(self.db.pool(), &*self.journal, task_id).await
    }
}

#[async_trait]
impl FiberStore for SqliteDurableStore {
    async fn commit(
        &self,
        id: &TaskId,
        writer: Writer,
        result: Value,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        op_commit(&mut tx, id, writer, &result, now).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn fail(
        &self,
        id: &TaskId,
        writer: Writer,
        failure_limit: u32,
        reason: JournaledReason,
        now: Millis,
    ) -> Result<FailedAttempt, DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        let failed = op_fail(&mut tx, id, writer, failure_limit, reason, now).await?;
        tx.commit().await?;
        Ok(failed)
    }

    async fn enqueue_all(&self, specs: &[TaskSpec], now: Millis) -> Result<(), DurableStoreError> {
        let mut tx = self.db.pool().begin().await?;
        for spec in specs {
            op_enqueue(&mut tx, spec, now).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn append_delta(
        &self,
        writer: Writer,
        task_id: &TaskId,
        payload: Value,
        holds_delta: i64,
        at: Millis,
    ) -> Result<(), DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        op_append_delta(&mut tx, writer, task_id, payload, holds_delta, at).await?;
        tx.commit().await
    }
}

#[async_trait]
impl DurableStore for SqliteDurableStore {
    fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    fn id(&self) -> &DurableStoreId {
        &self.id
    }

    async fn enqueue(&self, spec: &TaskSpec, now: Millis) -> Result<(), DurableStoreError> {
        self.enqueue_all(std::slice::from_ref(spec), now).await
    }

    async fn claim_ready(
        &self,
        owner: &str,
        now: Millis,
        ttl: Millis,
    ) -> Result<Option<ClaimedTask>, DurableStoreError> {
        self.claim_ready_excluding(owner, now, ttl, &[]).await
    }

    async fn claim_ready_excluding(
        &self,
        owner: &str,
        now: Millis,
        ttl: Millis,
        excluded: &[AgentKey],
    ) -> Result<Option<ClaimedTask>, DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        let claimed = op_claim(&mut tx, owner, now, ttl, excluded).await?;
        tx.commit().await?;
        Ok(claimed)
    }

    async fn fail_terminal(
        &self,
        id: &TaskId,
        writer: Writer,
        reason: JournaledReason,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        op_fail_terminal(&mut tx, id, writer, reason, now).await?;
        tx.commit().await
    }

    async fn park(
        &self,
        id: &TaskId,
        writer: Writer,
        wait: &Wait,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        op_park(&mut tx, id, writer, wait, now).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn abandoned_holds(
        &self,
        limit: usize,
        now: Millis,
    ) -> Result<Vec<TaskId>, DurableStoreError> {
        // `idx_tasks_holds` is partial on `holds > 0`, so this reads only the tasks that
        // hold something now; the decision is made over them here so that both durable stores
        // make it with the same function.
        let rows: Vec<(String, String, i64, Option<String>, Option<Millis>)> = sqlx::query_as(
            "SELECT id, status, holds, lease_owner, lease_expires FROM tasks
                 WHERE holds > 0 ORDER BY id",
        )
        .fetch_all(self.db.pool())
        .await?;
        let mut out = Vec::new();
        for (id, status, holds, owner, expires_at) in rows {
            if out.len() == limit {
                break;
            }
            let id: TaskId = stored_id("id", id)?;
            let status = parse_status(&id, status)?;
            let holds = holds_of(&id, holds)?;
            // A lease is the owner and the expiry together, as `row_to_task` reads it.
            let lease_expires_at = owner.and(expires_at);
            if crate::durable::abandoned_hold(status, holds, lease_expires_at, now) {
                out.push(id);
            }
        }
        Ok(out)
    }

    async fn wake_due(&self, now: Millis) -> Result<usize, DurableStoreError> {
        // Selected in id order, so one sweep's events land in the same order on every
        // durable store: `RETURNING` from an `UPDATE` promises no order.
        let mut tx = self.begin_immediate().await?;
        let due: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM tasks
             WHERE status='parked' AND wake_at IS NOT NULL AND wake_at <= ?
             ORDER BY id",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        for id in due.iter().cloned().map(|id| stored_id("id", id)) {
            let id: TaskId = id?;
            // Nobody answered: the run this queues reads a `Woken` without a reply.
            op_wake_into_ready(&mut tx, &id, None, now).await?;
        }
        tx.commit().await?;
        Ok(due.len())
    }

    async fn cancel_descendants(
        &self,
        parent: &TaskId,
        reason: &str,
        now: Millis,
    ) -> Result<crate::durable::Cancelled, DurableStoreError> {
        // One transaction, so a concurrent claim cannot start a child between reading it
        // as `ready` and writing it `failed`; IMMEDIATE because it reads, then writes.
        let mut tx = self.begin_immediate().await?;
        let mut out = crate::durable::Cancelled::default();
        let mut frontier = vec![parent.clone()];
        let mut seen: std::collections::HashSet<TaskId> =
            std::collections::HashSet::from([parent.clone()]);

        while let Some(p) = frontier.pop() {
            let kids: Vec<(String, String)> =
                sqlx::query_as("SELECT id, status FROM tasks WHERE continuation = ?")
                    .bind(p.as_str())
                    .fetch_all(&mut *tx)
                    .await?;
            for (id, status) in kids {
                let id: TaskId = stored_id("id", id)?;
                // A `continuation` cycle would otherwise spin here forever. It should
                // not happen; a sweep that hangs on malformed data is not acceptable.
                if !seen.insert(id.clone()) {
                    continue;
                }
                // Whatever this child's own state, walk past it: what IT spawned is
                // just as orphaned, and those grandchildren may not have started.
                frontier.push(id.clone());
                match status.as_str() {
                    // `running` goes too: it cannot be interrupted, but `commit`/`fail`
                    // refuse to write over a terminal row, so its work is discarded
                    // rather than un-doing this cancellation. Its lease stays with the
                    // runner still executing it, which releases it once wound up.
                    "ready" | "parked" | "running" => {
                        sqlx::query(
                            "UPDATE tasks
                                SET status='failed', wait=NULL, wake_at=NULL
                             WHERE id=?",
                        )
                        .bind(id.as_str())
                        .execute(&mut *tx)
                        .await?;
                        op_append_trajectory(
                            &mut tx,
                            &id,
                            &EventKind::Failed {
                                reason: JournaledReason::cancelled(reason),
                            },
                            now,
                        )
                        .await?;
                        if status == "running" {
                            out.running.push(id);
                        } else {
                            out.stopped.push(id);
                        }
                    }
                    _ => {}
                }
            }
        }
        tx.commit().await?;
        Ok(out)
    }

    async fn wake(&self, id: &TaskId, now: Millis) -> Result<(), DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        op_wake(&mut tx, id, now).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn resume(
        &self,
        id: &TaskId,
        reply: &AgentReply,
        now: Millis,
    ) -> Result<(), DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        op_resume(&mut tx, id, reply, now).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn reclaim_expired(&self, now: Millis) -> Result<usize, DurableStoreError> {
        let sql = format!(
            "UPDATE tasks SET {}
             WHERE status='running' AND (lease_expires IS NULL OR lease_expires <= ?)",
            reclaim_set()
        );
        let n = sqlx::query(AssertSqlSafe(sql))
            .bind(now)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        Ok(n as usize)
    }

    async fn reclaim_orphans(
        &self,
        previous: Option<&str>,
        live: &[LeaseToken],
        now: Millis,
    ) -> Result<usize, DurableStoreError> {
        let keep = std::iter::repeat_n("?", live.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mine = if previous.is_some() {
            "lease_owner = ? OR "
        } else {
            ""
        };
        let sql = format!(
            "UPDATE tasks SET {}
             WHERE status='running' AND (lease_token IS NULL OR lease_token NOT IN ({keep}))
               AND ({mine}lease_expires IS NULL OR lease_expires <= ?)",
            reclaim_set()
        );
        let mut q = sqlx::query(AssertSqlSafe(sql));
        for token in live {
            q = q.bind(stored(*token));
        }
        if let Some(owner) = previous {
            q = q.bind(owner);
        }
        let n = q.bind(now).execute(self.db.pool()).await?.rows_affected();
        Ok(n as usize)
    }

    async fn renew_leases(
        &self,
        tokens: &[LeaseToken],
        expires_at: Millis,
    ) -> Result<usize, DurableStoreError> {
        if tokens.is_empty() {
            return Ok(0);
        }
        let which = std::iter::repeat_n("?", tokens.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE tasks SET lease_expires = ? WHERE lease_token IN ({which})");
        let mut q = sqlx::query(AssertSqlSafe(sql)).bind(expires_at);
        for token in tokens {
            q = q.bind(stored(*token));
        }
        Ok(q.execute(self.db.pool()).await?.rows_affected() as usize)
    }

    async fn release_lease(&self, id: &TaskId, token: LeaseToken) -> Result<(), DurableStoreError> {
        sqlx::query(
            "UPDATE tasks SET lease_owner=NULL, lease_expires=NULL, lease_token=NULL
             WHERE id=? AND lease_token=?",
        )
        .bind(id.as_str())
        .bind(stored(token))
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    async fn record_effect(&self, key: &str, result: &Value) -> Result<(), DurableStoreError> {
        let mut conn = self.db.pool().acquire().await?;
        op_record_effect(&mut conn, key, result).await
    }

    async fn effect_result(&self, key: &str) -> Result<Option<Value>, DurableStoreError> {
        let mut conn = self.db.pool().acquire().await?;
        op_effect_result(&mut conn, key).await
    }
}

#[async_trait]
impl SealJournals for SqliteDurableStore {
    async fn seal_candidates(
        &self,
        max_tasks: usize,
        max_bytes: u64,
        now: Millis,
    ) -> Result<Vec<TaskId>, DurableStoreError> {
        // S(T) as one statement, driven from the live journal: a sealed task has no
        // rows left there, so the tasks sealed long ago are never looked at again.
        // The weight counts bytes the way the journal store does (`row_bytes`).
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT t.id,
                    (SELECT SUM(length(CAST(j.task_id AS BLOB)) + length(CAST(j.kind AS BLOB)) + 16)
                     FROM trajectory j WHERE j.task_id = t.id)
             FROM tasks t
             WHERE t.id IN (SELECT DISTINCT task_id FROM trajectory)
               AND t.status IN ('done','failed')
               AND t.holds = 0
               AND (t.lease_expires IS NULL OR t.lease_expires <= ?)
               AND (t.continuation IS NULL OR EXISTS (
                     SELECT 1 FROM tasks p
                     WHERE p.id = t.continuation AND p.status IN ('done','failed')))
               AND NOT EXISTS (
                     SELECT 1 FROM tasks c
                     WHERE c.continuation = t.id AND c.status NOT IN ('done','failed'))
               AND NOT EXISTS (SELECT 1 FROM journal_index x WHERE x.task_id = t.id)
             ORDER BY t.id LIMIT ?",
        )
        .bind(now)
        .bind(max_tasks as i64)
        .fetch_all(self.db.pool())
        .await?;
        let candidates = rows
            .into_iter()
            .map(|(task_id, bytes)| {
                Ok(Candidate {
                    task_id: stored_id("task_id", task_id)?,
                    bytes: bytes as u64,
                })
            })
            .collect::<Result<_, DurableStoreError>>()?;
        Ok(fit(candidates, max_bytes))
    }

    async fn seal_tasks(
        &self,
        task_ids: &[TaskId],
        now: Millis,
    ) -> Result<Vec<SealedJournal>, DurableStoreError> {
        if task_ids.is_empty() {
            return Ok(Vec::new());
        }
        let SealPlan {
            name,
            pending,
            rows,
        } = self.plan_seal(task_ids, now).await?;

        self.register_staged(&name, now).await?;
        let staged = self.journal.stage(&name, rows).await?;

        self.commit_seal(&pending, now, &staged).await
    }

    async fn remove_staged(&self, registered_by: Millis) -> Result<usize, DurableStoreError> {
        let mut tx = self.begin_immediate().await?;
        let names: Vec<String> = sqlx::query_scalar(
            "DELETE FROM journal_file WHERE state = 'staged' AND created_at <= ? RETURNING name",
        )
        .bind(registered_by)
        .fetch_all(&mut *tx)
        .await?;
        let names = names
            .iter()
            .map(|n| JournalFileName::parse(n))
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit().await?;

        remove_files(&*self.journal, &names).await
    }

    async fn remove_unregistered(&self) -> Result<usize, DurableStoreError> {
        // The listing first, the registry after: a file listed has been registered
        // since before it was written, so it cannot be mistaken for a leftover.
        let stored = self.journal.stored().await?;
        let registered: HashSet<String> = sqlx::query_scalar("SELECT name FROM journal_file")
            .fetch_all(self.db.pool())
            .await?
            .into_iter()
            .collect();
        let orphans: Vec<JournalFileName> = stored
            .into_iter()
            .filter(|name| !registered.contains(name.as_str()))
            .collect();
        remove_files(&*self.journal, &orphans).await
    }
}

// The reads below serve both the writable and the read-only durable store.

async fn read_get(pool: &SqlitePool, id: &TaskId) -> Result<Option<Task>, DurableStoreError> {
    op_get(&mut *pool.acquire().await?, id).await
}

async fn read_open_tasks(pool: &SqlitePool, job: &JobId) -> Result<Vec<Task>, DurableStoreError> {
    let rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT {TASK_COLUMNS} FROM tasks WHERE job_id = ? AND status NOT IN ('done','failed')"
    )))
    .bind(job.as_str())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_task).collect()
}

async fn read_tasks(pool: &SqlitePool) -> Result<Vec<Task>, DurableStoreError> {
    let rows = sqlx::query(AssertSqlSafe(format!("SELECT {TASK_COLUMNS} FROM tasks")))
        .fetch_all(pool)
        .await?;
    rows.iter().map(row_to_task).collect()
}

async fn read_since(
    pool: &SqlitePool,
    journal: &dyn TrajectoryLog,
    cursor: Cursor,
) -> Result<Vec<Event>, DurableStoreError> {
    let after = cursor.column();
    // Live rows and the index are read in one snapshot (see `merged_page`); the files
    // are read after it closes, since a live file never changes.
    let mut tx = pool.begin().await?;
    let rows: Vec<TrajectoryRow> = sqlx::query_as(
        "SELECT seq, task_id, kind, at FROM trajectory
         WHERE seq > ? ORDER BY seq LIMIT ?",
    )
    .bind(after)
    .bind(SINCE_PAGE_LIMIT)
    .fetch_all(&mut *tx)
    .await?;
    let sealed: Vec<(String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT task_id, file, row_group, first_seq, last_seq FROM journal_index
         WHERE last_seq > ? ORDER BY first_seq",
    )
    .bind(after)
    .fetch_all(&mut *tx)
    .await?;
    drop(tx);

    let live = rows
        .into_iter()
        .map(|row| event_of(journal_row(row)?))
        .collect::<Result<Vec<_>, _>>()?;
    let sealed = sealed
        .into_iter()
        .map(|(task_id, file, group, first, last)| {
            index_entry(&stored_id("task_id", task_id)?, (file, group, first, last))
        })
        .collect::<Result<Vec<_>, _>>()?;
    merged_page(live, sealed, cursor, SINCE_PAGE_LIMIT as usize, journal).await
}

async fn read_trajectory(
    pool: &SqlitePool,
    journal: &dyn TrajectoryLog,
    task_id: &TaskId,
) -> Result<Vec<Event>, DurableStoreError> {
    // No page limit: a task's journal is bounded by its own turns.
    let mut conn = pool.acquire().await?;
    read_journal_raw(&mut conn, journal, task_id)
        .await?
        .into_iter()
        .map(event_of)
        .collect()
}

/// A `trajectory` row as stored: `(seq, task_id, kind, at)`.
type TrajectoryRow = (i64, String, String, i64);

fn journal_row(
    (seq, task_id, payload, at): TrajectoryRow,
) -> Result<JournalRow, DurableStoreError> {
    let id: TaskId = stored_id("task_id", task_id)?;
    Ok(JournalRow {
        seq: stored_seq(&id, seq)?,
        task_id: id.into_inner(),
        at,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use serde_json::json;
    use sqlx::sqlite::SqliteConnectOptions;

    fn tenant() -> TenantId {
        TenantId::new("tenant-test")
    }

    /// Returns a schedule that never comes round within a test, so only `close` seals, by a
    /// clock stopped at 0 like the times these tests write.
    fn sealing() -> Sealing {
        Sealing {
            policy: crate::durable::JournalSealPolicy {
                every: Duration::from_secs(3_600),
                ..Default::default()
            },
            clock: Arc::new(|| 0),
            rt: tokio::runtime::Handle::current(),
        }
    }

    fn spec(id: &TaskId) -> TaskSpec {
        TaskSpec::new(
            id.clone(),
            JobId::new("job"),
            AgentKey::new("agent"),
            json!(id),
        )
    }

    #[tokio::test]
    async fn persists_across_reopen() {
        let path = std::env::temp_dir().join(format!("tars-rt2-sq-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let b = SqliteDurableStore::open(
                &path,
                tenant(),
                Arc::new(MemTrajectoryLog::new()),
                sealing(),
            )
            .await
            .unwrap();
            b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
            let claim = b.claim_ready("r", 0, 1000).await.unwrap().unwrap().token;
            b.commit(
                &TaskId::new("a"),
                Writer::Claim(claim),
                json!({ "summary": "A" }),
                0,
            )
            .await
            .unwrap();
            // Close the pool so WAL is checkpointed before the reopen.
            b.db.pool().close().await;
        }
        let b = SqliteDurableStore::open(
            &path,
            tenant(),
            Arc::new(MemTrajectoryLog::new()),
            sealing(),
        )
        .await
        .unwrap();
        let t = b.get(&TaskId::new("a")).await.unwrap().unwrap();
        assert_eq!(t.status, Status::Done);
        assert_eq!(t.result.unwrap()["summary"], "A");
        b.db.pool().close().await;
        let _ = std::fs::remove_file(&path);
    }

    /// A wake whose `Woken` append is refused leaves the task parked, so no task is made
    /// `Ready` without the event `woken_with` reads.
    #[tokio::test]
    async fn a_wake_whose_event_is_refused_changes_nothing() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();
        b.enqueue(&spec(&TaskId::new("o")), 0).await.unwrap();
        let claim = b.claim_ready("r", 0, 1000).await.unwrap().unwrap().token;
        b.park(&TaskId::new("o"), Writer::Claim(claim), &Wait::Children, 0)
            .await
            .unwrap();
        insert_index(&b, &TaskId::new("o"), (0, 1, 1))
            .await
            .unwrap();

        let r = b.wake(&TaskId::new("o"), 1).await;

        assert!(
            matches!(r, Err(DurableStoreError::JournalSealed { .. })),
            "{r:?}"
        );
        let t = b.get(&TaskId::new("o")).await.unwrap().unwrap();
        assert_eq!(t.status, Status::Parked);
        assert_eq!(t.wait, Some(Wait::Children));
    }

    #[tokio::test]
    async fn a_stored_empty_id_surfaces_as_empty_id_naming_its_column() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();
        b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
        sqlx::query("UPDATE tasks SET job_id='' WHERE id='a'")
            .execute(b.db.pool())
            .await
            .unwrap();
        let err = b.get(&TaskId::new("a")).await.unwrap_err();
        assert!(
            matches!(err, DurableStoreError::EmptyId { column: "job_id" }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn corrupt_status_surfaces_typed_not_defaulted() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();
        b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
        sqlx::query("UPDATE tasks SET status='bogus' WHERE id='a'")
            .execute(b.db.pool())
            .await
            .unwrap();
        let err = b.get(&TaskId::new("a")).await.unwrap_err();
        assert!(matches!(err, DurableStoreError::CorruptStatus { .. }));
    }

    #[tokio::test]
    async fn since_respects_the_page_limit_and_resumes_from_the_cursor() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();
        b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
        let deltas = SINCE_PAGE_LIMIT + 5;
        for i in 0..deltas {
            b.append_delta(Writer::Unclaimed, &TaskId::new("a"), json!(i), 0, i)
                .await
                .unwrap();
        }
        let total = deltas + 1;
        let page1 = b.since(Cursor::START).await.unwrap();
        assert_eq!(
            page1.len() as i64,
            SINCE_PAGE_LIMIT,
            "one call caps at the page limit"
        );
        let after = Cursor::after(page1.last().unwrap().seq).unwrap();
        let page2 = b.since(after).await.unwrap();
        assert_eq!(page2.len() as i64, total - SINCE_PAGE_LIMIT);
    }

    /// A runner killed while its seal opens the write transaction must not leave
    /// the write lock behind. A task that does nothing but open (and drop) write
    /// transactions is aborted over and over, each abort landing somewhere inside a
    /// begin; after each one another writer must still get the file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_runner_killed_mid_begin_leaves_the_file_writable() {
        use sqlx::Connection;

        const KILLS: u64 = 200;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("durable.db");
        let store = Arc::new(
            SqliteDurableStore::open(
                &path,
                tenant(),
                Arc::new(MemTrajectoryLog::new()),
                sealing(),
            )
            .await
            .unwrap(),
        );
        let other_writer = SqliteConnectOptions::new()
            .filename(&path)
            .busy_timeout(Duration::from_secs(2));

        for kill in 0..KILLS {
            let b = store.clone();
            let runner = tokio::spawn(async move {
                loop {
                    drop(b.begin_immediate().await.unwrap());
                }
            });
            // Spread the kills over the begin's awaits.
            tokio::time::sleep(Duration::from_micros(50 * (kill % 40))).await;
            runner.abort();
            assert!(runner.await.unwrap_err().is_cancelled());

            let mut conn = sqlx::SqliteConnection::connect_with(&other_writer)
                .await
                .unwrap();
            sqlx::query("BEGIN IMMEDIATE")
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("kill #{kill} left the write lock held: {e}"));
            sqlx::query("ROLLBACK").execute(&mut conn).await.unwrap();
            conn.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn wal_and_synchronous_normal_are_both_set() {
        let path = std::env::temp_dir().join(format!("tars-rt2-pragma-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let b = SqliteDurableStore::open(
            &path,
            tenant(),
            Arc::new(MemTrajectoryLog::new()),
            sealing(),
        )
        .await
        .unwrap();
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(b.db.pool())
            .await
            .unwrap();
        let sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(b.db.pool())
            .await
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal", "WAL is set on a file DB");
        assert_eq!(sync, 1, "synchronous=NORMAL (1) — WAL's correct pairing");
        b.db.pool().close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_connections_do_not_return_sqlite_busy() {
        // Two SEPARATE per-file pools on the SAME file: one hammers writes (each
        // `commit` a statement), the other reads/claims concurrently. Without
        // WAL + busy_timeout one side would get SQLITE_BUSY and the unwrap panic.
        use std::sync::Arc;

        let path = std::env::temp_dir().join(format!("tars-rt2-sq-conc-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        const N: usize = 60;
        let writer = Arc::new(
            SqliteDurableStore::open(
                &path,
                tenant(),
                Arc::new(MemTrajectoryLog::new()),
                sealing(),
            )
            .await
            .unwrap(),
        );
        let reader = Arc::new(
            SqliteDurableStore::open(
                &path,
                tenant(),
                Arc::new(MemTrajectoryLog::new()),
                sealing(),
            )
            .await
            .unwrap(),
        );
        for i in 0..N {
            writer
                .enqueue(&spec(&TaskId::try_from(format!("s{i}")).unwrap()), 0)
                .await
                .unwrap();
        }
        let w = writer.clone();
        let wt = tokio::spawn(async move {
            // The reader claims some of these first; the durable store refuses an unclaimed
            // commit on those, which is an answer, not a busy database.
            for i in 0..N {
                match w
                    .commit(
                        &TaskId::try_from(format!("s{i}")).unwrap(),
                        Writer::Unclaimed,
                        json!({ "summary": "ok" }),
                        0,
                    )
                    .await
                {
                    Ok(()) | Err(DurableStoreError::LeaseLost { .. }) => {}
                    Err(e) => panic!("{e:?}"),
                }
            }
        });
        let r = reader.clone();
        let rt = tokio::spawn(async move {
            for _ in 0..(N * 4) {
                let _ = r.tasks().await.unwrap();
                let _ = r.claim_ready("reader", 0, 1_000).await.unwrap();
            }
        });
        wt.await.expect("writer saw no SQLITE_BUSY");
        rt.await.expect("reader saw no SQLITE_BUSY");
        writer.db.pool().close().await;
        reader.db.pool().close().await;
        let _ = std::fs::remove_file(&path);
    }

    /// How long another writer keeps the lock while a durable store write is waiting on it:
    /// well inside the durable store's 5 s `busy_timeout`, so a write that waits gets the lock.
    const FOREIGN_LOCK_HELD: Duration = Duration::from_secs(1);

    /// Runs `write` while another connection holds the file's write lock, releasing it
    /// once `write` has returned or has waited `FOREIGN_LOCK_HELD`. A write that reads
    /// first and only then asks for the lock gets `SQLITE_BUSY` while it is held.
    async fn under_a_foreign_write_lock<T: Send + 'static>(
        path: &Path,
        write: impl std::future::Future<Output = Result<T, DurableStoreError>> + Send + 'static,
    ) -> Result<T, DurableStoreError> {
        use sqlx::Connection;

        let mut other = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut other)
            .await
            .unwrap();

        let mut write = tokio::spawn(write);
        let early = tokio::time::timeout(FOREIGN_LOCK_HELD, &mut write).await;
        sqlx::query("ROLLBACK").execute(&mut other).await.unwrap();

        match early {
            Ok(done) => done.unwrap(),
            Err(_waiting) => write.await.unwrap(),
        }
    }

    /// Another runner renewing its lease must not make `cancel_descendants` fail: it
    /// reads the children, then writes them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_descendants_waits_out_another_writers_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("durable.db");
        let b = Arc::new(
            SqliteDurableStore::open(
                &path,
                tenant(),
                Arc::new(MemTrajectoryLog::new()),
                sealing(),
            )
            .await
            .unwrap(),
        );
        b.enqueue(&spec(&TaskId::new("p")), 0).await.unwrap();
        b.enqueue(&spec(&TaskId::new("c")).waking(TaskId::new("p")), 0)
            .await
            .unwrap();

        let store = b.clone();
        let cancelled = under_a_foreign_write_lock(&path, async move {
            store.cancel_descendants(&TaskId::new("p"), "why", 0).await
        })
        .await;

        assert_eq!(cancelled.unwrap().stopped, vec![TaskId::new("c")]);
    }

    async fn store_with_one_delta() -> (SqliteDurableStore, u64) {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();
        b.enqueue(&spec(&TaskId::new("a")), 0).await.unwrap();
        b.append_delta(
            Writer::Unclaimed,
            &TaskId::new("a"),
            json!({"turn": 1}),
            0,
            1,
        )
        .await
        .unwrap();
        let seq = b.trajectory(&TaskId::new("a")).await.unwrap()[1].seq;
        (b, seq)
    }

    async fn store_kind(b: &SqliteDurableStore, seq: u64, kind: &str) {
        sqlx::query("UPDATE trajectory SET kind = ? WHERE seq = ?")
            .bind(kind)
            .bind(seq as i64)
            .execute(b.db.pool())
            .await
            .unwrap();
    }

    async fn insert_at_seq(b: &SqliteDurableStore, seq: i64) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO trajectory (seq, task_id, kind, at) VALUES (?, 'a', '{}', 0)")
            .bind(seq)
            .execute(b.db.pool())
            .await
            .map(drop)
    }

    fn assert_corrupt_event(err: DurableStoreError, want_seq: u64, want_kind: &str) {
        match err {
            DurableStoreError::CorruptEvent {
                seq, task_id, kind, ..
            } => {
                assert_eq!(
                    (seq, task_id.as_str(), kind.as_str()),
                    (want_seq, "a", want_kind)
                );
            }
            other => panic!("expected CorruptEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_event_kind_reads_back_with_its_seq_task_and_text() {
        let (b, seq) = store_with_one_delta().await;
        assert!(
            b.since(Cursor::START).await.is_ok(),
            "a kind the durable store wrote reads"
        );

        store_kind(&b, seq, r#"{"NoSuchKind":1}"#).await;

        assert_corrupt_event(
            b.since(Cursor::START).await.unwrap_err(),
            seq,
            r#"{"NoSuchKind":1}"#,
        );
        assert_corrupt_event(
            b.trajectory(&TaskId::new("a")).await.unwrap_err(),
            seq,
            r#"{"NoSuchKind":1}"#,
        );
    }

    #[tokio::test]
    async fn the_schema_takes_a_trajectory_seq_of_one_and_refuses_zero_and_below() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();

        insert_at_seq(&b, 1).await.unwrap();
        for seq in [0, -5] {
            let refused = insert_at_seq(&b, seq).await.unwrap_err().to_string();
            assert!(
                refused.contains("CHECK constraint failed"),
                "{seq}: {refused}"
            );
        }
    }

    async fn insert_index(
        b: &SqliteDurableStore,
        task_id: &TaskId,
        (row_group, first_seq, last_seq): (i64, i64, i64),
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO journal_file (name, state, created_at) VALUES ('f', 'live', 0)",
        )
        .execute(b.db.pool())
        .await?;
        sqlx::query(
            "INSERT INTO journal_index (task_id, file, row_group, first_seq, last_seq) \
             VALUES (?, 'f', ?, ?, ?)",
        )
        .bind(task_id.as_str())
        .bind(row_group)
        .bind(first_seq)
        .bind(last_seq)
        .execute(b.db.pool())
        .await
        .map(drop)
    }

    #[tokio::test]
    async fn the_schema_takes_index_seqs_in_order_from_one_and_refuses_the_rest() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();

        insert_index(&b, &TaskId::new("one"), (0, 1, 1))
            .await
            .unwrap();
        insert_index(&b, &TaskId::new("span"), (0, 1, 3))
            .await
            .unwrap();
        for (n, row) in [(0, 0, 3), (0, -1, 3), (0, 4, 3), (-1, 1, 3)]
            .into_iter()
            .enumerate()
        {
            let refused = insert_index(&b, &TaskId::try_from(format!("bad{n}")).unwrap(), row)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                refused.contains("CHECK constraint failed"),
                "{row:?}: {refused}"
            );
        }
    }

    #[tokio::test]
    async fn the_schema_refuses_a_negative_holds_count() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();
        b.enqueue(
            &TaskSpec::new(
                TaskId::new("t"),
                JobId::new("j"),
                AgentKey::new("k"),
                json!(null),
            ),
            0,
        )
        .await
        .unwrap();

        let set = |holds: i64| {
            sqlx::query("UPDATE tasks SET holds = ? WHERE id = 't'")
                .bind(holds)
                .execute(b.db.pool())
        };
        set(0).await.unwrap();
        let refused = set(-1).await.unwrap_err().to_string();
        assert!(refused.contains("CHECK constraint failed"), "{refused}");
    }

    #[tokio::test]
    async fn the_schema_refuses_a_value_of_another_type() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();

        let refused =
            sqlx::query("INSERT INTO trajectory (task_id, kind, at) VALUES ('a', '{}', 'soon')")
                .execute(b.db.pool())
                .await
                .unwrap_err()
                .to_string();
        assert!(
            refused.contains("cannot store TEXT value in INTEGER column"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn every_table_of_the_schema_is_strict() {
        let b = SqliteDurableStore::in_memory(tenant()).await.unwrap();

        let loose: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM pragma_table_list
             WHERE schema = 'main' AND type = 'table' AND strict = 0
               AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
        )
        .fetch_all(b.db.pool())
        .await
        .unwrap();
        assert!(loose.is_empty(), "{loose:?}");
    }

    #[test]
    fn stored_seq_reads_one_and_refuses_zero() {
        assert_eq!(stored_seq(&TaskId::new("a"), 1).unwrap(), 1);
        assert!(matches!(
            stored_seq(&TaskId::new("a"), 0),
            Err(DurableStoreError::CorruptSeq { seq: 0, .. })
        ));
    }

    #[test]
    fn seq_column_takes_i64_max_and_refuses_one_more() {
        assert_eq!(seq_column(i64::MAX as u64).unwrap(), i64::MAX);
        assert!(matches!(
            seq_column(i64::MAX as u64 + 1),
            Err(DurableStoreError::SeqOutOfRange { .. })
        ));
    }

    #[test]
    fn an_index_row_the_schema_refuses_is_corrupt() {
        let file = "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string();
        assert!(index_entry(&TaskId::new("a"), (file.clone(), 0, 1, 3)).is_ok());
        assert!(index_entry(&TaskId::new("a"), (file.clone(), 0, 3, 3)).is_ok());
        for (row_group, first, last) in [(0, 0, 3), (0, 4, 3), (-1, 1, 3), (0, 1, -1)] {
            assert!(
                matches!(
                    index_entry(&TaskId::new("a"), (file.clone(), row_group, first, last)),
                    Err(DurableStoreError::CorruptJournalIndex { .. })
                ),
                "row group {row_group}, seqs {first}..={last}"
            );
        }
    }

    #[test]
    fn an_index_row_with_a_negative_seq_is_corrupt_with_its_numbers() {
        let file = "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string();
        match index_entry(&TaskId::new("a"), (file, 0, -1, 3)).unwrap_err() {
            DurableStoreError::CorruptJournalIndex {
                task_id,
                row_group,
                first_seq,
                last_seq,
            } => assert_eq!(
                (task_id.as_str(), row_group, first_seq, last_seq),
                ("a", 0, -1, 3)
            ),
            other => panic!("expected CorruptJournalIndex, got {other:?}"),
        }
    }
}
