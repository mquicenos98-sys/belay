//! RFC 3339 (`date-time` production of section 5.6) text form of [`Timestamp`].
//!
//! Output is always UTC with exactly three fractional digits so that the
//! strings have a fixed width and sort like the instants they denote. Input
//! is more permissive, as the RFC allows: any offset, a space instead of `T`,
//! lowercase `t`/`z`, and up to nine fractional digits.

use std::fmt;

use super::{check_field, days_from_civil, days_in_month, TimeError, Timestamp};
use super::{MILLIS_PER_DAY, MILLIS_PER_SEC};

pub(super) fn write(ts: Timestamp, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let c = ts.to_civil();
    // The clamped range guarantees 0 <= year <= 9999, so `{:04}` is exact.
    write!(
        f,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.millis
    )
}

/// Byte cursor that reports failures with the offset where they happened.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Cursor {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn syntax(&self, expected: &'static str) -> TimeError {
        TimeError::Syntax {
            pos: self.pos,
            expected,
        }
    }

    /// Reads exactly `n` ASCII digits.
    fn digits(&mut self, n: usize, expected: &'static str) -> Result<u32, TimeError> {
        let mut value = 0u32;
        for _ in 0..n {
            match self.peek() {
                Some(b @ b'0'..=b'9') => {
                    value = value * 10 + u32::from(b - b'0');
                    self.pos += 1;
                }
                _ => return Err(self.syntax(expected)),
            }
        }
        Ok(value)
    }

    fn expect(&mut self, byte: u8, expected: &'static str) -> Result<(), TimeError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.syntax(expected))
        }
    }

    fn finish(&self) -> Result<(), TimeError> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(self.syntax("end of input"))
        }
    }
}
