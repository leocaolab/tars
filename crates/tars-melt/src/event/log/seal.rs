//! Pure core: when a buffer seals, and how a sealed batch is laid out.
//! Time arrives as an argument; nothing here reads a clock.

use std::time::{Duration, Instant};

use super::LogTable;

/// When the in-flight buffer is sealed. It is passed in, so the crash window, `max_age`, is a
/// setting to tune rather than a constant.
#[derive(Debug, Clone, Copy)]
pub struct SealPolicy {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub max_age: Duration,
    /// How old an orphaned temp file must be before cleanup may delete it
    /// (file adapters; another writer's temp file is never that old).
    pub grace: Duration,
}

/// The in-flight buffer: empty, or a batch opened when its first row arrived.
pub(crate) struct Pending<R>(Option<Batch<R>>);

/// The rows waiting to be sealed. It is never empty: only a push opens one.
struct Batch<R> {
    rows: Vec<R>,
    bytes: usize,
    opened_at: Instant,
}

impl<R> Pending<R> {
    pub(crate) fn new() -> Self {
        Self(None)
    }

    pub(crate) fn push(&mut self, row: R, bytes: usize, now: Instant) {
        let batch = self.0.get_or_insert_with(|| Batch {
            rows: Vec::new(),
            bytes: 0,
            opened_at: now,
        });
        batch.bytes += bytes;
        batch.rows.push(row);
    }

    pub(crate) fn take(&mut self) -> Vec<R> {
        self.0.take().map_or_else(Vec::new, |batch| batch.rows)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Returns when the oldest pending row reaches `max_age`, or `None` when the buffer is empty.
    pub(crate) fn deadline(&self, policy: &SealPolicy) -> Option<Instant> {
        self.0.as_ref().map(|b| b.opened_at + policy.max_age)
    }
}

pub(crate) fn seal_due<R>(p: &Pending<R>, now: Instant, policy: &SealPolicy) -> bool {
    p.0.as_ref().is_some_and(|b| {
        b.rows.len() >= policy.max_rows
            || b.bytes >= policy.max_bytes
            || now >= b.opened_at + policy.max_age
    })
}

/// Returns whether adding a row of `bytes` would push a non-empty buffer past `max_bytes`. The
/// buffer then seals first, so a batch exceeds `max_bytes` only when it is a single oversized row.
pub(crate) fn overflows<R>(p: &Pending<R>, bytes: usize, policy: &SealPolicy) -> bool {
    p.0.as_ref()
        .is_some_and(|b| b.bytes + bytes > policy.max_bytes)
}

/// A run of rows sorted by key, with the key range a lookup prunes on. Only [`row_groups`] makes
/// one, so `min` and `max` are its first and last row's keys.
pub(crate) struct RowGroup<T: LogTable> {
    min: T::Key,
    max: T::Key,
    rows: Vec<T::Row>,
}

impl<T: LogTable> RowGroup<T> {
    pub(crate) fn may_contain(&self, key: &T::Key) -> bool {
        self.min <= *key && *key <= self.max
    }

    pub(crate) fn rows(&self) -> &[T::Row] {
        &self.rows
    }

    #[cfg(feature = "parquet")]
    pub(crate) fn into_rows(self) -> Vec<T::Row> {
        self.rows
    }

    /// Builds a group whose rows need not match its range or be sorted, as a file some other
    /// writer left may hold.
    #[cfg(test)]
    pub(crate) fn unchecked(min: T::Key, max: T::Key, rows: Vec<T::Row>) -> Self {
        Self { min, max, rows }
    }
}

/// Sorts a batch by `T::sort_key` and cuts it into `T::ROW_GROUP_ROWS` groups.
pub(crate) fn row_groups<T: LogTable>(mut rows: Vec<T::Row>) -> Vec<RowGroup<T>> {
    rows.sort_by_key(T::sort_key);

    let size = T::ROW_GROUP_ROWS.get();
    let mut groups = Vec::with_capacity(rows.len().div_ceil(size));
    let mut rest = rows.into_iter().peekable();
    while rest.peek().is_some() {
        let chunk: Vec<T::Row> = rest.by_ref().take(size).collect();
        if let (Some(first), Some(last)) = (chunk.first(), chunk.last()) {
            groups.push(RowGroup {
                min: T::sort_key(first),
                max: T::sort_key(last),
                rows: chunk,
            });
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::log::contract::{TestRow, TestTable};

    fn policy() -> SealPolicy {
        SealPolicy {
            max_rows: 3,
            max_bytes: 100,
            max_age: Duration::from_secs(1),
            grace: Duration::from_secs(600),
        }
    }

    #[test]
    fn empty_buffer_never_seals() {
        let p = Pending::<u8>::new();
        let far = Instant::now() + Duration::from_secs(3600);
        assert!(!seal_due(&p, far, &policy()));
        assert_eq!(p.deadline(&policy()), None);
    }

    #[test]
    fn seals_on_rows_bytes_and_age() {
        let t0 = Instant::now();
        let mut by_rows = Pending::new();
        (0..3).for_each(|i| by_rows.push(i, 1, t0));
        assert!(seal_due(&by_rows, t0, &policy()));

        let mut by_bytes = Pending::new();
        by_bytes.push(0, 100, t0);
        assert!(seal_due(&by_bytes, t0, &policy()));

        let mut by_age = Pending::new();
        by_age.push(0, 1, t0);
        assert!(!seal_due(
            &by_age,
            t0 + Duration::from_millis(999),
            &policy()
        ));
        assert!(seal_due(&by_age, t0 + Duration::from_secs(1), &policy()));
    }

    #[test]
    fn age_counts_from_the_oldest_row_and_resets_on_take() {
        let t0 = Instant::now();
        let mut p = Pending::new();
        p.push(0, 1, t0);
        p.push(1, 1, t0 + Duration::from_millis(500));
        assert_eq!(p.deadline(&policy()), Some(t0 + Duration::from_secs(1)));

        assert_eq!(p.take(), vec![0, 1]);
        assert_eq!(p.deadline(&policy()), None);
        assert!(!overflows(&p, 1_000, &policy()));
    }

    #[test]
    fn row_groups_are_sorted_and_carry_their_key_range() {
        let rows = [5u64, 1, 4, 2, 3].map(|k| TestRow::new(k, "x")).to_vec();
        let groups = row_groups::<TestTable>(rows);

        let ranges: Vec<(u64, u64, usize)> = groups
            .iter()
            .map(|g| (g.min, g.max, g.rows.len()))
            .collect();
        assert_eq!(ranges, vec![(1, 2, 2), (3, 4, 2), (5, 5, 1)]);
        assert!(groups[1].may_contain(&3) && !groups[1].may_contain(&5));
    }
}
