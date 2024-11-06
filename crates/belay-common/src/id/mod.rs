//! ULIDs: 128-bit identifiers that sort by creation time.
//!
//! Layout (<https://github.com/ulid/spec>): a 48-bit big-endian millisecond
//! Unix timestamp followed by 80 random bits. The text form is 26 characters
//! of Crockford base32 (`0-9A-Z` without `I L O U`), most significant first,
//! so string order, byte order and numeric order all agree.
//!
//! Job and workflow ids are ULIDs: a store scan over `job/<ulid>` keys walks
//! jobs in creation order without a separate index.

mod generator;

use std::fmt;
use std::str::FromStr;

use crate::rand::Rng;
use crate::time::Timestamp;

pub use generator::{SyncUlidGenerator, UlidGenerator, UlidOverflow};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

const RANDOM_BITS: u32 = 80;

const RANDOM_MASK: u128 = (1 << RANDOM_BITS) - 1;
