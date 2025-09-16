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
