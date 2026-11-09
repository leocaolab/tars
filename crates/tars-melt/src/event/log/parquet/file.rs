//! One sealed batch ⇄ one Parquet file: each `RowGroup` is one Parquet row
//! group, so its key statistics are exactly the group's `[min, max]`.

use std::fs::File;
use std::io::Write;
use std::iter;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ::parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use ::parquet::arrow::{ArrowWriter, ProjectionMask};
use ::parquet::basic::{Compression, ZstdLevel};
use ::parquet::file::metadata::ParquetMetaData;
use ::parquet::file::properties::WriterProperties;
use ::parquet::file::reader::ChunkReader;
use arrow_array::RecordBatch;
use arrow_schema::Schema;
use itertools::{Itertools, process_results};

use super::super::seal::RowGroup;
use super::super::{LogError, LogTable};
use super::partition::io_at;
use super::{DecodeError, ParquetProjected, ParquetTable};

/// Which rows a read wants.
pub(super) enum Scan<K> {
    All,
    Key(K),
}

/// Writes `groups` to `path` and fsyncs it. Nothing is visible until the partition publishes the
/// file.
pub(super) fn write<T: ParquetTable>(path: &Path, groups: &[RowGroup<T>]) -> Result<(), LogError> {
    let mut writer = create::<T>(path)?;
    groups
        .iter()
        .try_for_each(|group| write_group::<T, _>(&mut writer, path, group.rows()))?;
    finish(writer, path)
}

fn create<T: ParquetTable>(path: &Path) -> Result<ArrowWriter<File>, LogError> {
    let file = File::create(path).map_err(io_at(path))?;
    writer::<T, _>(file, path)
}

/// Starts a Parquet file of `T` in `sink`, which receives the file at `path`.
fn writer<T: ParquetTable, W: Write + Send>(
    sink: W,
    path: &Path,
) -> Result<ArrowWriter<W>, LogError> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .build();
    ArrowWriter::try_new(sink, T::schema(), Some(props)).map_err(write_at(path))
}

/// Writes `rows` as exactly one row group.
fn write_group<T: ParquetTable, W: Write + Send>(
    writer: &mut ArrowWriter<W>,
    path: &Path,
    rows: &[T::Row],
) -> Result<(), LogError> {
    let batch = T::encode(rows).map_err(|source| LogError::Encode {
        path: path.to_path_buf(),
        source,
    })?;
    writer.write(&batch).map_err(write_at(path))?;
    writer.flush().map_err(write_at(path))
}

/// Writes the footer and returns the sink.
fn close<W: Write + Send>(writer: ArrowWriter<W>, path: &Path) -> Result<W, LogError> {
    writer.into_inner().map_err(write_at(path))
}

fn finish(writer: ArrowWriter<File>, path: &Path) -> Result<(), LogError> {
    close(writer, path)?.sync_all().map_err(io_at(path))
}

/// Rewrites every row of `inputs` into one file at `out`, sorted by key and
/// re-cut into row groups, and fsyncs it. Every input is sorted already (each
/// was written from `row_groups` or by a merge), so the rows stream through
/// a k-way merge: memory holds one decoded row group per input and the
/// output group being filled, however many rows the inputs hold. The file
/// must hold exactly as many rows as the inputs did, or the merge fails and
/// nothing may publish it.
pub(super) fn merge<T: ParquetTable>(inputs: &[PathBuf], out: &Path) -> Result<(), LogError> {
    let runs = inputs
        .iter()
        .map(|path| sorted_run::<T>(path))
        .collect::<Result<Vec<_>, _>>()?;

    let expected = write_rows::<T>(out, merge_runs::<T>(runs))?;

    let found = row_count(out)?;
    if found == expected {
        Ok(())
    } else {
        Err(LogError::RowCountChanged {
            path: out.to_path_buf(),
            expected,
            found,
        })
    }
}

/// A row with the key it sorts by, computed once.
struct Keyed<T: LogTable> {
    key: T::Key,
    row: T::Row,
}

/// Returns the rows of one input file, decoding a row group only once the merge has taken every
/// row of the one before.
fn sorted_run<T: ParquetTable>(
    path: &Path,
) -> Result<impl Iterator<Item = Result<Keyed<T>, LogError>>, LogError> {
    let (file, meta) = open(path)?;
    let groups = 0..meta.metadata().num_row_groups();
    let at = path.to_path_buf();
    let decoded = groups.map(move |group| read_group::<T>(&at, &file, &meta, group));
    Ok(keyed_run::<T>(path.to_path_buf(), decoded))
}

fn read_group<T: ParquetTable>(
    path: &Path,
    file: &File,
    meta: &ArrowReaderMetadata,
    group: usize,
) -> Result<Vec<T::Row>, LogError> {
    let file = file.try_clone().map_err(io_at(path))?;
    let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(file, meta.clone());
    decode_run(path, builder, group..group + 1, T::decode)
}

/// Decodes the rows of `groups`, consecutive row groups of the file at `path`, with `decode`.
/// Every failure names the file; a row that does not decode is named by its row in the file.
fn decode_run<R: ChunkReader + 'static, O>(
    path: &Path,
    builder: ParquetRecordBatchReaderBuilder<R>,
    groups: Range<usize>,
    decode: impl Fn(&RecordBatch) -> Result<Vec<O>, DecodeError>,
) -> Result<Vec<O>, LogError> {
    let first = rows_before(path, builder.metadata(), groups.start)?;
    let batches = builder
        .with_row_groups(groups.collect())
        .build()
        .map_err(open_at(path))?;

    let mut rows = Vec::new();
    let mut read = 0u64;
    for batch in batches {
        let batch = batch.map_err(decode_at(path))?;
        rows.extend(decode(&batch).map_err(|e| located(path, first + read, e))?);
        read += batch.num_rows() as u64;
    }
    Ok(rows)
}

/// Returns how many rows the row groups before `group` hold, from the footer.
fn rows_before(path: &Path, meta: &ParquetMetaData, group: usize) -> Result<u64, LogError> {
    meta.row_groups()
        .iter()
        .take(group)
        .map(|g| footer_rows(path, g.num_rows()))
        .sum()
}

/// Places a batch's decode failure in the file, the batch starting at file row `first`.
fn located(path: &Path, first: u64, e: DecodeError) -> LogError {
    match e {
        DecodeError::Columns(source) => LogError::Decode {
            path: path.to_path_buf(),
            source,
        },
        DecodeError::Row { row, source } => LogError::DecodeRow {
            path: path.to_path_buf(),
            row: first + row as u64,
            source,
        },
    }
}

fn open_at(path: &Path) -> impl FnOnce(::parquet::errors::ParquetError) -> LogError + '_ {
    move |source| LogError::Open {
        path: path.to_path_buf(),
        source,
    }
}

fn write_at(path: &Path) -> impl FnOnce(::parquet::errors::ParquetError) -> LogError + '_ {
    move |source| LogError::Write {
        path: path.to_path_buf(),
        source,
    }
}

fn decode_at(path: &Path) -> impl FnOnce(arrow_schema::ArrowError) -> LogError + '_ {
    move |source| LogError::Decode {
        path: path.to_path_buf(),
        source: source.into(),
    }
}

/// Returns `groups`' rows keyed, pulling the next group only when the last row of
/// the one before is taken. A row that sorts before the one ahead of it
/// fails the run: the merge below would place it wrong, not re-sort it.
fn keyed_run<T: LogTable>(
    path: PathBuf,
    groups: impl Iterator<Item = Result<Vec<T::Row>, LogError>>,
) -> impl Iterator<Item = Result<Keyed<T>, LogError>> {
    groups
        .flatten_ok()
        .enumerate()
        .scan(None, move |last: &mut Option<T::Key>, (at, row)| {
            Some(row.and_then(|row| {
                let key = T::sort_key(&row);
                if last.as_ref().is_some_and(|prev| key < *prev) {
                    return Err(LogError::UnsortedMergeInput {
                        path: path.clone(),
                        row: at as u64,
                    });
                }
                *last = Some(key.clone());
                Ok(Keyed { key, row })
            }))
        })
}

/// Merges sorted runs into one sorted stream, pulling a run's next row only
/// when its head is taken. Equal keys keep run order, then their order
/// within the run: exactly what a stable sort of the runs' concatenation
/// gives. A run's error comes out ahead of every row.
fn merge_runs<T: LogTable>(
    runs: Vec<impl Iterator<Item = Result<Keyed<T>, LogError>>>,
) -> impl Iterator<Item = Result<T::Row, LogError>> {
    runs.into_iter()
        .enumerate()
        .map(|(run, rows)| rows.map(move |keyed| keyed.map(|keyed| (run, keyed))))
        .kmerge_by(|a, b| match (a, b) {
            (Ok((run_a, a)), Ok((run_b, b))) => (&a.key, run_a) < (&b.key, run_b),
            (Err(_), _) => true,
            (Ok(_), Err(_)) => false,
        })
        .map_ok(|(_, keyed)| keyed.row)
}

/// Writes `rows` to `path` in `T::ROW_GROUP_ROWS` groups, as `row_groups` cuts a sorted batch,
/// fsyncs it, and returns how many rows it wrote.
fn write_rows<T: ParquetTable>(
    path: &Path,
    rows: impl Iterator<Item = Result<T::Row, LogError>>,
) -> Result<u64, LogError> {
    let mut writer = create::<T>(path)?;
    let written = process_results(rows, |rows| {
        rows.chunks(T::ROW_GROUP_ROWS.get())
            .into_iter()
            .try_fold(0u64, |written, chunk| {
                let group: Vec<T::Row> = chunk.collect();
                write_group::<T, _>(&mut writer, path, &group)?;
                Ok::<_, LogError>(written + group.len() as u64)
            })
    })??;
    finish(writer, path)?;
    Ok(written)
}

/// Returns how many rows the file holds, from its footer alone.
pub(super) fn row_count(path: &Path) -> Result<u64, LogError> {
    let file = File::open(path).map_err(io_at(path))?;
    let rows = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(open_at(path))?
        .metadata()
        .file_metadata()
        .num_rows();
    footer_rows(path, rows)
}

/// Returns a row count the footer states, which is signed on disk.
fn footer_rows(path: &Path, rows: i64) -> Result<u64, LogError> {
    // [arc:intentional-handle] reason: TryFromIntError says only that `rows` is negative, which the error carries.
    u64::try_from(rows).map_err(|_| LogError::NegativeRowCount {
        path: path.to_path_buf(),
        rows,
    })
}

pub(super) fn read<T: ParquetTable>(
    path: &Path,
    scan: &Scan<T::Key>,
) -> Result<Vec<T::Row>, LogError> {
    let (file, meta) = open(path)?;
    let runs: Vec<Range<usize>> = match scan {
        Scan::All => iter::once(0..meta.metadata().num_row_groups()).collect(),
        Scan::Key(key) => groups_to_read::<T>(path, meta.metadata(), key)?
            .into_iter()
            .map(|g| g..g + 1)
            .collect(),
    };

    let mut rows = Vec::new();
    for run in runs {
        let file = file.try_clone().map_err(io_at(path))?;
        let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(file, meta.clone());
        rows.extend(decode_run(path, builder, run, T::decode)?);
    }
    Ok(match scan {
        Scan::All => rows,
        Scan::Key(key) => rows
            .into_iter()
            .filter(|r| T::sort_key(r) == *key)
            .collect(),
    })
}

/// Returns the rows of row group `group` by position of `file`, opened from `path`, or `None`
/// when the file has fewer groups.
pub(super) fn read_row_group<T: ParquetTable>(
    path: &Path,
    file: &File,
    group: u32,
) -> Result<Option<Vec<T::Row>>, LogError> {
    let meta = metadata(path, file)?;
    let group = group as usize;
    if group >= meta.metadata().num_row_groups() {
        return Ok(None);
    }
    read_group::<T>(path, file, &meta, group).map(Some)
}

fn open(path: &Path) -> Result<(File, ArrowReaderMetadata), LogError> {
    let file = File::open(path).map_err(io_at(path))?;
    let meta = metadata(path, &file)?;
    Ok((file, meta))
}

fn metadata(path: &Path, file: &File) -> Result<ArrowReaderMetadata, LogError> {
    ArrowReaderMetadata::load(file, ArrowReaderOptions::default()).map_err(open_at(path))
}

/// Reads every row of the file at `path` in summary.
pub(super) fn read_partial<T: ParquetProjected>(path: &Path) -> Result<Vec<T::Partial>, LogError> {
    let file = File::open(path).map_err(io_at(path))?;
    read_partial_from::<T, _>(path, file)
}

/// Reads every row of `source`, the file at `path`, in summary, decoding only the column chunks
/// of `T::PARTIAL_COLUMNS`.
pub(super) fn read_partial_from<T: ParquetProjected, R: ChunkReader + 'static>(
    path: &Path,
    source: R,
) -> Result<Vec<T::Partial>, LogError> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(source).map_err(open_at(path))?;
    let mask = partial_mask::<T>(builder.parquet_schema(), builder.schema());
    let groups = 0..builder.metadata().num_row_groups();
    decode_run(
        path,
        builder.with_projection(mask),
        groups,
        T::decode_partial,
    )
}

/// Returns the mask of the file's top-level columns named in `T::PARTIAL_COLUMNS`. A writer maps
/// each Arrow field to one top-level Parquet column, in order.
fn partial_mask<T: ParquetProjected>(
    parquet: &::parquet::schema::types::SchemaDescriptor,
    arrow: &Schema,
) -> ProjectionMask {
    let wanted = arrow
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| T::PARTIAL_COLUMNS.contains(&f.name().as_str()))
        .map(|(i, _)| i);
    ProjectionMask::roots(parquet, wanted)
}

/// Returns the row groups whose key statistics can hold `key`. A group without statistics cannot
/// be ruled out, so it is read.
fn groups_to_read<T: ParquetTable>(
    path: &Path,
    meta: &ParquetMetaData,
    key: &T::Key,
) -> Result<Vec<usize>, LogError> {
    let column = meta
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .position(|c| c.name() == T::KEY_COLUMN)
        .ok_or_else(|| LogError::KeyColumn {
            path: path.to_path_buf(),
            column: T::KEY_COLUMN,
        })?;

    Ok(meta
        .row_groups()
        .iter()
        .enumerate()
        .filter(|(_, group)| {
            group
                .column(column)
                .statistics()
                .and_then(T::key_range)
                .is_none_or(|(min, max)| min <= *key && *key <= max)
        })
        .map(|(i, _)| i)
        .collect())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use ::parquet::file::statistics::Statistics;
    use arrow_array::{ArrayRef, StringArray, UInt64Array};
    use arrow_schema::SchemaRef;

    use super::super::tests::test_rows;
    use super::*;
    use crate::event::log::contract::{TestRow, TestTable};
    use crate::event::log::seal::row_groups;
    use crate::event::log::{EncodeError, OffsetLimit, TableName};

    fn rows(keys: &[u64], body: &str) -> Vec<TestRow> {
        keys.iter().map(|&k| TestRow::new(k, body)).collect()
    }

    #[test]
    fn a_merge_writes_the_file_a_sort_of_all_its_inputs_would() {
        let dir = tempfile::tempdir().unwrap();
        // Key 3 in every input: equal keys must keep input order.
        let batches = [
            rows(&[1, 3, 3, 5, 9], "a"),
            rows(&[2, 3, 6], "b"),
            rows(&[3, 4], "c"),
        ];
        let inputs: Vec<PathBuf> = batches
            .iter()
            .enumerate()
            .map(|(i, batch)| {
                let path = dir.path().join(format!("in-{i}.parquet"));
                write::<TestTable>(&path, &row_groups(batch.clone())).unwrap();
                path
            })
            .collect();
        let sorted_whole = dir.path().join("sorted.parquet");
        write::<TestTable>(&sorted_whole, &row_groups(batches.concat())).unwrap();

        let out = dir.path().join("out.tmp");
        merge::<TestTable>(&inputs, &out).unwrap();

        assert_eq!(
            read::<TestTable>(&out, &Scan::All).unwrap(),
            read::<TestTable>(&sorted_whole, &Scan::All).unwrap()
        );
        assert_eq!(
            std::fs::read(&out).unwrap(),
            std::fs::read(&sorted_whole).unwrap(),
            "same rows, same order, same row groups"
        );
    }

    #[test]
    fn a_merge_decodes_a_run_s_next_row_group_only_once_it_has_taken_the_last() {
        let pulled = [Cell::new(0), Cell::new(0)];
        let run = |i: usize, keys: std::ops::Range<u64>| {
            let groups: Vec<Vec<TestRow>> =
                keys.collect::<Vec<_>>().chunks(2).map(test_rows).collect();
            let count = &pulled[i];
            let counted = groups
                .into_iter()
                .inspect(move |_| count.set(count.get() + 1));
            keyed_run::<TestTable>(PathBuf::from(format!("run-{i}")), counted.map(Ok))
        };
        // 100 groups of two in each run; every row of run 0 sorts first.
        let mut merged = merge_runs::<TestTable>(vec![run(0, 0..200), run(1, 1000..1200)]);

        let taken: Vec<u64> = merged
            .by_ref()
            .take(3)
            .map(|row| row.unwrap().key())
            .collect();

        assert_eq!(taken, vec![0, 1, 2]);
        assert_eq!(
            (pulled[0].get(), pulled[1].get()),
            (2, 1),
            "run 0 decoded its first two groups, run 1 only its head's"
        );
    }

    #[test]
    fn an_input_out_of_key_order_fails_the_merge_naming_the_file_and_row() {
        let dir = tempfile::tempdir().unwrap();
        let sorted = dir.path().join("sorted.parquet");
        write::<TestTable>(&sorted, &row_groups(test_rows(&[1, 4]))).unwrap();
        let unsorted = dir.path().join("unsorted.parquet");
        let group = RowGroup::unchecked(2, 3, test_rows(&[3, 2]));
        write::<TestTable>(&unsorted, &[group]).unwrap();

        let merged = merge::<TestTable>(&[sorted, unsorted.clone()], &dir.path().join("out.tmp"));

        match merged {
            Err(LogError::UnsortedMergeInput { path, row }) => {
                assert_eq!((path, row), (unsorted, 1));
            }
            other => panic!("expected UnsortedMergeInput, got {other:?}"),
        }
    }

    #[test]
    fn lookup_opens_only_row_groups_whose_range_holds_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.parquet");
        // Groups of two: [1,2] [3,3] [3,5] [6,7].
        write::<TestTable>(&path, &row_groups(test_rows(&[7, 3, 1, 5, 3, 6, 2, 3]))).unwrap();

        let file = File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let meta = builder.metadata();
        assert_eq!(meta.num_row_groups(), 4);
        let groups = |k: u64| groups_to_read::<TestTable>(&path, meta, &k).unwrap();

        assert_eq!(groups(5), vec![2]);
        assert_eq!(groups(3), vec![1, 2]);
        assert_eq!(groups(4), vec![2], "inside [3,5]: read, then filtered out");
        assert!(groups(8).is_empty());
        assert_eq!(
            read::<TestTable>(&path, &Scan::Key(3)).unwrap().len(),
            3,
            "the key's rows across both groups"
        );
    }

    /// A sink that takes `budget` bytes, then fails every write.
    struct Budget {
        budget: usize,
        taken: Vec<u8>,
    }

    impl Write for Budget {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.budget - self.taken.len());
            if n == 0 && !buf.is_empty() {
                return Err(std::io::Error::other("sink is full"));
            }
            self.taken.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Writes two row groups of `TestTable` into a sink of `budget` bytes, as `write` does.
    fn write_into(budget: usize, path: &Path) -> Result<Vec<u8>, LogError> {
        let sink = Budget {
            budget,
            taken: Vec::new(),
        };
        let mut writer = writer::<TestTable, _>(sink, path)?;
        for keys in [[1, 2], [3, 4]] {
            write_group::<TestTable, _>(&mut writer, path, &test_rows(&keys))?;
        }
        Ok(close(writer, path)?.taken)
    }

    #[test]
    fn a_write_that_fails_at_any_byte_names_the_file() {
        let path = Path::new("tenant=t/test/date=2026-11-07/0001.parquet");
        let whole = write_into(usize::MAX, path).unwrap().len();

        assert!(write_into(whole, path).is_ok());
        for budget in 0..whole {
            match write_into(budget, path) {
                Err(LogError::Write { path: named, .. }) => assert_eq!(named, path),
                other => panic!("budget {budget} of {whole}: expected Write, got {other:?}"),
            }
        }
    }

    /// `TestTable` with its body column declared non-null and an empty body encoded as null, so a
    /// row with an empty body does not encode.
    struct NonEmptyBodyTable;

    impl LogTable for NonEmptyBodyTable {
        type Row = TestRow;
        type Key = u64;

        const NAME: TableName = TableName::new("non_empty_body_test");
        const ROW_GROUP_ROWS: NonZeroUsize = TestTable::ROW_GROUP_ROWS;

        fn sort_key(row: &TestRow) -> u64 {
            row.key()
        }

        fn approx_bytes(row: &TestRow) -> usize {
            TestTable::approx_bytes(row)
        }
    }

    impl ParquetTable for NonEmptyBodyTable {
        const KEY_COLUMN: &'static str = TestTable::KEY_COLUMN;

        fn schema() -> SchemaRef {
            TestTable::schema()
        }

        fn encode_within(rows: &[TestRow], _: OffsetLimit) -> Result<RecordBatch, EncodeError> {
            let keys = UInt64Array::from_iter_values(rows.iter().map(TestRow::key));
            let bodies = StringArray::from_iter(
                rows.iter()
                    .map(|row| Some(row.body()).filter(|body| !body.is_empty())),
            );
            Ok(RecordBatch::try_new(
                Self::schema(),
                vec![Arc::new(keys) as ArrayRef, Arc::new(bodies)],
            )?)
        }

        fn decode(batch: &RecordBatch) -> Result<Vec<TestRow>, DecodeError> {
            TestTable::decode(batch)
        }

        fn key_range(stats: &Statistics) -> Option<(u64, u64)> {
            TestTable::key_range(stats)
        }
    }

    #[test]
    fn a_group_whose_rows_do_not_encode_names_the_file() {
        let path = Path::new("tenant=t/test/date=2026-11-07/0001.parquet");
        let mut writer = writer::<NonEmptyBodyTable, _>(Vec::new(), path).unwrap();

        let encoded = write_group::<NonEmptyBodyTable, _>(&mut writer, path, &rows(&[1], "x"));
        let refused = write_group::<NonEmptyBodyTable, _>(&mut writer, path, &rows(&[2], ""));

        assert!(encoded.is_ok(), "{encoded:?}");
        match refused {
            Err(LogError::Encode { path: named, .. }) => assert_eq!(named, path),
            other => panic!("expected Encode, got {other:?}"),
        }
    }

    #[test]
    fn a_row_that_does_not_decode_is_placed_by_its_row_in_the_file() {
        let path = Path::new("f.parquet");
        let bad = || super::super::ColumnError::Null { column: "c".into() };

        let row = located(
            path,
            16,
            DecodeError::Row {
                row: 2,
                source: bad(),
            },
        );
        assert!(matches!(row, LogError::DecodeRow { row: 18, .. }), "{row}");
        let batch = located(path, 16, DecodeError::Columns(bad()));
        assert!(matches!(batch, LogError::Decode { .. }), "{batch}");
    }
}
