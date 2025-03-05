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

impl WriteBatch {
    /// An empty batch.
    #[must_use]
    pub fn new() -> WriteBatch {
        WriteBatch::default()
    }

    /// An empty batch with room for `n` operations.
    #[must_use]
    pub fn with_capacity(n: usize) -> WriteBatch {
        WriteBatch {
            ops: Vec::with_capacity(n),
            approx_len: 0,
        }
    }

    /// Adds an operation.
    pub fn push(&mut self, op: Op) -> &mut Self {
        self.approx_len += op.encoded_len_estimate();
        self.ops.push(op);
        self
    }

    /// Sets `key` to `value`.
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.push(Op::Put {
            key: key.into(),
            value: value.into(),
        })
    }

    /// Removes `key`.
    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> &mut Self {
        self.push(Op::Delete { key: key.into() })
    }

    /// Removes every key in `[start, end)`.
    pub fn delete_range(
        &mut self,
        start: impl Into<Vec<u8>>,
        end: impl Into<Vec<u8>>,
    ) -> &mut Self {
        self.push(Op::DeleteRange {
            start: start.into(),
            end: Some(end.into()),
        })
    }

    /// Removes every key from `start` to the end of the key space.
    pub fn delete_from(&mut self, start: impl Into<Vec<u8>>) -> &mut Self {
        self.push(Op::DeleteRange {
            start: start.into(),
            end: None,
        })
    }

    /// Removes every key starting with `prefix`.
    pub fn delete_prefix(&mut self, prefix: impl Into<Vec<u8>>) -> &mut Self {
        let start = prefix.into();
        let end = prefix_end(&start);
        self.push(Op::DeleteRange { start, end })
    }

    /// Appends all operations of `other`.
    pub fn append(&mut self, other: WriteBatch) -> &mut Self {
        self.approx_len += other.approx_len;
        self.ops.extend(other.ops);
        self
    }

    /// Number of operations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the batch has no operations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Approximate encoded size in bytes.
    #[must_use]
    pub fn approx_size(&self) -> usize {
        self.approx_len
    }

    /// The operations, in order.
    #[must_use]
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Iterates over the operations.
    pub fn iter(&self) -> std::slice::Iter<'_, Op> {
        self.ops.iter()
    }

    /// Removes all operations, keeping the allocation.
    pub fn clear(&mut self) {
        self.ops.clear();
        self.approx_len = 0;
    }

    /// Checks the size limits.
    pub fn validate(&self) -> Result<()> {
        let check = |what: &'static str, len: usize, max: usize| {
            if len > max {
                Err(StoreError::TooLarge { what, len, max })
            } else {
                Ok(())
            }
        };
        for op in &self.ops {
            match op {
                Op::Put { key, value } => {
                    check("key", key.len(), MAX_KEY_LEN)?;
                    check("value", value.len(), MAX_VALUE_LEN)?;
                }
                Op::Delete { key } => check("key", key.len(), MAX_KEY_LEN)?,
                Op::DeleteRange { start, end } => {
                    check("key", start.len(), MAX_KEY_LEN)?;
                    check("key", end.as_ref().map_or(0, Vec::len), MAX_KEY_LEN)?;
                }
            }
        }
        check("batch", self.approx_len, MAX_BATCH_LEN)
    }

    /// Appends the encoded batch to `out`.
    pub(crate) fn encode_into(&self, enc: &mut Encoder) {
        enc.put_count(self.ops.len());
        for op in &self.ops {
            match op {
                Op::Put { key, value } => {
                    enc.put_u8(TAG_PUT).put_bytes(key).put_bytes(value);
                }
                Op::Delete { key } => {
                    enc.put_u8(TAG_DELETE).put_bytes(key);
                }
                Op::DeleteRange {
                    start,
                    end: Some(end),
                } => {
                    enc.put_u8(TAG_DELETE_RANGE).put_bytes(start).put_bytes(end);
                }
                Op::DeleteRange { start, end: None } => {
                    enc.put_u8(TAG_DELETE_FROM).put_bytes(start);
                }
            }
        }
    }

    /// The encoded batch.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut enc = Encoder::with_capacity(self.approx_len + 5);
        self.encode_into(&mut enc);
        enc.into_vec()
    }

    /// Decodes a batch written by [`encode`](Self::encode), requiring the
    /// input to be consumed exactly.
    pub fn decode(bytes: &[u8]) -> Result<WriteBatch, DecodeError> {
        let mut dec = Decoder::new(bytes);
        let count = dec.get_count()?;
        let mut batch = WriteBatch::with_capacity(count);
        for _ in 0..count {
            let op = match dec.get_u8()? {
                TAG_PUT => Op::Put {
                    key: dec.get_bytes()?.to_vec(),
                    value: dec.get_bytes()?.to_vec(),
                },
                TAG_DELETE => Op::Delete {
                    key: dec.get_bytes()?.to_vec(),
                },
                TAG_DELETE_RANGE => Op::DeleteRange {
                    start: dec.get_bytes()?.to_vec(),
                    end: Some(dec.get_bytes()?.to_vec()),
                },
                TAG_DELETE_FROM => Op::DeleteRange {
                    start: dec.get_bytes()?.to_vec(),
                    end: None,
                },
                other => {
                    return Err(DecodeError::new(
                        dec.position() - 1,
                        belay_common::codec::DecodeErrorKind::Invalid(format!(
                            "unknown batch operation {other}"
                        )),
                    ))
                }
            };
            batch.push(op);
        }
        dec.expect_end()?;
        Ok(batch)
    }

    /// Consumes the batch, returning its operations.
    #[must_use]
    pub fn into_ops(self) -> Vec<Op> {
        self.ops
    }
}

impl From<Vec<Op>> for WriteBatch {
    fn from(ops: Vec<Op>) -> WriteBatch {
        ops.into_iter().collect()
    }
}

impl FromIterator<Op> for WriteBatch {
    fn from_iter<I: IntoIterator<Item = Op>>(iter: I) -> WriteBatch {
        let mut batch = WriteBatch::new();
        for op in iter {
            batch.push(op);
        }
        batch
    }
}

impl IntoIterator for WriteBatch {
    type Item = Op;
    type IntoIter = std::vec::IntoIter<Op>;

    fn into_iter(self) -> Self::IntoIter {
        self.ops.into_iter()
    }
}

impl<'a> IntoIterator for &'a WriteBatch {
    type Item = &'a Op;
    type IntoIter = std::slice::Iter<'a, Op>;

    fn into_iter(self) -> Self::IntoIter {
        self.ops.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Rng;

    #[test]
    fn builder_methods_record_ops_in_order() {
        let mut b = WriteBatch::new();
        b.put("a", "1")
            .delete(&b"b"[..])
            .delete_range("c", "d")
            .delete_from(vec![9u8])
            .delete_prefix("p/");
        assert_eq!(b.len(), 5);
        assert_eq!(
            b.ops()[4],
            Op::DeleteRange {
                start: b"p/".to_vec(),
                end: Some(b"p0".to_vec())
            }
        );
        let keys: Vec<&[u8]> = b.iter().map(Op::key).collect();
        assert_eq!(keys, vec![&b"a"[..], b"b", b"c", &[9], b"p/"]);
        assert!(b.approx_size() > 0);
        b.clear();
        assert!(b.is_empty());
        assert_eq!(b.approx_size(), 0);
    }

    #[test]
    fn encoding_matches_the_documented_layout() {
        let mut b = WriteBatch::new();
        b.put("k", "vv")
            .delete("x")
            .delete_range("a", "b")
            .delete_from("z");
        assert_eq!(
            b.encode(),
            vec![
                4, // count
                1, 1, b'k', 2, b'v', b'v', // put
                2, 1, b'x', // delete
                3, 1, b'a', 1, b'b', // delete range
                4, 1, b'z', // delete from
            ]
        );
    }

    #[test]
    fn random_batches_round_trip() {
        let mut rng = Rng::seed_from_u64(7);
        for _ in 0..200 {
            let mut b = WriteBatch::new();
            for _ in 0..rng.gen_range(0..8usize) {
                let mut key = vec![0u8; rng.gen_range(0..20usize)];
                rng.fill_bytes(&mut key);
                match rng.gen_range(0..4u8) {
                    0 | 1 => {
                        let mut value = vec![0u8; rng.gen_range(0..300usize)];
                        rng.fill_bytes(&mut value);
                        b.put(key, value);
                    }
                    2 => {
                        b.delete(key);
                    }
                    _ => {
                        b.delete_prefix(key);
                    }
                }
            }
            let encoded = b.encode();
            assert!(encoded.len() <= b.approx_size() + 5);
            assert_eq!(WriteBatch::decode(&encoded).expect("decodes"), b);
        }
    }

    #[test]
    fn decode_rejects_damage() {
        let mut b = WriteBatch::new();
        b.put("key", "value");
        let good = b.encode();
        for cut in 0..good.len() {
            assert!(WriteBatch::decode(&good[..cut]).is_err(), "cut at {cut}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(WriteBatch::decode(&trailing).is_err());
        let mut bad_tag = good;
        bad_tag[1] = 9;
        let e = WriteBatch::decode(&bad_tag).unwrap_err();
        assert_eq!(e.offset(), 1);
        assert!(e.to_string().contains("unknown batch operation 9"), "{e}");
    }

    #[test]
    fn validate_enforces_limits() {
        let mut b = WriteBatch::new();
        b.put(vec![0u8; MAX_KEY_LEN], "ok");
        b.validate().expect("key at the limit is fine");
        b.put(vec![0u8; MAX_KEY_LEN + 1], "");
        assert!(matches!(
            b.validate(),
            Err(StoreError::TooLarge { what: "key", .. })
        ));
        let mut b = WriteBatch::new();
        b.put("k", vec![0u8; MAX_VALUE_LEN + 1]);
        assert!(matches!(
            b.validate(),
            Err(StoreError::TooLarge { what: "value", .. })
        ));
        let mut b = WriteBatch::new();
        for _ in 0..5 {
            b.put("k", vec![0u8; MAX_VALUE_LEN]);
        }
        assert!(matches!(
            b.validate(),
            Err(StoreError::TooLarge { what: "batch", .. })
        ));
    }

    #[test]
    fn range_bounds_ignore_empty_ranges() {
        let op = Op::DeleteRange {
            start: b"b".to_vec(),
            end: Some(b"a".to_vec()),
        };
        assert!(op.range_bounds().is_none());
        let op = Op::DeleteRange {
            start: b"a".to_vec(),
            end: None,
        };
        assert_eq!(
            op.range_bounds(),
            Some((Bound::Included(&b"a"[..]), Bound::Unbounded))
        );
        assert!(Op::Delete { key: vec![] }.range_bounds().is_none());
    }

    #[test]
    fn collections_convert() {
        let ops = vec![
            Op::Put {
                key: b"a".to_vec(),
                value: vec![],
            },
            Op::Delete { key: b"a".to_vec() },
        ];
        let b = WriteBatch::from(ops.clone());
        assert_eq!((&b).into_iter().count(), 2);
        let mut c = WriteBatch::new();
        c.append(b.clone()).append(b);
        assert_eq!(c.len(), 4);
        assert_eq!(c.clone().into_ops()[..2], ops[..]);
        assert_eq!(c.into_iter().count(), 4);
    }
}
