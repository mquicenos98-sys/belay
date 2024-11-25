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

impl Iterator for IntoIter {
    type Item = (String, Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl DoubleEndedIterator for IntoIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back()
    }
}

impl ExactSizeIterator for IntoIter {}
impl FusedIterator for IntoIter {}

/// A view into one key of a [`Map`], from [`Map::entry`].
#[derive(Debug)]
pub enum Entry<'a> {
    /// The key is absent.
    Vacant(VacantEntry<'a>),
    /// The key is present.
    Occupied(OccupiedEntry<'a>),
}

impl<'a> Entry<'a> {
    /// The entry's key.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Entry::Vacant(e) => e.key(),
            Entry::Occupied(e) => e.key(),
        }
    }

    /// Inserts `default` if the key is absent; returns the value either way.
    pub fn or_insert(self, default: impl Into<Value>) -> &'a mut Value {
        match self {
            Entry::Vacant(e) => e.insert(default),
            Entry::Occupied(e) => e.into_mut(),
        }
    }

    /// Inserts the result of `default` if the key is absent.
    pub fn or_insert_with<V: Into<Value>>(self, default: impl FnOnce() -> V) -> &'a mut Value {
        match self {
            Entry::Vacant(e) => e.insert(default()),
            Entry::Occupied(e) => e.into_mut(),
        }
    }

    /// Runs `update` on the value if the key is present.
    #[must_use]
    pub fn and_modify(self, update: impl FnOnce(&mut Value)) -> Entry<'a> {
        match self {
            Entry::Occupied(mut e) => {
                update(e.get_mut());
                Entry::Occupied(e)
            }
            vacant => vacant,
        }
    }
}

/// An absent key of a [`Map`].
#[derive(Debug)]
pub struct VacantEntry<'a> {
    map: &'a mut Map,
    key: String,
}

impl<'a> VacantEntry<'a> {
    /// The key that would be inserted.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Inserts the value at the end of the map.
    pub fn insert(self, value: impl Into<Value>) -> &'a mut Value {
        let position = self.map.push_new(self.key, value.into());
        &mut self.map.entries[position].1
    }
}

/// A present key of a [`Map`].
#[derive(Debug)]
pub struct OccupiedEntry<'a> {
    map: &'a mut Map,
    position: usize,
}

impl<'a> OccupiedEntry<'a> {
    /// The entry's key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.map.entries[self.position].0
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> &Value {
        &self.map.entries[self.position].1
    }

    /// The current value, mutably.
    pub fn get_mut(&mut self) -> &mut Value {
        &mut self.map.entries[self.position].1
    }

    /// Converts into a mutable reference tied to the map.
    #[must_use]
    pub fn into_mut(self) -> &'a mut Value {
        &mut self.map.entries[self.position].1
    }

    /// Replaces the value, returning the old one.
    pub fn insert(&mut self, value: impl Into<Value>) -> Value {
        mem::replace(self.get_mut(), value.into())
    }

    /// Removes the entry, returning its value.
    pub fn remove(self) -> Value {
        self.map.entries.remove(self.position).1
    }
}

/// Hash index from key hash to entry positions.
///
/// Keys are hashed once with a per-map `RandomState` (so adversarial keys
/// cannot be precomputed to collide) and the resulting `u64` is used directly
/// as the key of the inner table. Distinct keys may still share a 64-bit hash,
/// so a slot can hold several positions and lookups always compare the key.
#[derive(Clone)]
struct Index {
    hasher: RandomState,
    slots: HashMap<u64, Slot, BuildHasherDefault<PrehashedHasher>>,
}

#[derive(Clone)]
enum Slot {
    One(usize),
    Many(Vec<usize>),
}

impl Index {
    fn build(entries: &[(String, Value)]) -> Index {
        let mut index = Index {
            hasher: RandomState::new(),
            slots: HashMap::with_capacity_and_hasher(entries.len(), Default::default()),
        };
        for (position, (key, _)) in entries.iter().enumerate() {
            index.add(key, position);
        }
        index
    }

    fn hash(&self, key: &str) -> u64 {
        self.hasher.hash_one(key)
    }

    fn find(&self, entries: &[(String, Value)], key: &str) -> Option<usize> {
        let matches = |&p: &usize| entries[p].0 == key;
        match self.slots.get(&self.hash(key))? {
            Slot::One(p) => Some(*p).filter(matches),
            Slot::Many(ps) => ps.iter().copied().find(matches),
        }
    }

    fn add(&mut self, key: &str, position: usize) {
        let hash = self.hash(key);
        match self.slots.get_mut(&hash) {
            None => {
                self.slots.insert(hash, Slot::One(position));
            }
            Some(slot) => match slot {
                Slot::One(p) => *slot = Slot::Many(vec![*p, position]),
                Slot::Many(ps) => ps.push(position),
            },
        }
    }

    /// Forgets `key` at `position` and shifts every later position down by
    /// one, mirroring `Vec::remove` on the entries.
    fn remove(&mut self, key: &str, position: usize) {
        let hash = self.hash(key);
        let emptied = match self.slots.get_mut(&hash) {
            Some(Slot::One(_)) => true,
            Some(Slot::Many(ps)) => {
                ps.retain(|&p| p != position);
                ps.is_empty()
            }
            None => false,
        };
        if emptied {
            self.slots.remove(&hash);
        }
        let shift = |p: &mut usize| {
            if *p > position {
                *p -= 1;
            }
        };
        for slot in self.slots.values_mut() {
            match slot {
                Slot::One(p) => shift(p),
                Slot::Many(ps) => ps.iter_mut().for_each(shift),
            }
        }
    }
}

/// Hasher for keys that are already uniformly distributed `u64` hashes.
#[derive(Default)]
struct PrehashedHasher(u64);

impl Hasher for PrehashedHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // Only `write_u64` is used by `HashMap<u64, _>`; fold anything else
        // FNV-style so the hasher stays correct if that ever changes.
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> Map {
        (0..n).map(|i| (format!("k{i}"), i)).collect()
    }

    #[test]
    fn preserves_insertion_order_and_replace_keeps_position() {
        let mut map = Map::new();
        map.insert("b", 1);
        map.insert("a", 2);
        map.insert("c", 3);
        assert_eq!(map.insert("a", 20), Some(Value::from(2)));
        let entries: Vec<(&str, &Value)> = map.iter().collect();
        assert_eq!(
            entries,
            [
                ("b", &Value::from(1)),
                ("a", &Value::from(20)),
                ("c", &Value::from(3))
            ]
        );
    }

    #[test]
    fn lookups_work_on_both_sides_of_index_threshold() {
        for n in [0, 1, INDEX_THRESHOLD, INDEX_THRESHOLD + 1, 200] {
            let map = numbered(n);
            assert_eq!(map.len(), n);
            assert_eq!(map.index.is_some(), n > INDEX_THRESHOLD);
            for i in 0..n {
                assert_eq!(
                    map.get(&format!("k{i}")),
                    Some(&Value::from(i)),
                    "n={n} i={i}"
                );
                assert_eq!(map.get_index_of(&format!("k{i}")), Some(i));
            }
            assert!(map.get("missing").is_none());
        }
    }

    #[test]
    fn remove_keeps_order_and_index_consistent() {
        let mut map = numbered(40);
        assert_eq!(map.remove("k0"), Some(Value::from(0)));
        assert_eq!(map.remove("k17"), Some(Value::from(17)));
        assert_eq!(map.remove("k39"), Some(Value::from(39)));
        assert_eq!(map.remove("k17"), None);
        assert_eq!(map.len(), 37);
        let expected: Vec<String> = (1..39)
            .filter(|&i| i != 17)
            .map(|i| format!("k{i}"))
            .collect();
        assert_eq!(map.keys().collect::<Vec<_>>(), expected);
        for (position, key) in expected.iter().enumerate() {
            assert_eq!(map.get_index_of(key), Some(position));
        }
        // Re-inserting a removed key appends it.
        map.insert("k17", 17);
        assert_eq!(map.get_index_of("k17"), Some(37));
    }

    #[test]
    fn colliding_slot_positions_are_tracked() {
        // Force every key into one slot to exercise `Slot::Many`.
        let mut index = Index::build(&[]);
        let entries: Vec<(String, Value)> = ["x", "y", "z"]
            .iter()
            .map(|k| (k.to_string(), Value::Null))
            .collect();
        let hash = index.hash("x");
        index.slots.insert(hash, Slot::Many(vec![0, 1, 2]));
        assert_eq!(index.find(&entries, "x"), Some(0));
        index
            .slots
            .insert(index.hash("z"), Slot::Many(vec![0, 1, 2]));
        assert_eq!(index.find(&entries, "z"), Some(2));
    }

    #[test]
    fn equality_ignores_order() {
        let a: Map = [("x", 1), ("y", 2)].into_iter().collect();
        let b: Map = [("y", 2), ("x", 1)].into_iter().collect();
        let c: Map = [("x", 1), ("y", 3)].into_iter().collect();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, numbered(2));
    }

    #[test]
    fn retain_and_sort_rebuild_index() {
        let mut map = numbered(30);
        map.retain(|_, v| v.as_u64().is_some_and(|n| n % 3 == 0));
        assert_eq!(map.len(), 10);
        assert_eq!(map.get_index_of("k27"), Some(9));
        map.sort_keys();
        // Bytewise: "k0" < "k12" < "k15" < ... < "k3" < ...
        assert_eq!(map.get_index(0).map(|(k, _)| k), Some("k0"));
        assert_eq!(map.get_index(1).map(|(k, _)| k), Some("k12"));
        assert_eq!(map.get("k12"), Some(&Value::from(12)));
        for (position, key) in map.keys().enumerate() {
            assert_eq!(map.get_index_of(key), Some(position));
        }
    }

    #[test]
    fn iterators_are_double_ended_and_sized() {
        let mut map = numbered(3);
        assert_eq!(map.iter().len(), 3);
        assert_eq!(map.iter().next_back().map(|(k, _)| k), Some("k2"));
        for (_, v) in map.iter_mut() {
            *v = Value::Null;
        }
        assert!(map.values().all(Value::is_null));
        let owned: Vec<(String, Value)> = map.into_iter().rev().collect();
        assert_eq!(owned[0].0, "k2");
    }

    #[test]
    fn clear_drops_index() {
        let mut map = numbered(20);
        map.clear();
        assert!(map.is_empty());
        assert!(map.index.is_none());
        map.insert("a", 1);
        assert_eq!(map.get("a"), Some(&Value::from(1)));
    }
}
