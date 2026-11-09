//! A list that has at least one item.

use serde::{Deserialize, Serialize};

/// A list with at least one item; every way to make one starts from an item, and a list read
/// from the wire that names none does not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<T>", into = "Vec<T>")]
#[serde(bound(
    serialize = "T: Serialize + Clone",
    deserialize = "T: Deserialize<'de>"
))]
pub struct NonEmpty<T>(Vec<T>);

/// A list that has no item where one is needed.
#[derive(Debug, thiserror::Error)]
#[error("the list must have at least one item")]
pub struct EmptyListError;

impl<T> NonEmpty<T> {
    /// Returns the list of `first` followed by `rest`.
    pub fn new(first: T, rest: impl IntoIterator<Item = T>) -> Self {
        let mut items = vec![first];
        items.extend(rest);
        Self(items)
    }

    /// Returns the list of `items`, or `None` when it has none.
    pub fn from_vec(items: Vec<T>) -> Option<Self> {
        (!items.is_empty()).then_some(Self(items))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T> std::ops::Deref for NonEmpty<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = EmptyListError;

    fn try_from(items: Vec<T>) -> Result<Self, Self::Error> {
        Self::from_vec(items).ok_or(EmptyListError)
    }
}

impl<T> From<NonEmpty<T>> for Vec<T> {
    fn from(list: NonEmpty<T>) -> Self {
        list.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_does_not_read_and_a_full_one_reads_back() {
        assert!(serde_json::from_str::<NonEmpty<u8>>("[]").is_err());
        let list = NonEmpty::new(1u8, [2]);
        let back: NonEmpty<u8> =
            serde_json::from_str(&serde_json::to_string(&list).unwrap()).unwrap();
        assert_eq!(back, list);
        assert_eq!(NonEmpty::<u8>::from_vec(Vec::new()), None);
    }
}
