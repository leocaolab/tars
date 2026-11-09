//! Test probe: which columns a read of one file touches, judged by the
//! byte ranges it asks the file for against each column chunk's range.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use ::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use ::parquet::file::reader::{ChunkReader, Length};
use bytes::Bytes;

use super::super::seal::row_groups;
use super::file::{read_partial_from, write};
use super::{ParquetProjected, ParquetTable};

/// Seals `rows` into one file at `path`, as a sink would.
pub(crate) fn sealed_file<T: ParquetTable>(path: &Path, rows: Vec<T::Row>) {
    write::<T>(path, &row_groups::<T>(rows)).unwrap();
}

/// Returns the top-level columns a summary read of `path` touched.
pub(crate) fn columns_read_in_summary<T: ParquetProjected>(path: &Path) -> BTreeSet<String> {
    let (source, reads) = Recorder::open(path);
    read_partial_from::<T, _>(path, source).unwrap();
    touched(path, &reads)
}

/// Returns the top-level columns a whole-row read of `path` touched: the control
/// that shows the probe sees a body read when there is one.
pub(crate) fn columns_read_whole<T: ParquetTable>(path: &Path) -> BTreeSet<String> {
    let (source, reads) = Recorder::open(path);
    for batch in ParquetRecordBatchReaderBuilder::try_new(source)
        .unwrap()
        .build()
        .unwrap()
    {
        T::decode(&batch.unwrap()).unwrap();
    }
    touched(path, &reads)
}

/// `[start, end)` byte ranges asked of the file.
type Reads = Arc<Mutex<Vec<(u64, u64)>>>;

struct Recorder {
    file: File,
    reads: Reads,
}

impl Recorder {
    fn open(path: &Path) -> (Self, Reads) {
        let reads = Reads::default();
        let file = File::open(path).unwrap();
        let source = Self {
            file,
            reads: Arc::clone(&reads),
        };
        (source, reads)
    }

    fn record(&self, start: u64, end: u64) {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((start, end));
    }
}

impl Length for Recorder {
    fn len(&self) -> u64 {
        self.file.len()
    }
}

impl ChunkReader for Recorder {
    type T = <File as ChunkReader>::T;

    /// Returns a reader from `start` on, counted as a read of the chunk it starts in.
    fn get_read(&self, start: u64) -> ::parquet::errors::Result<Self::T> {
        self.record(start, start + 1);
        self.file.get_read(start)
    }

    fn get_bytes(&self, start: u64, length: usize) -> ::parquet::errors::Result<Bytes> {
        self.record(start, start + length as u64);
        self.file.get_bytes(start, length)
    }
}

/// Returns each column whose chunk, in any row group, overlaps a recorded read.
fn touched(path: &Path, reads: &Reads) -> BTreeSet<String> {
    let reads = reads.lock().unwrap_or_else(PoisonError::into_inner);
    let file = File::open(path).unwrap();
    let meta = ParquetRecordBatchReaderBuilder::try_new(file)
        .unwrap()
        .metadata()
        .clone();
    meta.row_groups()
        .iter()
        .flat_map(|group| group.columns())
        .filter(|chunk| {
            let (start, len) = chunk.byte_range();
            reads.iter().any(|&(s, e)| s < start + len && start < e)
        })
        .map(|chunk| chunk.column_path().parts()[0].clone())
        .collect()
}
