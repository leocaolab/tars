//! Builds the columns whose offsets Arrow keeps as `i32`: string, binary and list-of-string
//! columns. Arrow's own builders panic when a column outgrows its offsets; these check every
//! length against an [`OffsetLimit`] before copying anything and return an [`EncodeError`]
//! naming the column and the row instead. The on-disk types stay `Utf8`, `Binary` and `List`,
//! not their 64-bit forms.

use std::sync::Arc;

use arrow_array::builder::{ListBuilder, StringBuilder};
use arrow_array::{ArrayRef, BinaryArray, StringArray};
use arrow_schema::FieldRef;

use super::EncodeError;

/// The last offset a column's values may reach: bytes for a string or binary column, items for a
/// list column. It is never past [`OffsetLimit::ARROW`], so a column within it never overflows
/// Arrow's offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetLimit(usize);

impl OffsetLimit {
    /// Arrow's own limit: string, binary and list offsets are `i32`.
    pub const ARROW: Self = Self(i32::MAX as usize);

    /// Returns a limit of `max`, or `None` when `max` is past [`OffsetLimit::ARROW`].
    #[cfg(test)]
    pub(crate) fn new(max: usize) -> Option<Self> {
        (max <= Self::ARROW.0).then_some(Self(max))
    }
}

/// Returns where a value of `len` ends when it starts at offset `start`, or `None` when it ends
/// past `max`.
fn offset_end(start: usize, len: usize, max: usize) -> Option<usize> {
    start.checked_add(len).filter(|&end| end <= max)
}

/// Returns the row of the first value that ends past `limit` when the `(row, len)` values are
/// laid end to end.
fn row_past_offsets(
    lens: impl IntoIterator<Item = (usize, usize)>,
    limit: OffsetLimit,
) -> Option<usize> {
    lens.into_iter()
        .try_fold(0, |start, (row, len)| {
            offset_end(start, len, limit.0).ok_or(row)
        })
        .err()
}

/// Returns the string column `column` of `values`.
///
/// # Errors
///
/// [`EncodeError::TextTooLong`] when the text is longer than `limit` reaches.
pub(crate) fn strs<'a>(
    limit: OffsetLimit,
    column: &'static str,
    values: impl Iterator<Item = Option<&'a str>>,
) -> Result<ArrayRef, EncodeError> {
    let values: Vec<Option<&str>> = values.collect();
    let lens = values.iter().map(|v| v.map_or(0, str::len)).enumerate();
    match row_past_offsets(lens, limit) {
        Some(row) => Err(EncodeError::TextTooLong { column, row }),
        None => Ok(Arc::new(StringArray::from(values))),
    }
}

/// Returns the binary column `column` of `values`.
///
/// # Errors
///
/// [`EncodeError::BytesTooLong`] when the bytes are more than `limit` reaches.
pub(crate) fn binaries<'a>(
    limit: OffsetLimit,
    column: &'static str,
    values: impl Iterator<Item = Option<&'a [u8]>>,
) -> Result<ArrayRef, EncodeError> {
    let values: Vec<Option<&[u8]>> = values.collect();
    let lens = values.iter().map(|v| v.map_or(0, <[u8]>::len)).enumerate();
    match row_past_offsets(lens, limit) {
        Some(row) => Err(EncodeError::BytesTooLong { column, row }),
        None => Ok(Arc::new(BinaryArray::from(values))),
    }
}

/// Returns the list column `column` of `rows`, a list of strings per row and null where a row has
/// none; `item` is the list's item field.
///
/// # Errors
///
/// [`EncodeError::ListTooLong`] when the items are more than `limit` counts, and
/// [`EncodeError::TextTooLong`] when their text is longer than `limit` reaches.
pub(crate) fn string_lists<'a>(
    limit: OffsetLimit,
    column: &'static str,
    item: FieldRef,
    rows: impl Iterator<Item = Option<&'a [String]>>,
) -> Result<ArrayRef, EncodeError> {
    let rows: Vec<Option<&[String]>> = rows.collect();
    let counts = rows
        .iter()
        .map(|r| r.map_or(0, <[String]>::len))
        .enumerate();
    if let Some(row) = row_past_offsets(counts, limit) {
        return Err(EncodeError::ListTooLong { column, row });
    }
    let texts = rows.iter().enumerate().flat_map(|(row, items)| {
        items
            .iter()
            .flat_map(|items| items.iter())
            .map(move |s| (row, s.len()))
    });
    if let Some(row) = row_past_offsets(texts, limit) {
        return Err(EncodeError::TextTooLong { column, row });
    }

    let mut b = ListBuilder::new(StringBuilder::new()).with_field(item);
    for list in rows {
        if let Some(items) = list {
            items.iter().for_each(|s| b.values().append_value(s));
        }
        b.append(list.is_some());
    }
    Ok(Arc::new(b.finish()))
}

#[cfg(test)]
mod tests {
    use arrow_array::Array;
    use arrow_schema::{DataType, Field};

    use super::*;

    /// Two values of four fit, the third ends at 12.
    fn ten() -> OffsetLimit {
        OffsetLimit::new(10).unwrap()
    }

    #[test]
    fn a_value_ending_at_the_limit_fits_and_one_more_does_not() {
        assert_eq!(offset_end(9, 1, 10), Some(10));
        assert_eq!(offset_end(9, 2, 10), None);
        assert_eq!(offset_end(usize::MAX, 1, usize::MAX), None);
    }

    #[test]
    fn a_limit_past_arrows_offsets_is_refused() {
        let arrow = i32::MAX as usize;
        assert_eq!(OffsetLimit::new(arrow), Some(OffsetLimit::ARROW));
        assert_eq!(OffsetLimit::new(arrow + 1), None);
    }

    #[test]
    fn the_first_row_past_the_limit_is_the_one_named() {
        let lens = [(0, 9), (1, 1), (2, 1), (3, 1)];
        assert_eq!(row_past_offsets(lens, ten()), Some(2));
        assert_eq!(row_past_offsets(lens[..2].iter().copied(), ten()), None);
    }

    #[test]
    fn a_string_column_longer_than_its_limit_names_the_column_and_row() {
        let fits = strs(ten(), "body", std::iter::repeat_n(Some("abcd"), 2));
        let too_long = strs(ten(), "body", std::iter::repeat_n(Some("abcd"), 3));

        assert_eq!(fits.unwrap().len(), 2);
        assert!(
            matches!(
                too_long,
                Err(EncodeError::TextTooLong {
                    column: "body",
                    row: 2
                })
            ),
            "{too_long:?}"
        );
    }

    #[test]
    fn a_binary_column_longer_than_its_limit_names_the_column_and_row() {
        let fits = binaries(ten(), "bytes", [Some(b"abcd".as_slice()), None].into_iter());
        let too_long = binaries(
            ten(),
            "bytes",
            std::iter::repeat_n(Some(b"abcd".as_slice()), 3),
        );

        assert_eq!(fits.unwrap().null_count(), 1);
        assert!(
            matches!(
                too_long,
                Err(EncodeError::BytesTooLong {
                    column: "bytes",
                    row: 2
                })
            ),
            "{too_long:?}"
        );
    }

    #[test]
    fn a_list_column_whose_text_is_longer_than_its_limit_names_the_column_and_row() {
        let item = Arc::new(Field::new("item", DataType::Utf8, false));
        let list = vec!["abcd".to_owned()];

        let fits = string_lists(
            ten(),
            "tags",
            item.clone(),
            [Some(list.as_slice()), None].into_iter(),
        );
        let too_long = string_lists(
            ten(),
            "tags",
            item,
            std::iter::repeat_n(Some(list.as_slice()), 3),
        );

        assert_eq!(fits.unwrap().null_count(), 1);
        assert!(
            matches!(
                too_long,
                Err(EncodeError::TextTooLong {
                    column: "tags",
                    row: 2
                })
            ),
            "{too_long:?}"
        );
    }

    #[test]
    fn a_list_column_with_more_items_than_its_limit_names_the_column_and_row() {
        let item = Arc::new(Field::new("item", DataType::Utf8, true));
        let list = vec![String::new(); 4];

        let too_many = string_lists(
            ten(),
            "tags",
            item,
            std::iter::repeat_n(Some(list.as_slice()), 3),
        );

        assert!(
            matches!(
                too_many,
                Err(EncodeError::ListTooLong {
                    column: "tags",
                    row: 2
                })
            ),
            "{too_many:?}"
        );
    }
}
