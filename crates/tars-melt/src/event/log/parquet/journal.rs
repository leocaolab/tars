//! Parquet store for the durable store's sealed journals: `<root>/task_journal/<name>.parquet`.
//!
//! `stage` writes `<name>.tmp`, fsyncs it, then renames it to `.parquet` —
//! durable under its final name, yet invisible: only the durable store's registry
//! makes it live, and only the durable store deletes it, by name.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ::parquet::file::statistics::Statistics;
use arrow_array::cast::AsArray;
use arrow_array::types::{Int64Type, UInt64Type};
use arrow_array::{Int64Array, RecordBatch, UInt64Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use tars_storage::durable::journal::{
    FindJournalError, JournalError, JournalFileName, JournalRow, StagedFile, TrajectoryLog, staged,
    task_groups,
};

use super::super::seal::{RowGroup, row_groups};
use super::super::{LogError, LogTable, TableName};
use super::file;
use super::partition::io_at;
use super::{
    ColumnError, DecodeError, EncodeError, IoWork, OffsetLimit, ParquetTable, blocking, strs,
};

/// The `task_journal` table: key = task id, one row group per task.
struct TaskJournal;

impl LogTable for TaskJournal {
    type Row = JournalRow;
    type Key = String;

    const NAME: TableName = TableName::new("task_journal");
    /// Never cuts a group: `task_groups` makes exactly one per task.
    const ROW_GROUP_ROWS: NonZeroUsize = NonZeroUsize::MAX;

    fn sort_key(row: &JournalRow) -> String {
        row.task_id.clone()
    }

    fn approx_bytes(row: &JournalRow) -> usize {
        row.approx_bytes()
    }
}

impl ParquetTable for TaskJournal {
    const KEY_COLUMN: &'static str = "task_id";

    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("task_id", DataType::Utf8, false),
            Field::new("seq", DataType::UInt64, false),
            Field::new("at", DataType::Int64, false),
            Field::new("payload", DataType::Utf8, false),
        ]))
    }

    fn encode_within(rows: &[JournalRow], limit: OffsetLimit) -> Result<RecordBatch, EncodeError> {
        Ok(RecordBatch::try_new(
            Self::schema(),
            vec![
                strs(
                    limit,
                    "task_id",
                    rows.iter().map(|r| Some(r.task_id.as_str())),
                )?,
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.seq))),
                Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.at))),
                strs(
                    limit,
                    "payload",
                    rows.iter().map(|r| Some(r.payload.as_str())),
                )?,
            ],
        )?)
    }

    fn decode(batch: &RecordBatch) -> Result<Vec<JournalRow>, DecodeError> {
        let columns = (
            batch.column(0).as_string_opt::<i32>(),
            batch.column(1).as_primitive_opt::<UInt64Type>(),
            batch.column(2).as_primitive_opt::<Int64Type>(),
            batch.column(3).as_string_opt::<i32>(),
        );
        let (Some(task_ids), Some(seqs), Some(ats), Some(payloads)) = columns else {
            return Err(ColumnError::Shape {
                expected: "utf8, u64, i64, utf8",
                found: batch.schema(),
            }
            .into());
        };
        Ok((0..batch.num_rows())
            .map(|i| JournalRow {
                task_id: task_ids.value(i).to_owned(),
                seq: seqs.value(i),
                at: ats.value(i),
                payload: payloads.value(i).to_owned(),
            })
            .collect())
    }

    fn key_range(stats: &Statistics) -> Option<(String, String)> {
        let Statistics::ByteArray(s) = stats else {
            return None;
        };
        let text = |v: &::parquet::data_type::ByteArray| v.as_utf8().ok().map(str::to_owned);
        Some((text(s.min_opt()?)?, text(s.max_opt()?)?))
    }
}

/// Sealed journals of one durable store, under `root`. Its own failures reach the durable store as
/// [`JournalError::Store`], the [`LogError`] inside.
#[derive(Debug)]
pub struct ParquetTrajectoryLog {
    dir: PathBuf,
}

impl ParquetTrajectoryLog {
    /// Opens the journals under `root`; it does no IO.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            dir: root.into().join(TaskJournal::NAME.as_str()),
        }
    }

    fn path(&self, name: &JournalFileName, ext: &str) -> PathBuf {
        self.dir.join(format!("{}.{ext}", name.as_str()))
    }
}

#[async_trait]
impl TrajectoryLog for ParquetTrajectoryLog {
    async fn stage(
        &self,
        name: &JournalFileName,
        rows: Vec<JournalRow>,
    ) -> Result<StagedFile, JournalError> {
        let groups = task_groups(rows);
        let file = staged(name, &groups);
        let groups: Vec<RowGroup<TaskJournal>> = groups
            .into_iter()
            .flat_map(row_groups::<TaskJournal>)
            .collect();
        let (dir, tmp, target) = (
            self.dir.clone(),
            self.path(name, "tmp"),
            self.path(name, "parquet"),
        );
        blocking(IoWork::StageJournal(name.clone()), move || {
            fs::create_dir_all(&dir).map_err(io_at(&dir))?;
            file::write::<TaskJournal>(&tmp, &groups)?;
            fs::rename(&tmp, &target).map_err(io_at(&target))
        })
        .await
        .map_err(store_failed)?;
        Ok(file)
    }

    async fn read_group(
        &self,
        file: &JournalFileName,
        row_group: u32,
    ) -> Result<Vec<JournalRow>, JournalError> {
        let path = self.path(file, "parquet");
        let work = IoWork::ReadJournalGroup {
            file: file.clone(),
            row_group,
        };
        let read = blocking(work, move || {
            let opened = match fs::File::open(&path) {
                Ok(opened) => opened,
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    return Ok(Err(FindJournalError::File { path, source }));
                }
                Err(e) => return Err(io_at(&path)(e)),
            };
            file::read_row_group::<TaskJournal>(&path, &opened, row_group).map(Ok)
        })
        .await
        .map_err(store_failed)?;
        match read {
            Ok(Some(rows)) => Ok(rows),
            Ok(None) => Err(JournalError::NoGroup {
                file: file.clone(),
                row_group,
            }),
            Err(source) => Err(JournalError::NotStored {
                file: file.clone(),
                source,
            }),
        }
    }

    /// Removes both forms the name can take on disk, since a crash mid-`stage` leaves the `.tmp`.
    async fn remove(&self, name: &JournalFileName) -> Result<(), JournalError> {
        let paths = [self.path(name, "tmp"), self.path(name, "parquet")];
        blocking(IoWork::RemoveJournal(name.clone()), move || {
            paths.iter().try_for_each(|p| remove_if_present(p))
        })
        .await
        .map_err(store_failed)
    }

    async fn stored(&self) -> Result<Vec<JournalFileName>, JournalError> {
        let dir = self.dir.clone();
        blocking(IoWork::ListJournals, move || {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(io_at(&dir)(e)),
            };
            let names = entries
                .map(|entry| entry.map(|e| journal_name(&e.file_name())))
                .collect::<Result<BTreeSet<_>, _>>()
                .map_err(io_at(&dir))?;
            Ok(names.into_iter().flatten().collect())
        })
        .await
        .map_err(store_failed)
    }
}

fn store_failed(e: LogError) -> JournalError {
    JournalError::Store(Box::new(e))
}

/// Returns the journal name a directory entry carries, if it is one of ours (`.parquet` or a
/// crashed stage's `.tmp`). Anything else in the directory is not ours to touch, a UUID spelled
/// other than hyphenated lower case included: we never write one, and `remove` would look for it
/// under the hyphenated name.
fn journal_name(file_name: &std::ffi::OsStr) -> Option<JournalFileName> {
    let file_name = file_name.to_str()?;
    let stem = file_name
        .strip_suffix(".parquet")
        .or_else(|| file_name.strip_suffix(".tmp"))?;
    JournalFileName::parse(stem)
        .ok()
        .filter(|name| name.as_str() == stem)
}

fn remove_if_present(path: &Path) -> Result<(), LogError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_at(path)(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(task: &str, seq: u64) -> JournalRow {
        JournalRow {
            task_id: task.to_owned(),
            seq,
            at: 1_700_000_000_000 + seq as i64,
            payload: format!(r#"{{"z":1,"kind":"delta","seq":{seq}}}"#),
        }
    }

    #[test]
    fn a_payload_past_the_limit_names_the_column_and_row() {
        // Each payload is about 30 bytes.
        let limit = OffsetLimit::new(40).unwrap();

        let got = TaskJournal::encode_within(&[row("t1", 1), row("t1", 2)], limit);

        assert!(
            matches!(
                got,
                Err(EncodeError::TextTooLong {
                    column: "payload",
                    row: 1
                })
            ),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn a_file_named_by_a_uuid_spelled_another_way_is_not_listed() {
        let root = tempfile::tempdir().unwrap();
        let log = ParquetTrajectoryLog::open(root.path());
        let dir = root.path().join("task_journal");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0123456789abcdef0123456789abcdef.parquet"), b"x").unwrap();
        std::fs::write(
            dir.join("01234567-89AB-CDEF-0123-456789ABCDEF.parquet"),
            b"x",
        )
        .unwrap();

        let listed = log.stored().await.unwrap();

        assert!(listed.is_empty(), "{listed:?}");
    }

    #[tokio::test]
    async fn a_staged_group_reads_back_byte_identical_and_no_tmp_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let log = ParquetTrajectoryLog::open(dir.path());
        let name = JournalFileName::fresh();

        let file = log
            .stage(&name, vec![row("t2", 7), row("t1", 3), row("t2", 5)])
            .await
            .unwrap();

        let t2 = file.groups.iter().find(|g| g.task_id == "t2").unwrap();
        assert_eq!((t2.first_seq, t2.last_seq), (5, 7));
        assert_eq!(
            log.read_group(&name, t2.row_group).await.unwrap(),
            vec![row("t2", 5), row("t2", 7)]
        );
        let names: Vec<String> = fs::read_dir(dir.path().join("task_journal"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![format!("{}.parquet", name.as_str())]);
        assert!(matches!(
            log.read_group(&name, 2).await,
            Err(JournalError::NoGroup { row_group: 2, .. })
        ));
    }

    #[tokio::test]
    async fn a_group_past_the_last_names_its_journal_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = ParquetTrajectoryLog::open(dir.path());
        let name = JournalFileName::fresh();
        log.stage(&name, vec![row("t1", 1)]).await.unwrap();

        let missing = log.read_group(&name, 1).await;

        assert!(
            matches!(&missing, Err(JournalError::NoGroup { file, row_group: 1 }) if *file == name),
            "{missing:?}"
        );
    }

    #[tokio::test]
    async fn a_journal_never_staged_or_removed_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let log = ParquetTrajectoryLog::open(dir.path());
        let (never, removed) = (JournalFileName::fresh(), JournalFileName::fresh());
        log.stage(&removed, vec![row("t1", 1)]).await.unwrap();
        log.remove(&removed).await.unwrap();

        for name in [never, removed] {
            let missing = log.read_group(&name, 0).await;
            assert!(
                matches!(
                    &missing,
                    Err(JournalError::NotStored { file, source: FindJournalError::File { path, .. } })
                        if *file == name && *path == log.path(&name, "parquet")
                ),
                "{missing:?}"
            );
        }
    }

    #[tokio::test]
    async fn remove_deletes_the_named_file_and_its_leftover_tmp_only() {
        let dir = tempfile::tempdir().unwrap();
        let log = ParquetTrajectoryLog::open(dir.path());
        let (gone, kept) = (JournalFileName::fresh(), JournalFileName::fresh());
        log.stage(&gone, vec![row("t1", 1)]).await.unwrap();
        log.stage(&kept, vec![row("t2", 2)]).await.unwrap();
        fs::write(log.path(&gone, "tmp"), b"half-written").unwrap();

        log.remove(&gone).await.unwrap();
        log.remove(&gone).await.unwrap();

        let names: Vec<String> = fs::read_dir(dir.path().join("task_journal"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![format!("{}.parquet", kept.as_str())]);
        fs::write(log.path(&gone, "tmp"), b"crashed stage").unwrap();
        fs::write(dir.path().join("task_journal/not-ours.txt"), b"x").unwrap();
        let mut stored = log.stored().await.unwrap();
        stored.sort();
        let mut want = vec![gone, kept];
        want.sort();
        assert_eq!(stored, want);
    }
}
