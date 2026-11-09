//! Opens a durable store file with its sealed journals kept in Parquet beside it, under
//! `<store file>.journal/`. The durable store does not know the journal's file format; this is
//! where the Parquet store is chosen for it.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::event::log::ParquetTrajectoryLog;
use tars_storage::durable::{
    DurableStoreInitError, ExistingDurableStore, Sealing, SqliteDurableStore,
};
use tars_types::TenantId;

/// Returns `<store file>.journal`: sealed journals sit beside the durable store that indexes
/// them, so a copied trace carries them along.
pub fn journal_dir(durable_file: &Path) -> PathBuf {
    let mut dir = durable_file.as_os_str().to_owned();
    dir.push(".journal");
    PathBuf::from(dir)
}

/// Opens the durable store file at `path` for `tenant`, creating it if missing, with its sealed
/// journals in Parquet under [`journal_dir`], sealing on `sealing`'s schedule; see
/// [`SqliteDurableStore::open`].
pub async fn open(
    path: impl AsRef<Path>,
    tenant: TenantId,
    sealing: Sealing,
) -> Result<SqliteDurableStore, DurableStoreInitError> {
    let path = path.as_ref();
    let journal = ParquetTrajectoryLog::open(journal_dir(path));
    SqliteDurableStore::open(path, tenant, Arc::new(journal), sealing).await
}

/// Opens a durable store file a runner already made, for a tool with no tenant of its own, with its
/// sealed journals in Parquet under [`journal_dir`]; see [`SqliteDurableStore::open_existing`].
pub async fn open_existing(
    path: impl AsRef<Path>,
) -> Result<ExistingDurableStore, DurableStoreInitError> {
    let path = path.as_ref();
    let journal = ParquetTrajectoryLog::open(journal_dir(path));
    SqliteDurableStore::open_existing(path, Arc::new(journal)).await
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use sqlx::ConnectOptions;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::time::Duration;
    use tars_storage::durable::journal::{
        JournalError, JournalFileName, JournalRow, StagedFile, TrajectoryLog,
    };

    use tars_storage::durable::{
        DurableStore, DurableStoreError, FiberStore, JournalSealPolicy, SealJournals, Writer,
        journal_task_ids, read_journal_raw,
    };
    use tars_types::{AgentKey, JobId, TaskId, TaskSpec};

    use super::*;

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

    /// `id` claimed, journalled three times, committed — terminal, owing nothing.
    async fn finished(b: &SqliteDurableStore, id: &TaskId) {
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
            b.append_delta(
                Writer::Claim(claim),
                id,
                json!({ "z": n, "a": [n, "x"] }),
                0,
                100 + n,
            )
            .await
            .unwrap();
        }
        b.commit(id, Writer::Claim(claim), json!({ "summary": "done" }), 0)
            .await
            .unwrap();
    }

    async fn seal(b: &SqliteDurableStore, id: &TaskId) -> Result<(), DurableStoreError> {
        b.seal_tasks(std::slice::from_ref(id), 1_000)
            .await
            .map(drop)
    }

    /// The debugger's path: its own read-only connection to the durable store file, the sealed
    /// files found beside it, and the rows exactly as the durable store stored them — `payload`
    /// byte for byte, not decoded and re-encoded.
    #[tokio::test]
    async fn the_raw_journal_is_byte_identical_on_a_read_only_connection_after_the_seal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fiber.db");
        let durable = open(&path, TenantId::new("tenant-test"), sealing())
            .await
            .unwrap();
        finished(&durable, &TaskId::new("t")).await;
        finished(&durable, &TaskId::new("u")).await;
        // Stored by some other writer: keys out of order, spaces kept. Decoding and
        // re-encoding would change these bytes, so only a verbatim path keeps them.
        const VERBATIM: &str = r#"{"payload": {"z": 1, "a": [1, "x"]}, "kind": "delta"}"#;
        let mut writer = SqliteConnectOptions::new()
            .filename(&path)
            .connect()
            .await
            .unwrap();
        sqlx::query("INSERT INTO trajectory (task_id, kind, at) VALUES ('t', ?, 200)")
            .bind(VERBATIM)
            .execute(&mut writer)
            .await
            .unwrap();
        let reader = || async {
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .read_only(true)
                .connect()
                .await
                .unwrap();
            let sealed = ParquetTrajectoryLog::open(journal_dir(&path));
            let rows = read_journal_raw(&mut conn, &sealed, &TaskId::new("t"))
                .await
                .unwrap();
            let ids = journal_task_ids(&mut conn).await.unwrap();
            (rows, ids)
        };
        let (before, ids_before) = reader().await;
        assert_eq!(
            ids_before,
            vec![TaskId::new("t"), TaskId::new("u")],
            "premise"
        );

        seal(&durable, &TaskId::new("t")).await.unwrap();

        let (after, ids_after) = reader().await;
        assert_eq!(after, before, "the same rows, payload bytes included");
        assert_eq!(
            ids_after, ids_before,
            "a sealed task is still listed, in place"
        );
        assert_eq!(
            after.last().map(|r| r.payload.as_str()),
            Some(VERBATIM),
            "a row no encoder of ours would produce comes back as it was stored"
        );
    }

    /// A Parquet journal store that writes the file, then fails as a crash before the commit
    /// would.
    struct CrashAfterStage(ParquetTrajectoryLog);

    #[async_trait]
    impl TrajectoryLog for CrashAfterStage {
        async fn stage(
            &self,
            name: &JournalFileName,
            rows: Vec<JournalRow>,
        ) -> Result<StagedFile, JournalError> {
            self.0.stage(name, rows).await?;
            Err(JournalError::Store("crashed after staging".into()))
        }

        async fn read_group(
            &self,
            file: &JournalFileName,
            row_group: u32,
        ) -> Result<Vec<JournalRow>, JournalError> {
            self.0.read_group(file, row_group).await
        }

        async fn remove(&self, name: &JournalFileName) -> Result<(), JournalError> {
            self.0.remove(name).await
        }

        async fn stored(&self) -> Result<Vec<JournalFileName>, JournalError> {
            self.0.stored().await
        }
    }

    #[tokio::test]
    async fn the_grace_sweep_deletes_the_parquet_file_a_crashed_seal_staged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fiber.db");
        let durable = SqliteDurableStore::open(
            &path,
            TenantId::new("tenant-test"),
            Arc::new(CrashAfterStage(ParquetTrajectoryLog::open(journal_dir(
                &path,
            )))),
            sealing(),
        )
        .await
        .unwrap();
        finished(&durable, &TaskId::new("t")).await;

        let crashed = seal(&durable, &TaskId::new("t")).await;
        assert!(
            matches!(crashed, Err(DurableStoreError::Journal(_))),
            "premise: {crashed:?}"
        );

        assert_eq!(durable.remove_staged(1_000).await.unwrap(), 1);
        let left: Vec<_> = std::fs::read_dir(journal_dir(&path).join("task_journal"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}
