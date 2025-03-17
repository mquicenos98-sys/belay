//! Order-preserving encoding of composite keys.
//!
//! The store orders keys bytewise. [`KeyBuf`] turns a tuple such as
//! `("job", 42u64, -5i64)` into bytes whose bytewise order equals the
//! tuple's element-wise order, so range scans over encoded keys visit
//! tuples in their natural order and a tuple prefix is a key prefix.
//!
//! | element | encoding | order |
//! |---|---|---|
//! | [`u8`](KeyBuf::u8), [`u16`](KeyBuf::u16), [`u32`](KeyBuf::u32), [`u64`](KeyBuf::u64) | big-endian, fixed width | numeric |
//! | [`i32`](KeyBuf::i32), [`i64`](KeyBuf::i64) | big-endian with the sign bit flipped | numeric |
//! | [`u64_desc`](KeyBuf::u64_desc) | big-endian of `!v` | reversed |
//! | [`uvarint`](KeyBuf::uvarint) | one byte `n` (0-8), then the `n` significant bytes big-endian | numeric |
//! | [`bool`](KeyBuf::bool) | `0x00` / `0x01` | `false < true` |
//! | [`bytes`](KeyBuf::bytes), [`str`](KeyBuf::str) | `0x00` escaped as `0x00 0xFF`, terminated by `0x00 0x01` | bytewise |
//! | [`timestamp`](KeyBuf::timestamp) | as `i64` milliseconds | chronological |
//! | [`ulid`](KeyBuf::ulid) | 16 bytes big-endian | creation time, then random |
//! | [`fixed`](KeyBuf::fixed) | raw bytes; every key must use the same width here | bytewise |
//!
//! The two-byte terminator matters. With a lone `0x00` terminator, `"a"`
//! followed by an element starting with `0xFF` would sort after `"a\0"`;
//! `0x00 0x01` is smaller than any escaped continuation (`0x00 0xFF`) and any
//! non-zero byte, so a shorter string sorts first no matter what follows.
//! It also makes the encoding prefix-free: no encoded string is a prefix of
//! another, which is what lets tuples concatenate.
//!
//! Tuples compare element-wise when they share a layout (the same element
//! kinds in the same positions), and a tuple sorts before its extensions.
//! Keys are not self-describing; decode them with a [`KeyReader`] that
//! reads the same layout.
//!
//! ```
//! use belay_store::keys::{KeyBuf, KeyReader};
//!
//! let a = KeyBuf::new().str("job").u64(9).into_vec();
//! let b = KeyBuf::new().str("job").u64(10).into_vec();
//! let c = KeyBuf::new().str("jobs").u64(1).into_vec();
//! assert!(a < b && b < c);
//!
//! let mut r = KeyReader::new(&b);
//! assert_eq!(r.str().unwrap(), "job");
//! assert_eq!(r.u64().unwrap(), 10);
//! r.expect_end().unwrap();
//! ```

mod reader;

use std::fmt;

use belay_common::{Timestamp, Ulid};

use crate::range::KeyRange;

pub use reader::{KeyError, KeyErrorKind, KeyReader};

/// Escape byte introducing either an escaped zero or the terminator.
const ESCAPE: u8 = 0x00;

/// Follows [`ESCAPE`] for a literal zero byte.
const ESCAPED_ZERO: u8 = 0xFF;

/// Follows [`ESCAPE`] at the end of a byte string.
const TERMINATOR: u8 = 0x01;

/// Builds an order-preserving composite key. Methods consume and return
/// the builder so keys read as one expression.
///
/// See the [module docs](self) for the encoding of each element.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyBuf {
    buf: Vec<u8>,
}

impl KeyBuf {
    /// An empty key.
    #[must_use]
    pub fn new() -> KeyBuf {
        KeyBuf::default()
    }

    /// A key starting with `prefix` verbatim, e.g. a namespace prefix.
    #[must_use]
    pub fn with_prefix(prefix: &[u8]) -> KeyBuf {
        KeyBuf {
            buf: prefix.to_vec(),
        }
    }

    /// Appends one byte.
    #[must_use]
    pub fn u8(mut self, v: u8) -> KeyBuf {
        self.buf.push(v);
        self
    }

    /// Appends a big-endian `u16`.
    #[must_use]
    pub fn u16(self, v: u16) -> KeyBuf {
        self.raw(&v.to_be_bytes())
    }

    /// Appends a big-endian `u32`.
    #[must_use]
    pub fn u32(self, v: u32) -> KeyBuf {
        self.raw(&v.to_be_bytes())
    }

    /// Appends a big-endian `u64`.
    #[must_use]
    pub fn u64(self, v: u64) -> KeyBuf {
        self.raw(&v.to_be_bytes())
    }

    /// Appends a `u64` that sorts in descending order, e.g. to scan the
    /// newest sequence numbers first without a reverse scan.
    #[must_use]
    pub fn u64_desc(self, v: u64) -> KeyBuf {
        self.u64(!v)
    }

    /// Appends an `i32` so that negative values sort before positive ones.
    #[must_use]
    pub fn i32(self, v: i32) -> KeyBuf {
        self.u32((v as u32) ^ (1 << 31))
    }

    /// Appends an `i64` so that negative values sort before positive ones.
    #[must_use]
    pub fn i64(self, v: i64) -> KeyBuf {
        self.u64((v as u64) ^ (1 << 63))
    }

    /// Appends a `u64` in 1 to 9 bytes, still in numeric order: a length
    /// byte, then the value's significant bytes big-endian. Compact for
    /// counters that are usually small.
    #[must_use]
    pub fn uvarint(mut self, v: u64) -> KeyBuf {
        let bytes = v.to_be_bytes();
        let skip = (v.leading_zeros() / 8) as usize;
        self.buf.push((8 - skip) as u8);
        self.buf.extend_from_slice(&bytes[skip..]);
        self
    }

    /// Appends `0` or `1`.
    #[must_use]
    pub fn bool(self, v: bool) -> KeyBuf {
        self.u8(u8::from(v))
    }

    /// Appends an escaped, terminated byte string.
    #[must_use]
    pub fn bytes(mut self, v: &[u8]) -> KeyBuf {
        escape_into(&mut self.buf, v);
        self.buf.extend_from_slice(&[ESCAPE, TERMINATOR]);
        self
    }

    /// Appends an escaped, terminated string.
    #[must_use]
    pub fn str(self, v: &str) -> KeyBuf {
        self.bytes(v.as_bytes())
    }

    /// Appends `v` *without* the terminator. Only useful as the
    /// last element of a scan prefix: `KeyBuf::new().str("tag").str_prefix("pri")`
    /// covers every key whose second element starts with `"pri"`.
    #[must_use]
    pub fn str_prefix(self, v: &str) -> KeyBuf {
        self.bytes_prefix(v.as_bytes())
    }

    /// Byte-string version of [`str_prefix`](Self::str_prefix).
    #[must_use]
    pub fn bytes_prefix(mut self, v: &[u8]) -> KeyBuf {
        self.buf.extend_from_slice(v);
        self
    }

    /// Appends a timestamp as signed milliseconds.
    #[must_use]
    pub fn timestamp(self, ts: Timestamp) -> KeyBuf {
        self.i64(ts.as_millis())
    }

    /// Appends a ULID's 16 big-endian bytes, which sort by creation time.
    #[must_use]
    pub fn ulid(self, id: Ulid) -> KeyBuf {
        self.raw(&id.to_bytes())
    }

    /// Appends bytes of a width that every key has at this position (a
    /// hash, a fixed-size id). Variable-width data belongs in
    /// [`bytes`](Self::bytes).
    #[must_use]
    pub fn fixed(self, v: &[u8]) -> KeyBuf {
        self.raw(v)
    }

    /// Appends bytes verbatim, with no ordering guarantees of its own.
    #[must_use]
    pub fn raw(mut self, v: &[u8]) -> KeyBuf {
        self.buf.extend_from_slice(v);
        self
    }

    /// The encoded key.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the builder, returning the encoded key.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// Encoded length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// See [`prefix_end`].
    #[must_use]
    pub fn prefix_end(&self) -> Option<Vec<u8>> {
        prefix_end(&self.buf)
    }

    /// The range of keys starting with this one.
    #[must_use]
    pub fn range(&self) -> KeyRange {
        KeyRange::prefix(&self.buf)
    }
}
