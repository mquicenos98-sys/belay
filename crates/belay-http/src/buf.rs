//! A read buffer with a movable start, used by both ends of a connection.
//!
//! Bytes are appended by socket reads and consumed from the front by the
//! parsers. Consumed space is reclaimed lazily (when at least half the buffer
//! is dead) so a pipelined burst of small requests does not memmove on every
//! request.

use std::io::{self, Read};

#[derive(Debug, Default)]
pub(crate) struct InBuf {
    data: Vec<u8>,
    start: usize,
}

impl InBuf {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The unconsumed bytes.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.data[self.start..]
    }

    pub(crate) fn len(&self) -> usize {
        self.data.len() - self.start
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Marks the first `n` unconsumed bytes as used.
    pub(crate) fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.len(), "consumed past the end of the buffer");
        self.start = (self.start + n).min(self.data.len());
        if self.start == self.data.len() {
            self.data.clear();
            self.start = 0;
        }
    }

    /// Moves up to `max` unconsumed bytes onto the end of `out`.
    pub(crate) fn drain_into(&mut self, out: &mut Vec<u8>, max: usize) -> usize {
        let n = self.len().min(max);
        out.extend_from_slice(&self.as_slice()[..n]);
        self.consume(n);
        n
    }

    /// Performs one `read` of at most `chunk` bytes from `src`, appending to
    /// the buffer. Returns the byte count (0 means end of stream).
    pub(crate) fn read_from<R: Read>(&mut self, src: &mut R, chunk: usize) -> io::Result<usize> {
        if self.start > 0 && self.start * 2 >= self.data.len() {
            self.data.drain(..self.start);
            self.start = 0;
        }
        let old_len = self.data.len();
        self.data.resize(old_len + chunk, 0);
        let result = src.read(&mut self.data[old_len..]);
        let n = *result.as_ref().unwrap_or(&0);
        self.data.truncate(old_len + n);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consume_and_refill_preserve_order() {
        let mut buf = InBuf::new();
        let mut src: &[u8] = b"hello world";
        assert_eq!(buf.read_from(&mut src, 5).expect("slice read"), 5);
        assert_eq!(buf.as_slice(), b"hello");
        buf.consume(2);
        assert_eq!(buf.read_from(&mut src, 64).expect("slice read"), 6);
        assert_eq!(buf.as_slice(), b"llo world");
        assert_eq!(buf.len(), 9);
    }

    #[test]
    fn fully_consumed_buffer_resets() {
        let mut buf = InBuf::new();
        let mut src: &[u8] = b"abc";
        buf.read_from(&mut src, 8).expect("slice read");
        buf.consume(3);
        assert!(buf.is_empty());
        assert_eq!(buf.start, 0);
        assert!(buf.data.is_empty());
    }

    #[test]
    fn drain_into_moves_at_most_max_bytes() {
        let mut buf = InBuf::new();
        let mut src: &[u8] = b"0123456789";
        buf.read_from(&mut src, 16).expect("slice read");
        let mut out = Vec::new();
        assert_eq!(buf.drain_into(&mut out, 4), 4);
        assert_eq!(out, b"0123");
        assert_eq!(buf.as_slice(), b"456789");
    }

    #[test]
    fn eof_leaves_buffer_untouched() {
        let mut buf = InBuf::new();
        let mut src: &[u8] = b"";
        assert_eq!(buf.read_from(&mut src, 8).expect("slice read"), 0);
        assert!(buf.is_empty());
    }
}
