//! The port the durable store seals finished tasks' journals through. There is no manifest: a
//! staged file is invisible until the durable store registers it in its own SQLite, in the same
//! transaction that deletes the rows it holds, so with one commit point a row is never in two
//! places or none. A store only writes such a file, reads one row group back and deletes it by
//! name; what a row's `payload` means, and when a file becomes visible, is the durable store's
//! business. The store's file format is its own: this crate does not know it.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

/// One journal row as the durable store stored it. `payload` is the durable store's `kind`
/// column verbatim: re-encoding it would reorder keys, and a sealed row must
/// read back byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRow {
    pub task_id: String,
    pub seq: u64,
    pub at: i64,
    pub payload: String,
}

impl JournalRow {
    /// Returns the row's weight, the size a store and the durable store's seal batches both count
    /// it as.
    pub fn approx_bytes(&self) -> usize {
        self.task_id.len() + self.payload.len() + 16
    }
}

/// A staged file's name. Minted here so it is always a safe single path
/// component; the durable store records it before the file exists.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JournalFileName(String);

#[derive(Debug, Error)]
#[error("`{0}` is not a journal file name")]
pub struct BadJournalFileName(pub String);

impl JournalFileName {
    pub fn fresh() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Parses a name read back from the durable store's registry or a store's listing.
    pub fn parse(name: &str) -> Result<Self, BadJournalFileName> {
        Uuid::parse_str(name)
            .map(|u| Self(u.to_string()))
            .map_err(|_| BadJournalFileName(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a store found no journal file of a name: where it looked and what it was told there, so a
/// person can look in the same place.
#[derive(Debug, Error)]
pub enum FindJournalError {
    /// A [`MemTrajectoryLog`] holds no file of that name.
    #[error("no file of that name in memory")]
    InMemory,
    /// Opening the file at `path` said it does not exist.
    #[error("{}", path.display())]
    File { path: PathBuf, source: io::Error },
}

/// Why a journal store could not do what the durable store asked.
#[derive(Debug, Error)]
pub enum JournalError {
    /// No journal file of this name is stored: it was never staged, or it was removed. `source`
    /// says where the store looked.
    #[error("journal file `{}` is not stored", file.as_str())]
    NotStored {
        file: JournalFileName,
        source: FindJournalError,
    },
    #[error("journal file `{}` has no row group {row_group}", file.as_str())]
    NoGroup {
        file: JournalFileName,
        row_group: u32,
    },
    /// The store itself failed: its file IO, or a file that does not decode. The store's own
    /// error is boxed because this crate defines the port and cannot name an adapter's types.
    #[error("the journal file store failed")]
    Store(#[source] Box<dyn StdError + Send + Sync>),
}

/// Where one task's journal landed in a staged file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskGroup {
    pub task_id: String,
    pub row_group: u32,
    pub first_seq: u64,
    pub last_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    pub name: JournalFileName,
    pub groups: Vec<TaskGroup>,
}

/// Write side and point read of sealed journals.
#[async_trait]
pub trait TrajectoryLog: Send + Sync {
    /// Writes `rows` to a durable file named `name`, one row group per task as [`task_groups`]
    /// cuts them. No reader sees it until the caller registers it.
    async fn stage(
        &self,
        name: &JournalFileName,
        rows: Vec<JournalRow>,
    ) -> Result<StagedFile, JournalError>;

    /// Reads one row group, which is one task's whole journal.
    async fn read_group(
        &self,
        file: &JournalFileName,
        row_group: u32,
    ) -> Result<Vec<JournalRow>, JournalError>;

    /// Deletes the file named `name`, finished or half-written, and nothing else. A file that is
    /// already gone counts as removed, because a crash between this and the caller dropping its
    /// registration makes the caller ask again.
    async fn remove(&self, name: &JournalFileName) -> Result<(), JournalError>;

    /// Lists every file this store holds under a name it could have minted, finished or
    /// half-written, for the caller to check against its registry for leftovers.
    async fn stored(&self) -> Result<Vec<JournalFileName>, JournalError>;
}

/// Sorts `rows` by `(task_id, seq)` and cuts one group per task, the layout every store writes;
/// no group is empty.
pub fn task_groups(mut rows: Vec<JournalRow>) -> Vec<Vec<JournalRow>> {
    rows.sort_by(|a, b| (&a.task_id, a.seq).cmp(&(&b.task_id, b.seq)));
    rows.chunk_by(|a, b| a.task_id == b.task_id)
        .map(<[JournalRow]>::to_vec)
        .collect()
}

/// Returns the registry entries the groups of a file named `name` need, group `i` being row
/// group `i`.
pub fn staged(name: &JournalFileName, groups: &[Vec<JournalRow>]) -> StagedFile {
    let groups = groups
        .iter()
        .zip(0u32..)
        .filter_map(|(rows, row_group)| {
            let (first, last) = (rows.first()?, rows.last()?);
            Some(TaskGroup {
                task_id: first.task_id.clone(),
                row_group,
                first_seq: first.seq,
                last_seq: last.seq,
            })
        })
        .collect();
    StagedFile {
        name: name.clone(),
        groups,
    }
}

/// In-memory store. Clones share the same files.
#[derive(Clone, Default)]
pub struct MemTrajectoryLog {
    files: Arc<RwLock<HashMap<JournalFileName, Vec<Vec<JournalRow>>>>>,
}

impl MemTrajectoryLog {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TrajectoryLog for MemTrajectoryLog {
    async fn stage(
        &self,
        name: &JournalFileName,
        rows: Vec<JournalRow>,
    ) -> Result<StagedFile, JournalError> {
        let groups = task_groups(rows);
        let file = staged(name, &groups);
        self.files
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.clone(), groups);
        Ok(file)
    }

    async fn read_group(
        &self,
        file: &JournalFileName,
        row_group: u32,
    ) -> Result<Vec<JournalRow>, JournalError> {
        let files = self.files.read().unwrap_or_else(PoisonError::into_inner);
        let groups = files.get(file).ok_or_else(|| JournalError::NotStored {
            file: file.clone(),
            source: FindJournalError::InMemory,
        })?;
        groups
            .get(row_group as usize)
            .cloned()
            .ok_or_else(|| JournalError::NoGroup {
                file: file.clone(),
                row_group,
            })
    }

    async fn remove(&self, name: &JournalFileName) -> Result<(), JournalError> {
        self.files
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
        Ok(())
    }

    async fn stored(&self) -> Result<Vec<JournalFileName>, JournalError> {
        Ok(self
            .files
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(task: &str, seq: u64) -> JournalRow {
        JournalRow {
            task_id: task.to_owned(),
            seq,
            at: seq as i64 * 10,
            payload: format!(r#"{{"kind":"delta","n":{seq}}}"#),
        }
    }

    #[test]
    fn one_group_per_task_sorted_by_task_then_seq() {
        let groups = task_groups(vec![row("b", 5), row("a", 3), row("b", 2), row("a", 1)]);
        let layout: Vec<(String, Vec<u64>)> = groups
            .iter()
            .map(|g| (g[0].task_id.clone(), g.iter().map(|r| r.seq).collect()))
            .collect();
        assert_eq!(
            layout,
            vec![("a".into(), vec![1, 3]), ("b".into(), vec![2, 5])]
        );
        let file = staged(&JournalFileName::fresh(), &groups);
        assert_eq!(file.groups[1].row_group, 1);
        assert_eq!((file.groups[1].first_seq, file.groups[1].last_seq), (2, 5));
    }

    #[test]
    fn a_name_that_is_not_one_this_module_minted_is_refused() {
        let fresh = JournalFileName::fresh();
        assert_eq!(JournalFileName::parse(fresh.as_str()).unwrap(), fresh);
        assert!(JournalFileName::parse("../x").is_err());
    }

    #[tokio::test]
    async fn mem_reads_back_exactly_the_staged_group() {
        let log = MemTrajectoryLog::new();
        let name = JournalFileName::fresh();
        let file = log
            .stage(&name, vec![row("b", 4), row("a", 1), row("b", 2)])
            .await
            .unwrap();
        let b = file.groups.iter().find(|g| g.task_id == "b").unwrap();
        assert_eq!(
            log.read_group(&name, b.row_group).await.unwrap(),
            vec![row("b", 2), row("b", 4)]
        );
        assert!(log.read_group(&name, 9).await.is_err());
    }

    #[tokio::test]
    async fn mem_a_missing_group_names_its_journal_file() {
        let log = MemTrajectoryLog::new();
        let name = JournalFileName::fresh();
        log.stage(&name, vec![row("a", 1)]).await.unwrap();

        let missing = log.read_group(&name, 1).await;

        assert!(
            matches!(&missing, Err(JournalError::NoGroup { file, row_group: 1 }) if *file == name),
            "{missing:?}"
        );
    }

    #[tokio::test]
    async fn mem_a_journal_never_staged_is_not_stored() {
        let log = MemTrajectoryLog::new();
        let name = JournalFileName::fresh();

        let missing = log.read_group(&name, 0).await;

        assert!(
            matches!(
                &missing,
                Err(JournalError::NotStored { file, source: FindJournalError::InMemory }) if *file == name
            ),
            "{missing:?}"
        );
    }

    #[tokio::test]
    async fn mem_remove_deletes_only_the_named_file() {
        let log = MemTrajectoryLog::new();
        let (gone, kept) = (JournalFileName::fresh(), JournalFileName::fresh());
        log.stage(&gone, vec![row("a", 1)]).await.unwrap();
        log.stage(&kept, vec![row("b", 2)]).await.unwrap();

        log.remove(&gone).await.unwrap();
        log.remove(&gone).await.unwrap();

        assert!(log.read_group(&gone, 0).await.is_err());
        assert_eq!(log.read_group(&kept, 0).await.unwrap(), vec![row("b", 2)]);
        assert_eq!(log.stored().await.unwrap(), vec![kept]);
    }
}
