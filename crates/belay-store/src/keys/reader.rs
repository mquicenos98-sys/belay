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
