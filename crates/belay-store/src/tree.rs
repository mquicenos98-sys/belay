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
