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
