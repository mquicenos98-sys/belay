//! Compact binary encoding for internal formats: WAL records, snapshots and
//! gossip messages.
//!
//! - Fixed-width integers and floats are little-endian.
//! - Varints are unsigned LEB128 (seven bits per byte, low group first);
//!   signed varints are zigzag-mapped first so that small negative numbers
//!   stay short (`-1` takes one byte).
//! - Byte strings and strings carry a varint length prefix.
//!
//! The decoder rejects over-long varints so every value has exactly one
//! encoding, which keeps checksums of re-encoded data stable. Getters leave
//! the position unchanged when they fail, and errors report the offset at
//! which the failed value starts.
//!
//! ```
//! use belay_common::codec::{Decoder, Encoder};
//!
//! let mut enc = Encoder::new();
//! enc.put_u8(1).put_uvarint(300).put_str("emails").put_ivarint(-2);
//! let bytes = enc.into_vec();
//!
//! let mut dec = Decoder::new(&bytes);
//! assert_eq!(dec.get_u8().unwrap(), 1);
//! assert_eq!(dec.get_uvarint().unwrap(), 300);
//! assert_eq!(dec.get_str().unwrap(), "emails");
//! assert_eq!(dec.get_ivarint().unwrap(), -2);
//! dec.expect_end().unwrap();
//! ```

use std::fmt;

/// Maximum encoded size of a 64-bit varint.
pub const MAX_VARINT_LEN: usize = 10;

/// Number of bytes [`Encoder::put_uvarint`] uses for `value`.
#[must_use]
pub const fn uvarint_len(value: u64) -> usize {
    let bits = 64 - (value | 1).leading_zeros() as usize;
    (bits + 6) / 7
}

const fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

const fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

/// Appends encoded values to a byte buffer. Methods chain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// An empty encoder.
    #[must_use]
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// An empty encoder with room for `capacity` bytes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Encoder {
        Encoder {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// An encoder appending to `buf`, e.g. after a header written elsewhere.
    #[must_use]
    pub fn from_vec(buf: Vec<u8>) -> Encoder {
        Encoder { buf }
    }

    /// Appends one byte.
    pub fn put_u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    /// Appends a little-endian `u16`.
    pub fn put_u16(&mut self, v: u16) -> &mut Self {
        self.put_raw(&v.to_le_bytes())
    }

    /// Appends a little-endian `u32`.
    pub fn put_u32(&mut self, v: u32) -> &mut Self {
        self.put_raw(&v.to_le_bytes())
    }

    /// Appends a little-endian `u64`.
    pub fn put_u64(&mut self, v: u64) -> &mut Self {
        self.put_raw(&v.to_le_bytes())
    }

    /// Appends a little-endian two's-complement `i64`.
    pub fn put_i64(&mut self, v: i64) -> &mut Self {
        self.put_raw(&v.to_le_bytes())
    }

    /// Appends the IEEE 754 bits of `v`, little-endian (NaN payloads kept).
    pub fn put_f64(&mut self, v: f64) -> &mut Self {
        self.put_u64(v.to_bits())
    }

    /// Appends `0` or `1`.
    pub fn put_bool(&mut self, v: bool) -> &mut Self {
        self.put_u8(u8::from(v))
    }

    /// Appends an unsigned LEB128 varint (1 to 10 bytes).
    pub fn put_uvarint(&mut self, mut v: u64) -> &mut Self {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
        self
    }

    /// Appends a zigzag-encoded signed varint.
    pub fn put_ivarint(&mut self, v: i64) -> &mut Self {
        self.put_uvarint(zigzag(v))
    }

    /// Appends an element count as a varint; pairs with
    /// [`Decoder::get_count`].
    pub fn put_count(&mut self, n: usize) -> &mut Self {
        self.put_uvarint(n as u64)
    }

    /// Appends a varint length followed by `bytes`.
    pub fn put_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.put_count(bytes.len());
        self.put_raw(bytes)
    }

    /// Appends a varint length followed by the UTF-8 bytes of `s`.
    pub fn put_str(&mut self, s: &str) -> &mut Self {
        self.put_bytes(s.as_bytes())
    }

    /// Appends `bytes` as they are, without a length.
    pub fn put_raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The encoded bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the encoder, returning its buffer.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// Empties the buffer, keeping its allocation.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}
