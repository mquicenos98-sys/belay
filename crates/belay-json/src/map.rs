//! Insertion-ordered JSON objects.
//!
//! Entries live in a `Vec`, so iteration order is the order keys were first
//! inserted (or appeared in the parsed document) and small objects - the vast
//! majority - are searched linearly without any hashing. Once an object grows
//! past `INDEX_THRESHOLD` (8) entries a hash index is built next to the entries;
//! without it, parsing an attacker-supplied object with a hundred thousand
//! keys would be quadratic, since every insert must check for duplicates.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
use std::iter::FusedIterator;
use std::mem;

use crate::Value;

/// Objects with more entries than this get a hash index.
const INDEX_THRESHOLD: usize = 8;

/// An insertion-ordered map from `String` keys to [`Value`]s.
///
/// Inserting an existing key replaces its value but keeps its original
/// position. Equality ignores order: two maps are equal when they hold the
/// same keys with equal values, because JSON objects are unordered.
///
/// ```
/// use belay_json::{Map, Value};
///
/// let mut map = Map::new();
/// map.insert("queue", "emails");
/// map.insert("priority", 5);
/// map.insert("queue", "reports");
///
/// let keys: Vec<&str> = map.keys().collect();
/// assert_eq!(keys, ["queue", "priority"]);
/// assert_eq!(map.get("queue"), Some(&Value::from("reports")));
/// ```
#[derive(Clone, Default)]
pub struct Map {
    entries: Vec<(String, Value)>,
    index: Option<Index>,
}

impl Map {
    /// Creates an empty map.
    #[must_use]
    pub fn new() -> Map {
        Map::default()
    }

    /// Creates an empty map with room for `capacity` entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Map {
        Map {
            entries: Vec::with_capacity(capacity),
            index: None,
        }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the map has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.index = None;
    }

    /// Position of `key` in iteration order.
    #[must_use]
    pub fn get_index_of(&self, key: &str) -> Option<usize> {
        match &self.index {
            Some(index) => index.find(&self.entries, key),
            None => self.entries.iter().position(|(k, _)| k == key),
        }
    }

    /// The value for `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.get_index_of(key).map(|i| &self.entries[i].1)
    }

    /// The value for `key`, mutably.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.get_index_of(key).map(move |i| &mut self.entries[i].1)
    }

    /// The stored key and value for `key`.
    #[must_use]
    pub fn get_key_value(&self, key: &str) -> Option<(&str, &Value)> {
        self.get_index_of(key).map(|i| {
            let (k, v) = &self.entries[i];
            (k.as_str(), v)
        })
    }

    /// The entry at position `index` in iteration order.
    #[must_use]
    pub fn get_index(&self, index: usize) -> Option<(&str, &Value)> {
        self.entries.get(index).map(|(k, v)| (k.as_str(), v))
    }

    /// True if the map has an entry for `key`.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get_index_of(key).is_some()
    }

    /// Inserts or replaces the value for `key`, returning the previous value.
    /// A replaced key keeps its position.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();
        match self.get_index_of(&key) {
            Some(i) => Some(mem::replace(&mut self.entries[i].1, value)),
            None => {
                self.push_new(key, value);
                None
            }
        }
    }

    /// Appends an entry whose key is known to be absent; returns its position.
    fn push_new(&mut self, key: String, value: Value) -> usize {
        let position = self.entries.len();
        if let Some(index) = &mut self.index {
            index.add(&key, position);
        }
        self.entries.push((key, value));
        if self.index.is_none() && self.entries.len() > INDEX_THRESHOLD {
            self.index = Some(Index::build(&self.entries));
        }
        position
    }

    /// Removes `key`, returning its value. Later entries shift down by one,
    /// so this is O(n); order is preserved.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.remove_entry(key).map(|(_, v)| v)
    }

    /// Removes `key`, returning the stored key and value.
    pub fn remove_entry(&mut self, key: &str) -> Option<(String, Value)> {
        let position = self.get_index_of(key)?;
        Some(self.remove_at(position))
    }

    fn remove_at(&mut self, position: usize) -> (String, Value) {
        let (key, value) = self.entries.remove(position);
        if let Some(index) = &mut self.index {
            index.remove(&key, position);
        }
        (key, value)
    }

    /// Keeps only the entries for which `keep` returns true, preserving order.
    pub fn retain(&mut self, mut keep: impl FnMut(&str, &mut Value) -> bool) {
        self.entries.retain_mut(|(k, v)| keep(k, v));
        self.reindex();
    }

    /// Sorts entries by key (bytewise), e.g. for canonical output.
    pub fn sort_keys(&mut self) {
        self.entries.sort_by(|a, b| a.0.cmp(&b.0));
        self.reindex();
    }

    fn reindex(&mut self) {
        if self.index.is_some() {
            self.index = Some(Index::build(&self.entries));
        }
    }

    /// The entry for `key`, for in-place insert-or-update.
    ///
    /// ```
    /// use belay_json::{Map, Value};
    ///
    /// let mut counts = Map::new();
    /// for word in ["a", "b", "a"] {
    ///     let n = counts.entry(word).or_insert(0).as_u64().unwrap_or(0);
    ///     counts.insert(word, n + 1);
    /// }
    /// assert_eq!(counts.get("a"), Some(&Value::from(2)));
    /// ```
    pub fn entry(&mut self, key: impl Into<String>) -> Entry<'_> {
        let key = key.into();
        match self.get_index_of(&key) {
            Some(position) => Entry::Occupied(OccupiedEntry {
                map: self,
                position,
            }),
            None => Entry::Vacant(VacantEntry { map: self, key }),
        }
    }

    /// Iterates over entries in order.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            inner: self.entries.iter(),
        }
    }

    /// Iterates over entries in order with mutable values.
    pub fn iter_mut(&mut self) -> IterMut<'_> {
        IterMut {
            inner: self.entries.iter_mut(),
        }
    }

    /// Iterates over keys in order.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator + '_ {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// Iterates over values in order.
    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Value> + ExactSizeIterator + '_ {
        self.entries.iter().map(|(_, v)| v)
    }

    /// Iterates over values in order, mutably.
    pub fn values_mut(
        &mut self,
    ) -> impl DoubleEndedIterator<Item = &mut Value> + ExactSizeIterator + '_ {
        self.entries.iter_mut().map(|(_, v)| v)
    }
}

impl PartialEq for Map {
    fn eq(&self, other: &Map) -> bool {
        // Keys are unique, so equal length plus "every entry of self is in
        // other" is enough for set equality.
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl Eq for Map {}

impl fmt::Debug for Map {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Map {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Map {
        let mut map = Map::new();
        map.extend(iter);
        map
    }
}

impl<K: Into<String>, V: Into<Value>> Extend<(K, V)> for Map {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<'a> IntoIterator for &'a Map {
    type Item = (&'a str, &'a Value);
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut Map {
    type Item = (&'a str, &'a mut Value);
    type IntoIter = IterMut<'a>;

    fn into_iter(self) -> IterMut<'a> {
        self.iter_mut()
    }
}

impl IntoIterator for Map {
    type Item = (String, Value);
    type IntoIter = IntoIter;

    fn into_iter(self) -> IntoIter {
        IntoIter {
            inner: self.entries.into_iter(),
        }
    }
}

/// Iterator over `(&str, &Value)` entries of a [`Map`].
#[derive(Debug, Clone)]
pub struct Iter<'a> {
    inner: std::slice::Iter<'a, (String, Value)>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a str, &'a Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, v)| (k.as_str(), v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|(k, v)| (k.as_str(), v))
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl FusedIterator for Iter<'_> {}

/// Iterator over `(&str, &mut Value)` entries of a [`Map`].
#[derive(Debug)]
pub struct IterMut<'a> {
    inner: std::slice::IterMut<'a, (String, Value)>,
}

impl<'a> Iterator for IterMut<'a> {
    type Item = (&'a str, &'a mut Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, v)| (k.as_str(), v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl DoubleEndedIterator for IterMut<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|(k, v)| (k.as_str(), v))
    }
}

impl ExactSizeIterator for IterMut<'_> {}

impl FusedIterator for IterMut<'_> {}

/// Owning iterator over `(String, Value)` entries of a [`Map`].
#[derive(Debug)]
pub struct IntoIter {
    inner: std::vec::IntoIter<(String, Value)>,
}
