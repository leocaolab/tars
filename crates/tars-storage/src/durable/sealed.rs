//! Reads a durable store file's journal whether or not it has been sealed.
//! [`SqliteDurableStore`](crate::durable::SqliteDurableStore)'s trajectory reads go through here,
//! and so does the debugger, which opens a trace's `fiber.db` on its own read-only connection.
//!
//! A task's journal is, at any moment, either all in the `trajectory` table or all in the
//! one row group `journal_index` points at (`docs/design/log-parquet.md` §4). Both are
//! handed back as the same [`JournalRow`], `payload` verbatim, so a reader sees the same
//! bytes before and after the seal.
use crate::durable::journal::{JournalFileName, JournalRow, TrajectoryLog};
use sqlx::{Connection, SqliteConnection};
use tars_types::TaskId;

use crate::durable::seal::IndexEntry;
use crate::durable::{DurableStoreError, stored_id};

/// Returns one task's whole journal in `seq` order, from wherever it is now. A sealed
/// one costs exactly one row-group read.
///
/// The index and the live rows are read in ONE snapshot: otherwise "not sealed" → a
/// seal commits and deletes the rows → "no rows" would read a sealed journal as empty.
pub async fn read_journal_raw(
    conn: &mut SqliteConnection,
    journal: &dyn TrajectoryLog,
    task_id: &TaskId,
) -> Result<Vec<JournalRow>, DurableStoreError> {
    let mut tx = conn.begin().await?;
    let Some(entry) = sealed_entry(&mut tx, task_id).await? else {
        return journal_rows(&mut tx, task_id).await;
    };
    drop(tx);

    Ok(journal.read_group(&entry.file, entry.row_group).await?)
}

/// Returns every task that has a journal, live or sealed, in the order its first row
/// was written.
pub async fn journal_task_ids(
    conn: &mut SqliteConnection,
) -> Result<Vec<TaskId>, DurableStoreError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT task_id FROM (
             SELECT task_id, MIN(seq) AS first FROM trajectory GROUP BY task_id
             UNION ALL
             SELECT task_id, first_seq AS first FROM journal_index)
         GROUP BY task_id ORDER BY MIN(first)",
    )
    .fetch_all(conn)
    .await?;
    ids.into_iter().map(|id| stored_id("task_id", id)).collect()
}

async fn sealed_entry(
    conn: &mut SqliteConnection,
    task_id: &TaskId,
) -> Result<Option<IndexEntry>, DurableStoreError> {
    let row: Option<IndexRow> = sqlx::query_as(
        "SELECT file, row_group, first_seq, last_seq FROM journal_index WHERE task_id = ?",
    )
    .bind(task_id.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| index_entry(task_id, row)).transpose()
}

/// A `journal_index` row as stored: `(file, row_group, first_seq, last_seq)`.
pub(super) type IndexRow = (String, i64, i64, i64);

/// Parses a `journal_index` row. A row the schema's checks refuse (a negative row group, a
/// seq at or below zero, seqs out of order) was written past them, so it is corrupt.
pub(super) fn index_entry(
    task_id: &TaskId,
    row: IndexRow,
) -> Result<IndexEntry, DurableStoreError> {
    let (file, row_group, first_seq, last_seq) = row;
    let corrupt = || DurableStoreError::CorruptJournalIndex {
        task_id: task_id.clone(),
        row_group,
        first_seq,
        last_seq,
    };
    let (Ok(group), Ok(first), Ok(last)) = (
        u32::try_from(row_group),
        u64::try_from(first_seq),
        u64::try_from(last_seq),
    ) else {
        return Err(corrupt());
    };
    if !seqs_in_order(first, last) {
        return Err(corrupt());
    }
    Ok(IndexEntry {
        file: JournalFileName::parse(&file)?,
        row_group: group,
        first_seq: first,
        last_seq: last,
    })
}

/// Returns whether a sealed journal's first and last seq are ones a journal can have: the
/// first above zero and no later than the last.
fn seqs_in_order(first: u64, last: u64) -> bool {
    first > 0 && first <= last
}

/// Reads a stored `seq` of `task_id`'s journal. The schema refuses one at or below zero, so
/// such a value means the file was written past its checks.
pub(super) fn stored_seq(task_id: &TaskId, seq: i64) -> Result<u64, DurableStoreError> {
    match u64::try_from(seq) {
        Ok(positive) if positive > 0 => Ok(positive),
        _ => Err(DurableStoreError::CorruptSeq {
            task_id: task_id.clone(),
            seq,
        }),
    }
}

/// Returns `seq` as a SQLite durable store stores and compares it.
pub(super) fn seq_column(seq: u64) -> Result<i64, DurableStoreError> {
    i64::try_from(seq).map_err(|_| DurableStoreError::SeqOutOfRange { seq })
}

/// Returns one task's live journal rows, `kind` kept verbatim.
pub(super) async fn journal_rows(
    conn: &mut SqliteConnection,
    task_id: &TaskId,
) -> Result<Vec<JournalRow>, DurableStoreError> {
    let rows: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT seq, kind, at FROM trajectory WHERE task_id = ? ORDER BY seq")
            .bind(task_id.as_str())
            .fetch_all(&mut *conn)
            .await?;
    rows.into_iter()
        .map(|(seq, payload, at)| {
            Ok(JournalRow {
                task_id: task_id.to_string(),
                seq: stored_seq(task_id, seq)?,
                at,
                payload,
            })
        })
        .collect()
}
