/// A position in the global event stream; [`since`](super::DurableStoreRead::since) returns the
/// events after it.
///
/// Its seq is never above `i64::MAX`, the most a SQLite durable store can store, and only
/// [`Cursor::after`] makes one, so every backend reads the same range for every cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor(i64);

/// A seq above `i64::MAX`, which no durable store hands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("seq {seq} is beyond what a durable store can store")]
pub struct CursorRangeError {
    pub seq: u64,
}

impl Cursor {
    /// The cursor before the first event.
    pub const START: Cursor = Cursor(0);

    /// Returns the cursor after the event numbered `seq`.
    ///
    /// # Errors
    ///
    /// [`CursorRangeError`] when `seq` is above `i64::MAX`.
    pub fn after(seq: u64) -> Result<Cursor, CursorRangeError> {
        i64::try_from(seq)
            .map(Cursor)
            .map_err(|_| CursorRangeError { seq })
    }

    /// Returns the seq of the last event this cursor has passed.
    pub fn seq(self) -> u64 {
        self.0.unsigned_abs()
    }

    pub(crate) fn column(self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_reaches_i64_max_and_refuses_one_more() {
        let last = i64::MAX.unsigned_abs();
        assert_eq!(Cursor::after(last).unwrap().seq(), last);
        assert_eq!(Cursor::after(last).unwrap().column(), i64::MAX);
        assert_eq!(
            Cursor::after(last + 1),
            Err(CursorRangeError { seq: last + 1 })
        );
    }

    #[test]
    fn the_start_is_the_cursor_after_seq_zero() {
        assert_eq!(Cursor::after(0), Ok(Cursor::START));
        assert_eq!(Cursor::START.seq(), 0);
    }
}
