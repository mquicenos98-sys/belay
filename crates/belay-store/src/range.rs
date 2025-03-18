//! Key ranges for scans.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::{Bound, RangeBounds};

use crate::keys::{prefix_end, Printable};

/// The keys a [`scan`](crate::Kv::scan) visits, with an optional result
/// limit and direction.
///
/// ```
/// use belay_store::KeyRange;
///
/// let jobs = KeyRange::prefix(b"job/").limit(100);
/// assert!(jobs.contains(b"job/17"));
/// assert!(!jobs.contains(b"jobs"));
///
/// let window = KeyRange::new(&b"a"[..]..&b"m"[..]).reverse();
/// assert!(window.contains(b"k") && !window.contains(b"m"));
/// assert!(window.is_reverse());
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct KeyRange {
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    limit: Option<usize>,
    reverse: bool,
}

fn owned(bound: Bound<&impl AsRef<[u8]>>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Included(k) => Bound::Included(k.as_ref().to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.as_ref().to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn borrowed(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(k) => Bound::Included(k),
        Bound::Excluded(k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

impl KeyRange {
    /// Every key.
    #[must_use]
    pub fn all() -> KeyRange {
        KeyRange::from_bounds(Bound::Unbounded, Bound::Unbounded)
    }

    /// Keys starting with `prefix`.
    #[must_use]
    pub fn prefix(prefix: impl AsRef<[u8]>) -> KeyRange {
        let prefix = prefix.as_ref();
        let end = match prefix_end(prefix) {
            Some(end) => Bound::Excluded(end),
            None => Bound::Unbounded,
        };
        KeyRange::from_bounds(Bound::Included(prefix.to_vec()), end)
    }

    /// Keys within any standard range expression over byte-like keys:
    /// `KeyRange::new(&b"a"[..]..&b"c"[..])`, `KeyRange::new("a"..="c")`,
    /// `KeyRange::new(start.clone()..)`.
    #[must_use]
    pub fn new<K: AsRef<[u8]>>(range: impl RangeBounds<K>) -> KeyRange {
        KeyRange::from_bounds(owned(range.start_bound()), owned(range.end_bound()))
    }

    /// Keys between explicit bounds.
    #[must_use]
    pub fn from_bounds(start: Bound<Vec<u8>>, end: Bound<Vec<u8>>) -> KeyRange {
        KeyRange {
            start,
            end,
            limit: None,
            reverse: false,
        }
    }

    /// Returns at most `n` entries.
    #[must_use]
    pub fn limit(mut self, n: usize) -> KeyRange {
        self.limit = Some(n);
        self
    }

    /// Scans from the largest key down. With a [`limit`](Self::limit), this
    /// returns the *last* `n` keys of the range.
    #[must_use]
    pub fn reverse(mut self) -> KeyRange {
        self.reverse = true;
        self
    }

    /// Resumes a paginated scan: narrows the range to keys strictly above
    /// `key`. Passing the last key of the previous page yields the next
    /// page.
    #[must_use]
    pub fn after(mut self, key: impl AsRef<[u8]>) -> KeyRange {
        let key = key.as_ref().to_vec();
        let tighter = match &self.start {
            Bound::Unbounded => true,
            Bound::Included(start) | Bound::Excluded(start) => key >= *start,
        };
        if tighter {
            self.start = Bound::Excluded(key);
        }
        self
    }

    /// The lower bound.
    #[must_use]
    pub fn start_bound(&self) -> Bound<&[u8]> {
        borrowed(&self.start)
    }

    /// The upper bound.
    #[must_use]
    pub fn end_bound(&self) -> Bound<&[u8]> {
        borrowed(&self.end)
    }

    /// The result limit, if any.
    #[must_use]
    pub fn max_results(&self) -> Option<usize> {
        self.limit
    }

    /// Whether the scan runs from the largest key down.
    #[must_use]
    pub fn is_reverse(&self) -> bool {
        self.reverse
    }

    /// Whether `key` lies within the bounds (limit and direction aside).
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        let above_start = match &self.start {
            Bound::Included(s) => key >= s.as_slice(),
            Bound::Excluded(s) => key > s.as_slice(),
            Bound::Unbounded => true,
        };
        let below_end = match &self.end {
            Bound::Included(e) => key <= e.as_slice(),
            Bound::Excluded(e) => key < e.as_slice(),
            Bound::Unbounded => true,
        };
        above_start && below_end
    }

    /// Whether no key can lie within the bounds, e.g. `b..a` or `a..a`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match (&self.start, &self.end) {
            (Bound::Included(s), Bound::Included(e)) => s > e,
            (Bound::Included(s), Bound::Excluded(e)) | (Bound::Excluded(s), Bound::Included(e)) => {
                s >= e
            }
            // (s, e) exclusive on both ends is empty when nothing fits
            // between them: e is s itself or s followed by one zero byte.
            (Bound::Excluded(s), Bound::Excluded(e)) => {
                s >= e || (e.len() == s.len() + 1 && e.starts_with(s) && e[s.len()] == 0)
            }
            _ => false,
        }
    }

    /// Runs this range over a sorted map, copying out the matching entries.
    pub(crate) fn scan_map(&self, map: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.collect_map(map, |k, v| (k.clone(), v.clone()))
    }

    /// Like [`scan_map`](Self::scan_map), keys only.
    pub(crate) fn scan_map_keys(&self, map: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<Vec<u8>> {
        self.collect_map(map, |k, _| k.clone())
    }

    /// Number of entries the scan would return.
    pub(crate) fn count_map(&self, map: &BTreeMap<Vec<u8>, Vec<u8>>) -> usize {
        if self.is_empty() || self.limit == Some(0) {
            return 0;
        }
        let n = map
            .range::<[u8], _>((self.start_bound(), self.end_bound()))
            .count();
        self.limit.map_or(n, |limit| n.min(limit))
    }

    fn collect_map<T>(
        &self,
        map: &BTreeMap<Vec<u8>, Vec<u8>>,
        f: impl Fn(&Vec<u8>, &Vec<u8>) -> T,
    ) -> Vec<T> {
        // `BTreeMap::range` panics on inverted bounds, so those (and a zero
        // limit) are answered without touching the map.
        if self.is_empty() || self.limit == Some(0) {
            return Vec::new();
        }
        let iter = map.range::<[u8], _>((self.start_bound(), self.end_bound()));
        let limit = self.limit.unwrap_or(usize::MAX);
        if self.reverse {
            iter.rev().take(limit).map(|(k, v)| f(k, v)).collect()
        } else {
            iter.take(limit).map(|(k, v)| f(k, v)).collect()
        }
    }
}

impl Default for KeyRange {
    fn default() -> KeyRange {
        KeyRange::all()
    }
}
