//! Atomic groups of writes and their log encoding (specified in
//! `crate::format`, under "Write batch payload").

use std::ops::Bound;

use belay_common::codec::{DecodeError, Decoder, Encoder};

use crate::error::{Result, StoreError};
use crate::keys::prefix_end;

/// Largest accepted key, in bytes.
pub const MAX_KEY_LEN: usize = 64 * 1024;

/// Largest accepted value, in bytes.
pub const MAX_VALUE_LEN: usize = 32 * 1024 * 1024;

/// Largest accepted encoded batch, in bytes. This also bounds the length
/// of a log record, which lets recovery reject absurd lengths read from a
/// damaged header without trying to allocate them.
pub const MAX_BATCH_LEN: usize = 128 * 1024 * 1024;

const TAG_PUT: u8 = 1;

const TAG_DELETE: u8 = 2;

const TAG_DELETE_RANGE: u8 = 3;

const TAG_DELETE_FROM: u8 = 4;

/// One write in a [`WriteBatch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Sets `key` to `value`.
    Put {
        /// The key.
        key: Vec<u8>,
        /// The new value.
        value: Vec<u8>,
    },
    /// Removes `key` if present.
    Delete {
        /// The key.
        key: Vec<u8>,
    },
    /// Removes every key `k` with `start <= k < end` (or `start <= k` when
    /// `end` is `None`). An empty or inverted range removes nothing.
    DeleteRange {
        /// Inclusive lower bound.
        start: Vec<u8>,
        /// Exclusive upper bound; `None` for the end of the key space.
        end: Option<Vec<u8>>,
    },
}

impl Op {
    /// The key this operation writes, or the start of its range.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        match self {
            Op::Put { key, .. } | Op::Delete { key } => key,
            Op::DeleteRange { start, .. } => start,
        }
    }

    /// The key-space bounds of a range deletion, `None` if it is empty.
    pub(crate) fn range_bounds(&self) -> Option<(Bound<&[u8]>, Bound<&[u8]>)> {
        let Op::DeleteRange { start, end } = self else {
            return None;
        };
        match end {
            Some(end) if start >= end => None,
            Some(end) => Some((Bound::Included(start), Bound::Excluded(end))),
            None => Some((Bound::Included(start), Bound::Unbounded)),
        }
    }

    fn encoded_len_estimate(&self) -> usize {
        // Tag plus up to five bytes of length per string; exact enough for
        // limits and statistics.
        match self {
            Op::Put { key, value } => 11 + key.len() + value.len(),
            Op::Delete { key } => 6 + key.len(),
            Op::DeleteRange { start, end } => 11 + start.len() + end.as_ref().map_or(0, Vec::len),
        }
    }
}

/// Writes applied atomically by [`Kv::write`](crate::Kv::write): after a
/// crash either all of them are visible or none.
///
/// ```
/// use belay_store::{Kv, MemKv, WriteBatch};
///
/// let kv = MemKv::new();
/// let mut batch = WriteBatch::new();
/// batch.put("job/1", "ready").put("queue/emails", "{}").delete("job/0");
/// let seq = kv.write(batch).unwrap();
/// assert_eq!(seq, 1);
/// assert_eq!(kv.get(b"job/1").unwrap().as_deref(), Some(&b"ready"[..]));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteBatch {
    ops: Vec<Op>,
    approx_len: usize,
}
