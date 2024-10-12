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
