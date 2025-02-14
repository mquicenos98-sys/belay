//! Decoding keys built with [`KeyBuf`](super::KeyBuf).

use std::fmt;

use belay_common::{Timestamp, Ulid};

use super::{ESCAPE, ESCAPED_ZERO, TERMINATOR};

/// Reads the elements of a composite key back, in the order they were
/// written. Each getter must match the method that encoded the element.
///
/// ```
/// use belay_store::keys::{KeyBuf, KeyReader};
///
/// let key = KeyBuf::new().str("sched").i64(-5).bytes(b"a\0b").into_vec();
/// let mut r = KeyReader::new(&key);
/// assert_eq!(r.str().unwrap(), "sched");
/// assert_eq!(r.i64().unwrap(), -5);
/// assert_eq!(r.bytes().unwrap(), b"a\0b");
/// assert!(r.is_empty());
/// ```
#[derive(Clone, Debug)]
pub struct KeyReader<'a> {
    key: &'a [u8],
    pos: usize,
}

impl<'a> KeyReader<'a> {
    /// A reader at the start of `key`.
    #[must_use]
    pub fn new(key: &'a [u8]) -> KeyReader<'a> {
        KeyReader { key, pos: 0 }
    }

    /// A reader positioned after `prefix`, or `None` if `key` does not
    /// start with it.
    #[must_use]
    pub fn after_prefix(key: &'a [u8], prefix: &[u8]) -> Option<KeyReader<'a>> {
        key.starts_with(prefix).then_some(KeyReader {
            key,
            pos: prefix.len(),
        })
    }

    /// Current offset in the key.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Whether the whole key has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos == self.key.len()
    }

    /// The unread bytes.
    #[must_use]
    pub fn rest(&self) -> &'a [u8] {
        &self.key[self.pos..]
    }

    /// Fails unless the whole key has been read.
    pub fn expect_end(&self) -> Result<(), KeyError> {
        match self.key.len() - self.pos {
            0 => Ok(()),
            n => Err(self.error(KeyErrorKind::TrailingBytes(n))),
        }
    }

    fn error(&self, kind: KeyErrorKind) -> KeyError {
        KeyError {
            offset: self.pos,
            kind,
        }
    }

    /// Reads `n` raw bytes.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], KeyError> {
        if self.key.len() - self.pos < n {
            return Err(self.error(KeyErrorKind::UnexpectedEnd {
                needed: n,
                available: self.key.len() - self.pos,
            }));
        }
        let out = &self.key[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Reads `N` bytes written with [`KeyBuf::fixed`](super::KeyBuf::fixed).
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], KeyError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.raw(N)?);
        Ok(out)
    }

    /// Reads a `u8`.
    pub fn u8(&mut self) -> Result<u8, KeyError> {
        Ok(self.fixed::<1>()?[0])
    }

    /// Reads a big-endian `u16`.
    pub fn u16(&mut self) -> Result<u16, KeyError> {
        self.fixed().map(u16::from_be_bytes)
    }

    /// Reads a big-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, KeyError> {
        self.fixed().map(u32::from_be_bytes)
    }

    /// Reads a big-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, KeyError> {
        self.fixed().map(u64::from_be_bytes)
    }

    /// Reads a value written with [`KeyBuf::u64_desc`](super::KeyBuf::u64_desc).
    pub fn u64_desc(&mut self) -> Result<u64, KeyError> {
        self.u64().map(|v| !v)
    }

    /// Reads a sign-flipped `i32`.
    pub fn i32(&mut self) -> Result<i32, KeyError> {
        self.u32().map(|v| (v ^ (1 << 31)) as i32)
    }

    /// Reads a sign-flipped `i64`.
    pub fn i64(&mut self) -> Result<i64, KeyError> {
        self.u64().map(|v| (v ^ (1 << 63)) as i64)
    }

    /// Reads a value written with [`KeyBuf::uvarint`](super::KeyBuf::uvarint).
    /// Non-minimal encodings are rejected: they would sort out of order.
    pub fn uvarint(&mut self) -> Result<u64, KeyError> {
        let start = self.pos;
        let n = self.u8()?;
        if n > 8 {
            self.pos = start;
            return Err(self.error(KeyErrorKind::BadLength(n)));
        }
        let bytes = match self.raw(usize::from(n)) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.pos = start;
                return Err(e);
            }
        };
        if bytes.first() == Some(&0) {
            self.pos = start;
            return Err(self.error(KeyErrorKind::NonMinimal));
        }
        Ok(bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }

    /// Reads a `bool`; bytes other than 0 and 1 are rejected.
    pub fn bool(&mut self) -> Result<bool, KeyError> {
        let start = self.pos;
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => {
                self.pos = start;
                Err(self.error(KeyErrorKind::InvalidBool(other)))
            }
        }
    }

    /// Reads an escaped, terminated byte string.
    pub fn bytes(&mut self) -> Result<Vec<u8>, KeyError> {
        let start = self.pos;
        let mut out = Vec::new();
        let mut i = self.pos;
        loop {
            let Some(&b) = self.key.get(i) else {
                return Err(KeyError {
                    offset: start,
                    kind: KeyErrorKind::Unterminated,
                });
            };
            if b != ESCAPE {
                out.push(b);
                i += 1;
                continue;
            }
            match self.key.get(i + 1) {
                Some(&ESCAPED_ZERO) => {
                    out.push(0);
                    i += 2;
                }
                Some(&TERMINATOR) => {
                    self.pos = i + 2;
                    return Ok(out);
                }
                Some(&other) => {
                    return Err(KeyError {
                        offset: i + 1,
                        kind: KeyErrorKind::BadEscape(other),
                    })
                }
                None => {
                    return Err(KeyError {
                        offset: start,
                        kind: KeyErrorKind::Unterminated,
                    })
                }
            }
        }
    }

    /// Reads an escaped, terminated UTF-8 string.
    pub fn str(&mut self) -> Result<String, KeyError> {
        let start = self.pos;
        let bytes = self.bytes()?;
        String::from_utf8(bytes).map_err(|_| {
            self.pos = start;
            self.error(KeyErrorKind::InvalidUtf8)
        })
    }

    /// Reads a timestamp written with [`KeyBuf::timestamp`](super::KeyBuf::timestamp).
    pub fn timestamp(&mut self) -> Result<Timestamp, KeyError> {
        let start = self.pos;
        let ms = self.i64()?;
        Timestamp::try_from_millis(ms).ok_or_else(|| {
            self.pos = start;
            self.error(KeyErrorKind::OutOfRange("timestamp"))
        })
    }

    /// Reads a ULID written with [`KeyBuf::ulid`](super::KeyBuf::ulid).
    pub fn ulid(&mut self) -> Result<Ulid, KeyError> {
        self.fixed().map(Ulid::from_bytes)
    }
}

/// A key that does not match the layout it is read with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyError {
    offset: usize,
    kind: KeyErrorKind,
}

impl KeyError {
    /// Offset in the key where the offending element starts.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &KeyErrorKind {
        &self.kind
    }
}

/// The reason a key failed to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyErrorKind {
    /// The key ended inside a fixed-width element.
    UnexpectedEnd {
        /// Bytes the element needs.
        needed: usize,
        /// Bytes that were left.
        available: usize,
    },
    /// A byte string has no terminator.
    Unterminated,
    /// A zero byte inside a byte string is followed by something other than
    /// an escape or the terminator.
    BadEscape(u8),
    /// A varint length byte above 8.
    BadLength(u8),
    /// A varint with a leading zero byte.
    NonMinimal,
    /// A boolean byte other than 0 or 1.
    InvalidBool(u8),
    /// A string element is not UTF-8.
    InvalidUtf8,
    /// A value outside its type's range.
    OutOfRange(&'static str),
    /// Bytes remained after the last expected element.
    TrailingBytes(usize),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid key at offset {}: ", self.offset)?;
        match &self.kind {
            KeyErrorKind::UnexpectedEnd { needed, available } => write!(
                f,
                "key ends early (needed {needed} bytes, {available} available)"
            ),
            KeyErrorKind::Unterminated => f.write_str("unterminated byte string"),
            KeyErrorKind::BadEscape(b) => write!(f, "invalid escape byte {b:#04x}"),
            KeyErrorKind::BadLength(n) => write!(f, "varint length {n} exceeds 8"),
            KeyErrorKind::NonMinimal => f.write_str("varint is not minimally encoded"),
            KeyErrorKind::InvalidBool(b) => write!(f, "invalid boolean byte {b:#04x}"),
            KeyErrorKind::InvalidUtf8 => f.write_str("string is not valid UTF-8"),
            KeyErrorKind::OutOfRange(what) => write!(f, "{what} out of range"),
            KeyErrorKind::TrailingBytes(n) => write!(f, "{n} unexpected trailing bytes"),
        }
    }
}

impl std::error::Error for KeyError {}

#[cfg(test)]
mod tests {
    use super::super::KeyBuf;
    use super::*;

    fn err_kind<T: fmt::Debug>(r: Result<T, KeyError>) -> (usize, KeyErrorKind) {
        let e = r.expect_err("decoding should fail");
        (e.offset(), e.kind().clone())
    }

    #[test]
    fn round_trips_every_element_kind() {
        let ts = Timestamp::from_millis(1_700_000_000_123);
        let id = Ulid::from_parts(1_700_000_000_000, 0xABCD);
        let key = KeyBuf::new()
            .u8(7)
            .u16(513)
            .u32(70_000)
            .u64(u64::MAX)
            .u64_desc(5)
            .i32(-3)
            .i64(i64::MIN)
            .uvarint(300)
            .bool(true)
            .bytes(&[0, 0xFF, 0])
            .str("héllo")
            .timestamp(ts)
            .ulid(id)
            .fixed(&[9, 9])
            .into_vec();
        let mut r = KeyReader::new(&key);
        assert_eq!(r.u8(), Ok(7));
        assert_eq!(r.u16(), Ok(513));
        assert_eq!(r.u32(), Ok(70_000));
        assert_eq!(r.u64(), Ok(u64::MAX));
        assert_eq!(r.u64_desc(), Ok(5));
        assert_eq!(r.i32(), Ok(-3));
        assert_eq!(r.i64(), Ok(i64::MIN));
        assert_eq!(r.uvarint(), Ok(300));
        assert_eq!(r.bool(), Ok(true));
        assert_eq!(r.bytes(), Ok(vec![0, 0xFF, 0]));
        assert_eq!(r.str().as_deref(), Ok("héllo"));
        assert_eq!(r.timestamp(), Ok(ts));
        assert_eq!(r.ulid(), Ok(id));
        assert_eq!(r.fixed::<2>(), Ok([9, 9]));
        r.expect_end().expect("consumed");
    }

    #[test]
    fn after_prefix_skips_a_namespace() {
        let key = KeyBuf::new().str("job").u64(4).into_vec();
        let prefix = KeyBuf::new().str("job").into_vec();
        let mut r = KeyReader::after_prefix(&key, &prefix).expect("has prefix");
        assert_eq!(r.u64(), Ok(4));
        assert!(KeyReader::after_prefix(&key, b"x").is_none());
    }

    #[test]
    fn rejects_malformed_byte_strings() {
        assert_eq!(
            err_kind(KeyReader::new(b"abc").bytes()),
            (0, KeyErrorKind::Unterminated)
        );
        assert_eq!(
            err_kind(KeyReader::new(&[b'a', 0]).bytes()),
            (0, KeyErrorKind::Unterminated)
        );
        assert_eq!(
            err_kind(KeyReader::new(&[b'a', 0, 7]).bytes()),
            (2, KeyErrorKind::BadEscape(7))
        );
        let mut r = KeyReader::new(&[0xC3, 0x28, 0, 1]);
        assert_eq!(err_kind(r.str()), (0, KeyErrorKind::InvalidUtf8));
        assert_eq!(r.position(), 0, "failed reads do not advance");
    }

    #[test]
    fn rejects_bad_fixed_width_and_varint_elements() {
        let mut r = KeyReader::new(&[1, 2, 3]);
        assert_eq!(
            err_kind(r.u32()),
            (
                0,
                KeyErrorKind::UnexpectedEnd {
                    needed: 4,
                    available: 3
                }
            )
        );
        assert_eq!(r.rest(), &[1, 2, 3]);
        assert_eq!(
            err_kind(KeyReader::new(&[9]).uvarint()),
            (0, KeyErrorKind::BadLength(9))
        );
        assert_eq!(
            err_kind(KeyReader::new(&[2, 0, 5]).uvarint()),
            (0, KeyErrorKind::NonMinimal)
        );
        let mut r = KeyReader::new(&[3, 1]);
        assert!(r.uvarint().is_err());
        assert_eq!(r.position(), 0);
        assert_eq!(
            err_kind(KeyReader::new(&[2]).bool()),
            (0, KeyErrorKind::InvalidBool(2))
        );
        let far_future = KeyBuf::new().i64(i64::MAX).into_vec();
        assert_eq!(
            err_kind(KeyReader::new(&far_future).timestamp()),
            (0, KeyErrorKind::OutOfRange("timestamp"))
        );
    }

    #[test]
    fn errors_render_with_offsets() {
        let mut r = KeyReader::new(&[0, 1, 0xAA]);
        assert_eq!(r.bytes(), Ok(Vec::new()));
        let e = r.expect_end().unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid key at offset 2: 1 unexpected trailing bytes"
        );
        let e = KeyReader::new(&[b'x', 0, 3]).bytes().unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid key at offset 2: invalid escape byte 0x03"
        );
    }
}
