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

/// Reads values back from a byte slice. Borrowed byte strings and strings
/// point into the input, so decoding does not copy.
#[derive(Clone, Debug)]
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    /// A decoder at the start of `buf`.
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Decoder<'a> {
        Decoder { buf, pos: 0 }
    }

    /// Current offset from the start of the input.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes left to read.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether everything has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The unread bytes, without consuming them.
    #[must_use]
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// Fails with [`DecodeErrorKind::TrailingBytes`] unless everything has
    /// been read.
    pub fn expect_end(&self) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(self.error_here(DecodeErrorKind::TrailingBytes(n))),
        }
    }

    /// An [`Invalid`](DecodeErrorKind::Invalid) error at the current offset,
    /// for format-level checks such as an unknown record type.
    #[must_use]
    pub fn invalid(&self, message: impl Into<String>) -> DecodeError {
        self.error_here(DecodeErrorKind::Invalid(message.into()))
    }

    fn error_here(&self, kind: DecodeErrorKind) -> DecodeError {
        DecodeError {
            offset: self.pos,
            kind,
        }
    }

    /// Reads exactly `n` bytes, borrowed from the input.
    pub fn get_raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.remaining() {
            return Err(self.error_here(DecodeErrorKind::UnexpectedEof {
                needed: n,
                available: self.remaining(),
            }));
        }
        let bytes = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    /// Reads exactly `N` bytes into an array.
    pub fn get_array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.get_raw(N)?);
        Ok(out)
    }

    /// Reads one byte.
    pub fn get_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.get_array::<1>()?[0])
    }

    /// Reads a little-endian `u16`.
    pub fn get_u16(&mut self) -> Result<u16, DecodeError> {
        self.get_array().map(u16::from_le_bytes)
    }

    /// Reads a little-endian `u32`.
    pub fn get_u32(&mut self) -> Result<u32, DecodeError> {
        self.get_array().map(u32::from_le_bytes)
    }

    /// Reads a little-endian `u64`.
    pub fn get_u64(&mut self) -> Result<u64, DecodeError> {
        self.get_array().map(u64::from_le_bytes)
    }

    /// Reads a little-endian `i64`.
    pub fn get_i64(&mut self) -> Result<i64, DecodeError> {
        self.get_array().map(i64::from_le_bytes)
    }

    /// Reads an `f64` from its little-endian bits.
    pub fn get_f64(&mut self) -> Result<f64, DecodeError> {
        self.get_u64().map(f64::from_bits)
    }

    /// Reads a boolean; any byte other than `0` or `1` is an error.
    pub fn get_bool(&mut self) -> Result<bool, DecodeError> {
        let start = self.pos;
        match self.get_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => {
                self.pos = start;
                Err(self.error_here(DecodeErrorKind::InvalidBool(other)))
            }
        }
    }

    /// Reads an unsigned LEB128 varint in its minimal encoding.
    pub fn get_uvarint(&mut self) -> Result<u64, DecodeError> {
        let start = self.pos;
        let mut value = 0u64;
        for i in 0..MAX_VARINT_LEN {
            let Some(&byte) = self.buf.get(start + i) else {
                return Err(self.error_here(DecodeErrorKind::UnexpectedEof {
                    needed: i + 1,
                    available: i,
                }));
            };
            value |= u64::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                if byte == 0 && i > 0 {
                    return Err(self.error_here(DecodeErrorKind::NonMinimalVarint));
                }
                self.pos = start + i + 1;
                return Ok(value);
            }
        }
        // Ten bytes, each with its continuation bit set.
        Err(self.error_here(DecodeErrorKind::VarintOverflow))
    }

    /// Reads a zigzag-encoded signed varint.
    pub fn get_ivarint(&mut self) -> Result<i64, DecodeError> {
        self.get_uvarint().map(unzigzag)
    }

    /// Reads an element count written by [`Encoder::put_count`].
    ///
    /// Counts larger than the remaining input are rejected, on the basis
    /// that every element takes at least one byte; a corrupt count therefore
    /// cannot make the caller pre-allocate a huge collection.
    pub fn get_count(&mut self) -> Result<usize, DecodeError> {
        let start = self.pos;
        let n = self.get_uvarint()?;
        let available = self.remaining();
        match usize::try_from(n) {
            Ok(n) if n <= available => Ok(n),
            _ => {
                self.pos = start;
                Err(self.error_here(DecodeErrorKind::CountTooLarge {
                    count: n,
                    available,
                }))
            }
        }
    }

    /// Reads a length-prefixed byte string, borrowed from the input.
    pub fn get_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let start = self.pos;
        let len = self.get_uvarint()?;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        self.get_raw(len).map_err(|e| {
            self.pos = start;
            DecodeError { offset: start, ..e }
        })
    }

    /// Reads a length-prefixed UTF-8 string, borrowed from the input.
    pub fn get_str(&mut self) -> Result<&'a str, DecodeError> {
        let start = self.pos;
        let bytes = self.get_bytes()?;
        std::str::from_utf8(bytes).map_err(|_| {
            self.pos = start;
            self.error_here(DecodeErrorKind::InvalidUtf8)
        })
    }
}

/// Failure to decode, with the offset of the value that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    offset: usize,
    kind: DecodeErrorKind,
}

impl DecodeError {
    /// An error of `kind` at `offset`, for decoders built on top of this one.
    #[must_use]
    pub fn new(offset: usize, kind: DecodeErrorKind) -> DecodeError {
        DecodeError { offset, kind }
    }

    /// Byte offset of the value that failed to decode.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &DecodeErrorKind {
        &self.kind
    }
}

/// The reason decoding failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeErrorKind {
    /// The input ended inside a value.
    UnexpectedEof {
        /// Bytes the value needs (for varints, at least this many).
        needed: usize,
        /// Bytes that were left.
        available: usize,
    },
    /// A varint does not fit in 64 bits.
    VarintOverflow,
    /// A varint has redundant trailing zero groups.
    NonMinimalVarint,
    /// An element count exceeds the remaining input.
    CountTooLarge {
        /// The decoded count.
        count: u64,
        /// Bytes that were left after the count.
        available: usize,
    },
    /// A string is not valid UTF-8.
    InvalidUtf8,
    /// A boolean byte other than 0 or 1.
    InvalidBool(u8),
    /// Input remained after the last expected value.
    TrailingBytes(usize),
    /// A format-level check failed.
    Invalid(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error at offset {}: ", self.offset)?;
        match &self.kind {
            DecodeErrorKind::UnexpectedEof { needed, available } => {
                write!(
                    f,
                    "unexpected end of input (needed {needed} bytes, {available} available)"
                )
            }
            DecodeErrorKind::VarintOverflow => f.write_str("varint does not fit in 64 bits"),
            DecodeErrorKind::NonMinimalVarint => f.write_str("varint is not minimally encoded"),
            DecodeErrorKind::CountTooLarge { count, available } => write!(
                f,
                "element count {count} exceeds the {available} remaining bytes"
            ),
            DecodeErrorKind::InvalidUtf8 => f.write_str("string is not valid UTF-8"),
            DecodeErrorKind::InvalidBool(b) => write!(f, "invalid boolean byte {b:#04x}"),
            DecodeErrorKind::TrailingBytes(n) => write!(f, "{n} unexpected trailing bytes"),
            DecodeErrorKind::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::Rng;

    fn kind_of<T: fmt::Debug>(r: Result<T, DecodeError>) -> (usize, DecodeErrorKind) {
        let e = r.expect_err("decoding should fail");
        (e.offset(), e.kind().clone())
    }

    #[test]
    fn fixed_width_values_are_little_endian() {
        let mut enc = Encoder::new();
        enc.put_u16(0x0102)
            .put_u32(0x0304_0506)
            .put_u64(1)
            .put_i64(-1);
        assert_eq!(
            enc.as_slice(),
            &[
                0x02, 0x01, 0x06, 0x05, 0x04, 0x03, 1, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
                0xFF, 0xFF, 0xFF, 0xFF
            ]
        );
        let mut dec = Decoder::new(enc.as_slice());
        assert_eq!(dec.get_u16(), Ok(0x0102));
        assert_eq!(dec.get_u32(), Ok(0x0304_0506));
        assert_eq!(dec.get_u64(), Ok(1));
        assert_eq!(dec.get_i64(), Ok(-1));
        assert!(dec.is_empty());
    }

    #[test]
    fn varint_boundaries_and_lengths() {
        let cases: [(u64, &[u8]); 6] = [
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (300, &[0xAC, 0x02]),
            (16_384, &[0x80, 0x80, 0x01]),
        ];
        for (value, bytes) in cases {
            let mut enc = Encoder::new();
            enc.put_uvarint(value);
            assert_eq!(enc.as_slice(), bytes, "{value}");
            assert_eq!(uvarint_len(value), bytes.len());
            assert_eq!(Decoder::new(bytes).get_uvarint(), Ok(value));
        }
        let mut enc = Encoder::new();
        enc.put_uvarint(u64::MAX);
        assert_eq!(enc.len(), MAX_VARINT_LEN);
        assert_eq!(uvarint_len(u64::MAX), MAX_VARINT_LEN);
        assert_eq!(Decoder::new(enc.as_slice()).get_uvarint(), Ok(u64::MAX));
    }

    #[test]
    fn zigzag_keeps_small_negatives_short() {
        for (v, z) in [
            (0i64, 0u64),
            (-1, 1),
            (1, 2),
            (-2, 3),
            (i64::MAX, u64::MAX - 1),
            (i64::MIN, u64::MAX),
        ] {
            assert_eq!(zigzag(v), z);
            assert_eq!(unzigzag(z), v);
        }
        let mut enc = Encoder::new();
        enc.put_ivarint(-1).put_ivarint(-64);
        assert_eq!(enc.as_slice(), &[0x01, 0x7F]);
    }

    #[test]
    fn rejects_non_minimal_and_overflowing_varints() {
        assert_eq!(
            kind_of(Decoder::new(&[0x80, 0x00]).get_uvarint()),
            (0, DecodeErrorKind::NonMinimalVarint)
        );
        assert_eq!(
            kind_of(Decoder::new(&[0xFF, 0x80, 0x00]).get_uvarint()),
            (0, DecodeErrorKind::NonMinimalVarint)
        );
        let too_long = [0x80; 11];
        assert_eq!(
            kind_of(Decoder::new(&too_long).get_uvarint()),
            (0, DecodeErrorKind::VarintOverflow)
        );
    }

    #[test]
    fn truncated_input_reports_offset_and_leaves_position() {
        let mut dec = Decoder::new(&[1, 2, 3]);
        dec.get_u8().expect("one byte available");
        assert_eq!(
            kind_of(dec.get_u32()),
            (
                1,
                DecodeErrorKind::UnexpectedEof {
                    needed: 4,
                    available: 2
                }
            )
        );
        assert_eq!(dec.position(), 1);
        assert_eq!(dec.rest(), &[2, 3]);
        assert_eq!(
            kind_of(Decoder::new(&[0x80]).get_uvarint()),
            (
                0,
                DecodeErrorKind::UnexpectedEof {
                    needed: 2,
                    available: 1
                }
            )
        );
    }

    #[test]
    fn bytes_and_strings_are_length_prefixed_and_borrowed() {
        let mut enc = Encoder::new();
        enc.put_bytes(&[9, 8, 7]).put_str("héllo").put_bytes(&[]);
        let bytes = enc.into_vec();
        assert_eq!(&bytes[..4], &[3, 9, 8, 7]);
        let mut dec = Decoder::new(&bytes);
        let b = dec.get_bytes().expect("bytes");
        assert_eq!(b, &[9, 8, 7]);
        assert!(std::ptr::eq(b.as_ptr(), bytes[1..].as_ptr()), "no copy");
        assert_eq!(dec.get_str(), Ok("héllo"));
        assert_eq!(dec.get_bytes(), Ok(&[][..]));
        dec.expect_end().expect("fully consumed");
    }

    #[test]
    fn length_beyond_input_fails_at_the_prefix() {
        let mut dec = Decoder::new(&[0x05, b'a', b'b']);
        assert_eq!(
            kind_of(dec.get_bytes()),
            (
                0,
                DecodeErrorKind::UnexpectedEof {
                    needed: 5,
                    available: 2
                }
            )
        );
        assert_eq!(dec.position(), 0);
    }

    #[test]
    fn invalid_utf8_and_bools_are_rejected() {
        let mut dec = Decoder::new(&[0x02, 0xC3, 0x28]);
        assert_eq!(kind_of(dec.get_str()), (0, DecodeErrorKind::InvalidUtf8));
        assert_eq!(dec.position(), 0);
        let mut dec = Decoder::new(&[1, 0, 2]);
        assert_eq!(dec.get_bool(), Ok(true));
        assert_eq!(dec.get_bool(), Ok(false));
        assert_eq!(
            kind_of(dec.get_bool()),
            (2, DecodeErrorKind::InvalidBool(2))
        );
        assert_eq!(dec.position(), 2);
    }

    #[test]
    fn counts_larger_than_remaining_input_are_rejected() {
        let mut enc = Encoder::new();
        enc.put_count(3).put_raw(&[1, 2, 3]);
        let mut dec = Decoder::new(enc.as_slice());
        assert_eq!(dec.get_count(), Ok(3));

        let mut enc = Encoder::new();
        enc.put_uvarint(u64::MAX).put_u8(0);
        let mut dec = Decoder::new(enc.as_slice());
        assert_eq!(
            kind_of(dec.get_count()),
            (
                0,
                DecodeErrorKind::CountTooLarge {
                    count: u64::MAX,
                    available: 1
                }
            )
        );
    }

    #[test]
    fn expect_end_and_custom_errors() {
        let mut dec = Decoder::new(&[7, 1, 2]);
        dec.get_u8().expect("byte");
        let e = dec.expect_end().unwrap_err();
        assert_eq!(
            e.to_string(),
            "decode error at offset 1: 2 unexpected trailing bytes"
        );
        let e = dec.invalid(format!("unknown record type {}", 7));
        assert_eq!(
            e.to_string(),
            "decode error at offset 1: unknown record type 7"
        );
        let e = DecodeError::new(40, DecodeErrorKind::InvalidBool(9));
        assert_eq!(
            e.to_string(),
            "decode error at offset 40: invalid boolean byte 0x09"
        );
    }

    #[test]
    fn floats_round_trip_bit_exactly() {
        let nan = f64::from_bits(0x7FF8_0000_0000_1234);
        let mut enc = Encoder::with_capacity(32);
        enc.put_f64(-0.0).put_f64(nan).put_f64(f64::INFINITY);
        let mut dec = Decoder::new(enc.as_slice());
        assert_eq!(dec.get_f64().map(f64::to_bits), Ok((-0.0f64).to_bits()));
        assert_eq!(dec.get_f64().map(f64::to_bits), Ok(nan.to_bits()));
        assert_eq!(dec.get_f64(), Ok(f64::INFINITY));
    }

    #[test]
    fn random_mixed_sequences_round_trip() {
        let mut rng = Rng::seed_from_u64(30);
        for _ in 0..200 {
            let values: Vec<(u8, u64)> = (0..rng.gen_range(0..20usize))
                .map(|_| {
                    (
                        rng.gen_range(0..4u8),
                        rng.next_u64() >> rng.gen_range(0..64u32),
                    )
                })
                .collect();
            let mut enc = Encoder::from_vec(vec![0xAA]);
            for &(tag, v) in &values {
                match tag {
                    0 => enc.put_uvarint(v),
                    1 => enc.put_ivarint(v as i64),
                    2 => enc.put_u64(v),
                    _ => enc.put_str(&v.to_string()),
                };
            }
            let bytes = enc.into_vec();
            let mut dec = Decoder::new(&bytes);
            assert_eq!(dec.get_u8(), Ok(0xAA));
            for &(tag, v) in &values {
                match tag {
                    0 => assert_eq!(dec.get_uvarint(), Ok(v)),
                    1 => assert_eq!(dec.get_ivarint(), Ok(v as i64)),
                    2 => assert_eq!(dec.get_u64(), Ok(v)),
                    _ => assert_eq!(dec.get_str().map(str::to_owned), Ok(v.to_string())),
                }
            }
            dec.expect_end().expect("no trailing bytes");
        }
    }

    #[test]
    fn encoder_buffer_management() {
        let mut enc = Encoder::new();
        assert!(enc.is_empty());
        enc.put_bool(true).put_raw(b"xy");
        assert_eq!(enc.len(), 3);
        enc.clear();
        assert!(enc.is_empty());
        let mut dec = Decoder::new(b"abcd");
        assert_eq!(dec.get_array::<2>(), Ok(*b"ab"));
        assert_eq!(dec.remaining(), 2);
        assert_eq!(dec.get_raw(2), Ok(&b"cd"[..]));
    }
}
