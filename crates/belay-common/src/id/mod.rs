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

const TIMESTAMP_MASK: u64 = (1 << 48) - 1;

const INVALID: u8 = 0xFF;

/// Maps ASCII bytes to base32 digit values; `INVALID` elsewhere. Lowercase
/// is accepted; the excluded letters I, L, O and U are not, so that each ID
/// has exactly one spelling (modulo case).
const DECODE: [u8; 256] = {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < 32 {
        let c = ALPHABET[i];
        table[c as usize] = i as u8;
        table[c.to_ascii_lowercase() as usize] = i as u8;
        i += 1;
    }
    table
};

/// A ULID.
///
/// ```
/// use belay_common::id::Ulid;
///
/// let id: Ulid = "01ARYZ6S41TSV4RRFFQ69G5FAV".parse().unwrap();
/// assert_eq!(id.timestamp_ms(), 1_469_918_176_385);
/// assert_eq!(id.to_string(), "01ARYZ6S41TSV4RRFFQ69G5FAV");
/// assert_eq!("01aryz6s41tsv4rrffq69g5fav".parse::<Ulid>().unwrap(), id);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ulid(u128);

impl Ulid {
    /// Length of the text form.
    pub const TEXT_LEN: usize = 26;
    /// The largest ULID, `7ZZZZZZZZZZZZZZZZZZZZZZZZZ`.
    pub const MAX: Ulid = Ulid(u128::MAX);

    /// The all-zero ULID, `00000000000000000000000000`; useful as a
    /// placeholder or lower bound.
    #[must_use]
    pub const fn nil() -> Ulid {
        Ulid(0)
    }

    /// Whether this is [`nil`](Self::nil).
    #[must_use]
    pub const fn is_nil(self) -> bool {
        self.0 == 0
    }

    /// Assembles a ULID from a millisecond timestamp (low 48 bits used) and
    /// randomness (low 80 bits used).
    #[must_use]
    pub const fn from_parts(timestamp_ms: u64, random: u128) -> Ulid {
        Ulid(((timestamp_ms & TIMESTAMP_MASK) as u128) << RANDOM_BITS | (random & RANDOM_MASK))
    }

    /// A ULID for `ts` with fresh randomness. Not monotonic within a
    /// millisecond; use a [`UlidGenerator`] when order matters.
    #[must_use]
    pub fn with_random(ts: Timestamp, rng: &mut Rng) -> Ulid {
        Ulid::from_parts(timestamp_bits(ts), random_bits(rng))
    }

    /// A new ULID from the system clock, strictly greater than every ULID
    /// previously returned by this function in this process.
    ///
    /// Uses a process-wide [`SyncUlidGenerator`]. Code that runs under a
    /// test clock should own a generator built from that clock instead.
    #[must_use]
    pub fn generate() -> Ulid {
        generator::global()
    }

    /// The smallest ULID with timestamp `ts`; with [`max_at`](Self::max_at),
    /// bounds a key range covering one instant.
    #[must_use]
    pub fn min_at(ts: Timestamp) -> Ulid {
        Ulid::from_parts(timestamp_bits(ts), 0)
    }

    /// The largest ULID with timestamp `ts`.
    #[must_use]
    pub fn max_at(ts: Timestamp) -> Ulid {
        Ulid::from_parts(timestamp_bits(ts), RANDOM_MASK)
    }

    /// The 48-bit millisecond timestamp.
    #[must_use]
    pub const fn timestamp_ms(self) -> u64 {
        (self.0 >> RANDOM_BITS) as u64
    }

    /// The timestamp as a [`Timestamp`]. ULID timestamps reach year 10889;
    /// ones after 9999 saturate to [`Timestamp::MAX`].
    #[must_use]
    pub fn timestamp(self) -> Timestamp {
        // At most 2^48 - 1, which fits in i64.
        Timestamp::from_millis(self.timestamp_ms() as i64)
    }

    /// The 80 random bits.
    #[must_use]
    pub const fn random(self) -> u128 {
        self.0 & RANDOM_MASK
    }

    /// The next ULID in the same millisecond (random part plus one), or
    /// `None` if the random part is already all ones.
    #[must_use]
    pub const fn increment(self) -> Option<Ulid> {
        if self.random() == RANDOM_MASK {
            None
        } else {
            Some(Ulid(self.0 + 1))
        }
    }

    /// The 128-bit value.
    #[must_use]
    pub const fn to_u128(self) -> u128 {
        self.0
    }

    /// From a 128-bit value.
    #[must_use]
    pub const fn from_u128(value: u128) -> Ulid {
        Ulid(value)
    }

    /// Big-endian bytes; byte-wise comparison matches ULID order, which
    /// makes them suitable as ordered store keys.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }

    /// From big-endian bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Ulid {
        Ulid(u128::from_be_bytes(bytes))
    }

    /// Writes the 26-character text form into `buf` without allocating.
    pub fn encode(self, buf: &mut [u8; Ulid::TEXT_LEN]) -> &str {
        for (i, slot) in buf.iter_mut().enumerate() {
            let shift = 125 - 5 * i as u32;
            *slot = ALPHABET[((self.0 >> shift) & 0x1F) as usize];
        }
        // Every byte comes from ALPHABET, which is ASCII.
        std::str::from_utf8(buf).expect("base32 alphabet is ASCII")
    }

    /// Parses the text form, accepting upper or lower case.
    pub fn parse(s: &str) -> Result<Ulid, ParseUlidError> {
        let bytes = s.as_bytes();
        if bytes.len() != Ulid::TEXT_LEN {
            return Err(ParseUlidError::InvalidLength(s.chars().count()));
        }
        let mut value: u128 = 0;
        for (i, &b) in bytes.iter().enumerate() {
            let digit = DECODE[usize::from(b)];
            if digit == INVALID {
                return Err(ParseUlidError::InvalidChar {
                    ch: char::from(b),
                    pos: i,
                });
            }
            if i == 0 && digit > 7 {
                // 26 digits hold 130 bits; the top two must be zero.
                return Err(ParseUlidError::Overflow);
            }
            value = value << 5 | u128::from(digit);
        }
        Ok(Ulid(value))
    }
}

/// Timestamps before 1970 are clamped to 0, the earliest ULID timestamp.
fn timestamp_bits(ts: Timestamp) -> u64 {
    u64::try_from(ts.as_millis()).unwrap_or(0)
}

fn random_bits(rng: &mut Rng) -> u128 {
    (u128::from(rng.next_u64()) << 64 | u128::from(rng.next_u64())) & RANDOM_MASK
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; Ulid::TEXT_LEN];
        f.pad(self.encode(&mut buf))
    }
}

impl fmt::Debug for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ulid({self})")
    }
}
