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

impl fmt::Display for KeyRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.start {
            Bound::Included(s) => write!(f, "[\"{}\"", Printable(s))?,
            Bound::Excluded(s) => write!(f, "(\"{}\"", Printable(s))?,
            Bound::Unbounded => f.write_str("(-inf")?,
        }
        match &self.end {
            Bound::Included(e) => write!(f, ", \"{}\"]", Printable(e))?,
            Bound::Excluded(e) => write!(f, ", \"{}\")", Printable(e))?,
            Bound::Unbounded => f.write_str(", +inf)")?,
        }
        if let Some(limit) = self.limit {
            write!(f, " limit {limit}")?;
        }
        if self.reverse {
            f.write_str(" reverse")?;
        }
        Ok(())
    }
}

impl fmt::Debug for KeyRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyRange({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Rng;

    fn map(keys: &[&[u8]]) -> BTreeMap<Vec<u8>, Vec<u8>> {
        keys.iter()
            .map(|k| (k.to_vec(), [b"v:", *k].concat()))
            .collect()
    }

    fn keys_of(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<Vec<u8>> {
        entries.into_iter().map(|(k, _)| k).collect()
    }

    fn v(keys: &[&[u8]]) -> Vec<Vec<u8>> {
        keys.iter().map(|k| k.to_vec()).collect()
    }

    #[test]
    fn prefix_ranges_stop_at_the_prefix_end() {
        let m = map(&[b"job", b"job/1", b"job/2", b"job0", b"jobs", b"a"]);
        let got = keys_of(KeyRange::prefix(b"job/").scan_map(&m));
        assert_eq!(got, v(&[b"job/1", b"job/2"]));
        let all_ff = map(&[&[0xFF], &[0xFF, 0xFF, 1], &[0xFE]]);
        let got = keys_of(KeyRange::prefix([0xFF]).scan_map(&all_ff));
        assert_eq!(got, vec![vec![0xFF], vec![0xFF, 0xFF, 1]]);
        assert_eq!(KeyRange::prefix(b"").scan_map(&m).len(), 6);
    }

    #[test]
    fn bounded_ranges_limit_and_reverse() {
        let m = map(&[b"a", b"b", b"c", b"d", b"e"]);
        let r = KeyRange::new(&b"b"[..]..&b"e"[..]);
        assert_eq!(keys_of(r.scan_map(&m)), v(&[b"b", b"c", b"d"]));
        assert_eq!(
            keys_of(KeyRange::new(&b"b"[..]..=&b"e"[..]).scan_map(&m)),
            v(&[b"b", b"c", b"d", b"e"])
        );
        assert_eq!(keys_of(r.clone().limit(2).scan_map(&m)), v(&[b"b", b"c"]));
        assert_eq!(
            keys_of(r.clone().reverse().limit(2).scan_map(&m)),
            v(&[b"d", b"c"])
        );
        assert!(r.clone().limit(0).scan_map(&m).is_empty());
        assert_eq!(
            keys_of(KeyRange::new("c"..).scan_map(&m)),
            v(&[b"c", b"d", b"e"])
        );
        assert_eq!(r.count_map(&m), 3);
        assert_eq!(r.clone().limit(1).count_map(&m), 1);
        assert_eq!(r.scan_map_keys(&m), v(&[b"b", b"c", b"d"]));
    }

    #[test]
    fn inverted_and_degenerate_ranges_are_empty_instead_of_panicking() {
        let m = map(&[b"a", b"b"]);
        for r in [
            KeyRange::new(&b"b"[..]..&b"a"[..]),
            KeyRange::new(&b"a"[..]..&b"a"[..]),
            KeyRange::from_bounds(
                Bound::Excluded(b"a".to_vec()),
                Bound::Excluded(b"a".to_vec()),
            ),
            KeyRange::from_bounds(
                Bound::Excluded(b"a".to_vec()),
                Bound::Excluded(b"a\0".to_vec()),
            ),
        ] {
            assert!(r.is_empty(), "{r}");
            assert!(r.scan_map(&m).is_empty(), "{r}");
            assert_eq!(r.count_map(&m), 0);
        }
        assert!(!KeyRange::new(&b"a"[..]..=&b"a"[..]).is_empty());
        assert!(!KeyRange::from_bounds(
            Bound::Excluded(b"a".to_vec()),
            Bound::Excluded(b"a\0\0".to_vec())
        )
        .is_empty());
    }

    #[test]
    fn display_shows_bounds_limit_and_direction() {
        assert_eq!(KeyRange::all().to_string(), "(-inf, +inf)");
        assert_eq!(
            KeyRange::prefix(b"q\x01").limit(5).reverse().to_string(),
            r#"["q\x01", "q\x02") limit 5 reverse"#
        );
        assert_eq!(
            format!("{:?}", KeyRange::new("a"..="b")),
            r#"KeyRange(["a", "b"])"#
        );
        assert_eq!(KeyRange::default(), KeyRange::all());
    }

    #[test]
    fn contains_agrees_with_scans() {
        let mut rng = Rng::seed_from_u64(99);
        let keys: Vec<Vec<u8>> = (0..200)
            .map(|_| {
                (0..rng.gen_range(0..3usize))
                    .map(|_| rng.gen_range(0..4u8))
                    .collect()
            })
            .collect();
        let m: BTreeMap<Vec<u8>, Vec<u8>> = keys.iter().map(|k| (k.clone(), vec![])).collect();
        let bound = |rng: &mut Rng| -> Bound<Vec<u8>> {
            let k: Vec<u8> = (0..rng.gen_range(0..3usize))
                .map(|_| rng.gen_range(0..4u8))
                .collect();
            match rng.gen_range(0..3u8) {
                0 => Bound::Included(k),
                1 => Bound::Excluded(k),
                _ => Bound::Unbounded,
            }
        };
        for _ in 0..500 {
            let r = KeyRange::from_bounds(bound(&mut rng), bound(&mut rng));
            let expected: Vec<Vec<u8>> = m.keys().filter(|k| r.contains(k)).cloned().collect();
            assert_eq!(r.scan_map_keys(&m), expected, "{r}");
            // `is_empty` may only claim emptiness when nothing can match.
            if r.is_empty() {
                assert!(expected.is_empty(), "{r}");
            }
        }
    }
}
