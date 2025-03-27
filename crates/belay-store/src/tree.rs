//! Prefix-scoped views of a store.
//!
//! Every subsystem of belay keeps its records under its own prefix (jobs,
//! queue configs, schedules, workflows). A [`Namespace`] is such a prefix
//! plus the key arithmetic that goes with it; a [`Tree`] binds one to a
//! store for reads and single writes. Keys passed to and returned from a
//! `Tree` are relative to its prefix.
//!
//! Writes that span namespaces stay atomic by building one [`WriteBatch`]
//! through several namespaces:
//!
//! ```
//! use std::sync::Arc;
//! use belay_store::{Kv, MemKv, Namespace, Tree, WriteBatch};
//!
//! let kv: Arc<dyn Kv> = Arc::new(MemKv::new());
//! let jobs = Tree::open(Arc::clone(&kv), "job");
//! let uniq = Namespace::new("uniq");
//!
//! let mut batch = WriteBatch::new();
//! jobs.namespace().put(&mut batch, b"01J0", b"{\"state\":\"ready\"}");
//! uniq.put(&mut batch, b"invoice-7", b"01J0");
//! kv.write(batch).unwrap();
//!
//! assert!(jobs.contains(b"01J0").unwrap());
//! assert_eq!(jobs.len().unwrap(), 1);
//! assert_eq!(kv.get(&uniq.key(b"invoice-7")).unwrap().as_deref(), Some(&b"01J0"[..]));
//! ```

use std::fmt;
use std::ops::Bound;
use std::sync::Arc;

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::keys::{prefix_end, KeyBuf, Printable};
use crate::kv::Kv;
use crate::range::KeyRange;

/// A key prefix delimiting part of the key space.
///
/// [`Namespace::new`] encodes the name as a [`KeyBuf`] string element, so
/// sibling namespaces never overlap even when one name is a prefix of
/// another (`"job"` and `"jobs"`), and nested namespaces sort under their
/// parent.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Namespace {
    prefix: Vec<u8>,
}

impl Namespace {
    /// The namespace called `name`.
    #[must_use]
    pub fn new(name: &str) -> Namespace {
        Namespace {
            prefix: KeyBuf::new().str(name).into_vec(),
        }
    }

    /// A namespace with a hand-chosen raw prefix, e.g. `b"job/"`. The
    /// caller is responsible for keeping raw prefixes from overlapping.
    #[must_use]
    pub fn from_prefix(prefix: impl Into<Vec<u8>>) -> Namespace {
        Namespace {
            prefix: prefix.into(),
        }
    }

    /// The whole key space.
    #[must_use]
    pub fn root() -> Namespace {
        Namespace::default()
    }

    /// A namespace nested inside this one.
    #[must_use]
    pub fn child(&self, name: &str) -> Namespace {
        Namespace {
            prefix: KeyBuf::with_prefix(&self.prefix).str(name).into_vec(),
        }
    }

    /// The raw prefix.
    #[must_use]
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// The absolute key for relative `key`.
    #[must_use]
    pub fn key(&self, key: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.prefix.len() + key.len());
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(key);
        out
    }

    /// A key builder that starts with this prefix, for composite keys:
    /// `ns.key_buf().u64(seq).into_vec()`.
    #[must_use]
    pub fn key_buf(&self) -> KeyBuf {
        KeyBuf::with_prefix(&self.prefix)
    }

    /// The relative part of absolute `key`, if it lies in this namespace.
    #[must_use]
    pub fn strip<'a>(&self, key: &'a [u8]) -> Option<&'a [u8]> {
        key.strip_prefix(self.prefix.as_slice())
    }

    /// Whether absolute `key` lies in this namespace.
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        key.starts_with(&self.prefix)
    }

    /// Every key in the namespace.
    #[must_use]
    pub fn all(&self) -> KeyRange {
        KeyRange::prefix(&self.prefix)
    }

    /// Translates a range of relative keys into absolute keys, keeping its
    /// limit and direction. Unbounded ends stop at the namespace's edges.
    #[must_use]
    pub fn range(&self, relative: &KeyRange) -> KeyRange {
        let start = match relative.start_bound() {
            Bound::Included(k) => Bound::Included(self.key(k)),
            Bound::Excluded(k) => Bound::Excluded(self.key(k)),
            Bound::Unbounded => Bound::Included(self.prefix.clone()),
        };
        let end = match relative.end_bound() {
            Bound::Included(k) => Bound::Included(self.key(k)),
            Bound::Excluded(k) => Bound::Excluded(self.key(k)),
            Bound::Unbounded => match prefix_end(&self.prefix) {
                Some(end) => Bound::Excluded(end),
                None => Bound::Unbounded,
            },
        };
        let mut range = KeyRange::from_bounds(start, end);
        if let Some(limit) = relative.max_results() {
            range = range.limit(limit);
        }
        if relative.is_reverse() {
            range = range.reverse();
        }
        range
    }

    /// Adds a put of relative `key` to `batch`.
    pub fn put<'b>(
        &self,
        batch: &'b mut WriteBatch,
        key: &[u8],
        value: impl Into<Vec<u8>>,
    ) -> &'b mut WriteBatch {
        batch.put(self.key(key), value)
    }

    /// Adds a delete of relative `key` to `batch`.
    pub fn delete<'b>(&self, batch: &'b mut WriteBatch, key: &[u8]) -> &'b mut WriteBatch {
        batch.delete(self.key(key))
    }

    /// Adds the deletion of the whole namespace to `batch`.
    pub fn clear<'b>(&self, batch: &'b mut WriteBatch) -> &'b mut WriteBatch {
        batch.delete_prefix(self.prefix.clone())
    }
}

impl fmt::Debug for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Namespace(\"{}\")", Printable(&self.prefix))
    }
}

/// A [`Namespace`] bound to a store. Cheap to clone.
#[derive(Clone)]
pub struct Tree {
    kv: Arc<dyn Kv>,
    ns: Namespace,
}

impl Tree {
    /// A view of `kv` restricted to `ns`.
    #[must_use]
    pub fn new(kv: Arc<dyn Kv>, ns: Namespace) -> Tree {
        Tree { kv, ns }
    }

    /// A view of `kv` restricted to [`Namespace::new(name)`](Namespace::new).
    #[must_use]
    pub fn open(kv: Arc<dyn Kv>, name: &str) -> Tree {
        Tree::new(kv, Namespace::new(name))
    }

    /// A view of a namespace nested in this one.
    #[must_use]
    pub fn subtree(&self, name: &str) -> Tree {
        Tree::new(Arc::clone(&self.kv), self.ns.child(name))
    }

    /// The namespace.
    #[must_use]
    pub fn namespace(&self) -> &Namespace {
        &self.ns
    }

    /// The underlying store, e.g. to commit a batch spanning trees.
    #[must_use]
    pub fn kv(&self) -> &Arc<dyn Kv> {
        &self.kv
    }

    /// The value of relative `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.kv.get(&self.ns.key(key))
    }

    /// Whether relative `key` has a value.
    pub fn contains(&self, key: &[u8]) -> Result<bool> {
        self.kv.contains(&self.ns.key(key))
    }

    /// Entries in a range of relative keys, returned with relative keys.
    pub fn scan(&self, range: KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let entries = self.kv.scan(self.ns.range(&range))?;
        Ok(entries
            .into_iter()
            .map(|(k, v)| (self.relative(k), v))
            .collect())
    }

    /// Relative keys in a range of relative keys.
    pub fn scan_keys(&self, range: KeyRange) -> Result<Vec<Vec<u8>>> {
        let keys = self.kv.scan_keys(self.ns.range(&range))?;
        Ok(keys.into_iter().map(|k| self.relative(k)).collect())
    }

    /// Number of entries in a range of relative keys.
    pub fn count(&self, range: KeyRange) -> Result<usize> {
        self.kv.count(self.ns.range(&range))
    }

    /// Number of entries in the tree.
    pub fn len(&self) -> Result<usize> {
        self.kv.count(self.ns.all())
    }

    /// Whether the tree has no entries.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.kv.count(self.ns.all().limit(1))? == 0)
    }

    /// Writes one relative key.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<u64> {
        let mut batch = WriteBatch::with_capacity(1);
        self.ns.put(&mut batch, key, value);
        self.kv.write(batch)
    }

    /// Deletes one relative key.
    pub fn delete(&self, key: &[u8]) -> Result<u64> {
        let mut batch = WriteBatch::with_capacity(1);
        self.ns.delete(&mut batch, key);
        self.kv.write(batch)
    }

    /// Deletes every entry in the tree, atomically.
    pub fn clear(&self) -> Result<u64> {
        let mut batch = WriteBatch::with_capacity(1);
        self.ns.clear(&mut batch);
        self.kv.write(batch)
    }

    fn relative(&self, mut key: Vec<u8>) -> Vec<u8> {
        debug_assert!(self.ns.contains(&key), "scan stayed inside the namespace");
        key.drain(..self.ns.prefix.len());
        key
    }
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tree")
            .field("namespace", &self.ns)
            .field("kv", &self.kv)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemKv;

    fn store() -> Arc<dyn Kv> {
        Arc::new(MemKv::new())
    }

    fn keys(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<Vec<u8>> {
        entries.into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn sibling_namespaces_do_not_overlap() {
        let kv = store();
        let job = Tree::open(Arc::clone(&kv), "job");
        let jobs = Tree::open(Arc::clone(&kv), "jobs");
        job.put(b"1", b"a").expect("put");
        jobs.put(b"1", b"b").expect("put");
        jobs.put(b"2", b"c").expect("put");
        assert_eq!(job.len().expect("len"), 1);
        assert_eq!(jobs.len().expect("len"), 2);
        assert_eq!(job.get(b"1").expect("get"), Some(b"a".to_vec()));
        job.clear().expect("clear");
        assert!(job.is_empty().expect("empty"));
        assert_eq!(jobs.len().expect("len"), 2);
    }

    #[test]
    fn scans_use_relative_keys_and_keep_limit_and_direction() {
        let kv = store();
        let t = Tree::open(Arc::clone(&kv), "q");
        for k in ["a", "b", "c", "d"] {
            t.put(k.as_bytes(), b"").expect("put");
        }
        kv.put(b"zzz", b"outside").expect("put");
        assert_eq!(
            keys(t.scan(KeyRange::all()).expect("scan")),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
        assert_eq!(
            keys(
                t.scan(KeyRange::new("b"..).reverse().limit(2))
                    .expect("scan")
            ),
            vec![b"d".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            t.scan_keys(KeyRange::new(&b"a"[..]..&b"c"[..]))
                .expect("keys"),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert_eq!(t.count(KeyRange::prefix("c")).expect("count"), 1);
        let page = t.scan(KeyRange::all().after(b"b")).expect("page");
        assert_eq!(keys(page), vec![b"c".to_vec(), b"d".to_vec()]);
    }

    #[test]
    fn nested_trees_sort_under_their_parent() {
        let kv = store();
        let parent = Tree::open(Arc::clone(&kv), "wf");
        let child = parent.subtree("steps");
        child.put(b"x", b"1").expect("put");
        parent.put(b"meta", b"2").expect("put");
        assert_eq!(parent.len().expect("len"), 2);
        assert_eq!(child.len().expect("len"), 1);
        let full = child.namespace().key(b"x");
        assert!(parent.namespace().contains(&full));
        assert_eq!(child.namespace().strip(&full), Some(&b"x"[..]));
        assert_eq!(parent.namespace().strip(b"other"), None);
    }

    #[test]
    fn raw_prefixes_and_the_root() {
        let kv = store();
        let raw = Tree::new(Arc::clone(&kv), Namespace::from_prefix(b"job/".to_vec()));
        raw.put(b"7", b"x").expect("put");
        assert_eq!(kv.get(b"job/7").expect("get"), Some(b"x".to_vec()));
        let top = Tree::new(Arc::clone(&kv), Namespace::from_prefix(vec![0xFF]));
        top.put(&[0xFF, 1], b"y").expect("put");
        assert_eq!(top.len().expect("len"), 1);
        assert_eq!(
            top.namespace().range(&KeyRange::all()).end_bound(),
            Bound::Unbounded
        );
        let root = Tree::new(Arc::clone(&kv), Namespace::root());
        assert_eq!(root.len().expect("len"), 2);
    }

    #[test]
    fn composite_keys_through_key_buf() {
        let ns = Namespace::new("ready");
        let key = ns.key_buf().i32(5).u64(9).into_vec();
        assert!(ns.contains(&key));
        let mut r = crate::keys::KeyReader::after_prefix(&key, ns.prefix()).expect("prefix");
        assert_eq!(r.i32(), Ok(5));
        assert_eq!(r.u64(), Ok(9));
        assert_eq!(format!("{ns:?}"), r#"Namespace("ready\x00\x01")"#);
    }

    #[test]
    fn range_translation_keeps_bound_kinds() {
        let ns = Namespace::from_prefix(b"p/".to_vec());
        let r = ns.range(&KeyRange::from_bounds(
            Bound::Excluded(b"a".to_vec()),
            Bound::Included(b"z".to_vec()),
        ));
        assert_eq!(r.start_bound(), Bound::Excluded(&b"p/a"[..]));
        assert_eq!(r.end_bound(), Bound::Included(&b"p/z"[..]));
        let all = ns.range(&KeyRange::all());
        assert_eq!(all.start_bound(), Bound::Included(&b"p/"[..]));
        assert_eq!(all.end_bound(), Bound::Excluded(&b"p0"[..]));
    }
}
