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
