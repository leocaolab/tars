//! Scoped reads: one tenant's table directory, only the days the window
//! covers, only the files each manifest lists — and every row checked
//! against the tenant whose directory it was found in.

use std::path::{Path, PathBuf};

use tars_types::TenantId;

use super::super::{LogError, Partitioned, Projected, TenantScope, greatest_first};
use super::file::{self, Scan};
use super::partition::{PartitionDir, read_listed};
use super::{DATE_PREFIX, ParquetProjected, ParquetTable, dated, is_gone, subdirs, table_dir};

/// Reads every row `scan` wants from the partitions `scope` covers.
pub(super) fn read_scoped<T: ParquetTable + Partitioned>(
    root: &Path,
    scope: &TenantScope,
    scan: &Scan<T::Key>,
) -> Result<Vec<T::Row>, LogError> {
    let parts = scoped_partitions(root, T::NAME.as_str(), scope)?;
    let rows = parts
        .iter()
        .map(|p| {
            read_owned_partition(
                p,
                scope.tenant(),
                |path| file::read::<T>(path, scan),
                |row| T::partition(row).tenant,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().flatten().collect())
}

/// What a newest-first read decodes from each file: whole rows, or their
/// summaries.
pub(super) trait Decode<T: Projected> {
    type Out: Send + 'static;

    fn read(path: &Path) -> Result<Vec<Self::Out>, LogError>;
    fn tenant(out: &Self::Out) -> TenantId;
    fn key(out: &Self::Out) -> T::Key;
    fn keeps(filter: &T::Filter, out: &Self::Out) -> bool;
}

/// Whole rows, every column decoded.
pub(super) struct Rows;

impl<T: ParquetTable + Projected> Decode<T> for Rows {
    type Out = T::Row;

    fn read(path: &Path) -> Result<Vec<T::Row>, LogError> {
        file::read::<T>(path, &Scan::All)
    }

    fn tenant(row: &T::Row) -> TenantId {
        T::partition(row).tenant
    }

    fn key(row: &T::Row) -> T::Key {
        T::sort_key(row)
    }

    fn keeps(filter: &T::Filter, row: &T::Row) -> bool {
        T::keeps(filter, &T::partial(row))
    }
}

/// Summaries, only the summary's columns decoded.
pub(super) struct Summaries;

impl<T: ParquetProjected> Decode<T> for Summaries {
    type Out = T::Partial;

    fn read(path: &Path) -> Result<Vec<T::Partial>, LogError> {
        file::read_partial::<T>(path)
    }

    fn tenant(partial: &T::Partial) -> TenantId {
        T::partial_tenant(partial).clone()
    }

    fn key(partial: &T::Partial) -> T::Key {
        T::partial_key(partial)
    }

    fn keeps(filter: &T::Filter, partial: &T::Partial) -> bool {
        T::keeps(filter, partial)
    }
}

/// Reads what `D` decodes from the partitions `scope` covers and `filter` keeps,
/// greatest key first, at most `limit`: newest day first, stopping at the
/// first whole day that brings the kept count to the limit. Every decoded
/// row is checked against the tenant before the filter sees it, so a
/// foreign row fails the read even where the filter would drop it.
pub(super) fn read_newest<T: Projected, D: Decode<T>>(
    root: &Path,
    scope: &TenantScope,
    filter: &T::Filter,
    limit: Option<usize>,
) -> Result<Vec<D::Out>, LogError> {
    let parts = newest_first(scoped_partitions(root, T::NAME.as_str(), scope)?);

    let mut kept = Vec::new();
    for part in parts {
        if limit_met(kept.len(), limit) {
            break;
        }
        let read = read_owned_partition(&part, scope.tenant(), D::read, D::tenant)?;
        kept.extend(read.into_iter().filter(|out| D::keeps(filter, out)));
    }

    Ok(greatest_first(kept, D::key, limit))
}

/// Lists the partitions of the scope's tenant's table directory alone; no other tenant's
/// directory is opened.
fn scoped_partitions(
    root: &Path,
    table: &str,
    scope: &TenantScope,
) -> Result<Vec<PartitionDir>, LogError> {
    let listed = dated(subdirs(
        &table_dir(root, table, scope.tenant()),
        DATE_PREFIX,
    )?)?;
    Ok(covered(listed, scope))
}

/// Keeps the listed partitions on a day the scope's window covers.
fn covered(listed: Vec<PartitionDir>, scope: &TenantScope) -> Vec<PartitionDir> {
    listed
        .into_iter()
        .filter(|p| scope.window().covers(p.day()))
        .collect()
}

/// Returns whether, once `read` rows are kept, `limit` needs no older day.
fn limit_met(read: usize, limit: Option<usize>) -> bool {
    limit.is_some_and(|n| read >= n)
}

fn newest_first(mut parts: Vec<PartitionDir>) -> Vec<PartitionDir> {
    parts.sort_by_key(|p| std::cmp::Reverse(p.day()));
    parts
}

/// Reads a partition. A file merged away since the manifest was read is found at its retired name
/// until `grace` has passed; a reader slower than that finds it gone, and then the manifest is read
/// once more and the partition read again.
fn read_owned_partition<R>(
    dir: &PartitionDir,
    tenant: &TenantId,
    read: impl Fn(&Path) -> Result<Vec<R>, LogError>,
    owner: impl Fn(&R) -> TenantId,
) -> Result<Vec<R>, LogError> {
    let read_owned = |paths: &[PathBuf]| -> Result<Vec<R>, LogError> {
        let rows = paths
            .iter()
            .map(|path| {
                read_listed(path, &read).and_then(|rows| owned_by(path, rows, tenant, &owner))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    };
    match read_owned(&dir.live_files()?) {
        Err(e) if is_gone(&e) => read_owned(&dir.live_files()?),
        read => read,
    }
}

/// Checks every row belongs to `tenant`: a row of another tenant in its directory fails the read,
/// naming the file and the row's tenant, and is never skipped.
fn owned_by<R>(
    path: &Path,
    rows: Vec<R>,
    tenant: &TenantId,
    owner: impl Fn(&R) -> TenantId,
) -> Result<Vec<R>, LogError> {
    match rows.iter().map(owner).find(|t| t != tenant) {
        Some(found) => Err(LogError::ForeignRow {
            path: path.to_path_buf(),
            expected: tenant.clone(),
            found,
        }),
        None => Ok(rows),
    }
}

#[cfg(test)]
mod tests;
