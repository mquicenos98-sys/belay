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
        // A non-ASCII character is never valid; naming it beats reporting a
        // byte length that does not match what the user sees.
        if let Some((pos, ch)) = s.chars().enumerate().find(|(_, c)| !c.is_ascii()) {
            return Err(ParseUlidError::InvalidChar { ch, pos });
        }
        let bytes = s.as_bytes();
        if bytes.len() != Ulid::TEXT_LEN {
            return Err(ParseUlidError::InvalidLength(bytes.len()));
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

impl FromStr for Ulid {
    type Err = ParseUlidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::parse(s)
    }
}

impl From<u128> for Ulid {
    fn from(value: u128) -> Self {
        Ulid(value)
    }
}

impl From<Ulid> for u128 {
    fn from(id: Ulid) -> Self {
        id.0
    }
}

impl From<[u8; 16]> for Ulid {
    fn from(bytes: [u8; 16]) -> Self {
        Ulid::from_bytes(bytes)
    }
}

impl From<Ulid> for [u8; 16] {
    fn from(id: Ulid) -> Self {
        id.to_bytes()
    }
}

/// Error returned when parsing a [`Ulid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseUlidError {
    /// The input is not 26 characters long.
    InvalidLength(usize),
    /// A character outside the Crockford base32 alphabet.
    InvalidChar {
        /// The character.
        ch: char,
        /// Its character (not byte) position.
        pos: usize,
    },
    /// The first character is above `7`, so the value needs more than
    /// 128 bits.
    Overflow,
}

impl fmt::Display for ParseUlidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseUlidError::InvalidLength(n) => {
                write!(f, "invalid ULID: expected 26 characters, got {n}")
            }
            ParseUlidError::InvalidChar { ch, pos } => write!(
                f,
                "invalid ULID: {ch:?} at position {pos} is not a base32 digit (0-9, A-Z except I, L, O, U)"
            ),
            ParseUlidError::Overflow => {
                f.write_str("invalid ULID: value exceeds 128 bits (first character must be 0-7)")
            }
        }
    }
}

impl std::error::Error for ParseUlidError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nil_and_max_have_canonical_text() {
        assert_eq!(Ulid::nil().to_string(), "00000000000000000000000000");
        assert_eq!(Ulid::MAX.to_string(), "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
        assert_eq!("7ZZZZZZZZZZZZZZZZZZZZZZZZZ".parse(), Ok(Ulid::MAX));
        assert_eq!("00000000000000000000000000".parse(), Ok(Ulid::nil()));
        assert!(Ulid::nil().is_nil() && !Ulid::MAX.is_nil());
        assert_eq!(Ulid::default(), Ulid::nil());
    }

    #[test]
    fn rejects_values_beyond_128_bits() {
        assert_eq!(
            "80000000000000000000000000".parse::<Ulid>(),
            Err(ParseUlidError::Overflow)
        );
        assert_eq!(
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZ".parse::<Ulid>(),
            Err(ParseUlidError::Overflow)
        );
    }

    #[test]
    fn decodes_the_reference_timestamp() {
        // From the reference implementation's test suite.
        let id: Ulid = "01ARYZ6S41TSV4RRFFQ69G5FAV".parse().expect("valid ULID");
        assert_eq!(id.timestamp_ms(), 1_469_918_176_385);
        assert_eq!(id.timestamp().to_rfc3339(), "2016-07-30T22:36:16.385Z");
        let min = Ulid::min_at(Timestamp::from_millis(1_469_918_176_385));
        assert_eq!(&min.to_string()[..10], "01ARYZ6S41");
    }

    #[test]
    fn parsing_is_case_insensitive_and_display_is_uppercase() {
        let lower: Ulid = "01arz3ndektsv4rrffq69g5fav".parse().expect("valid ULID");
        let upper: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("valid ULID");
        assert_eq!(lower, upper);
        assert_eq!(lower.to_string(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(lower.timestamp_ms(), 1_469_922_850_259);
    }

    #[test]
    fn rejects_excluded_letters_with_their_position() {
        for (bad, ch) in [('I', 'I'), ('l', 'l'), ('O', 'O'), ('u', 'u')] {
            let mut s = String::from("01ARZ3NDEKTSV4RRFFQ69G5FAV");
            s.replace_range(4..5, &bad.to_string());
            assert_eq!(
                s.parse::<Ulid>(),
                Err(ParseUlidError::InvalidChar { ch, pos: 4 })
            );
        }
        let err = "01ARZ3NDEKTSV4RRFFQ69G5FA-".parse::<Ulid>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid ULID: '-' at position 25 is not a base32 digit (0-9, A-Z except I, L, O, U)"
        );
    }

    #[test]
    fn rejects_wrong_lengths_and_non_ascii() {
        assert_eq!("".parse::<Ulid>(), Err(ParseUlidError::InvalidLength(0)));
        assert_eq!(
            "01ARZ3NDEKTSV4RRFFQ69G5FA".parse::<Ulid>(),
            Err(ParseUlidError::InvalidLength(25))
        );
        // Regression: 26 characters but 27 bytes used to be reported as
        // "expected 26 characters, got 26".
        assert_eq!(
            "01ARZ3NDEKTSV4RRFFQ69G5FAé".parse::<Ulid>(),
            Err(ParseUlidError::InvalidChar { ch: 'é', pos: 25 })
        );
        assert_eq!(
            "é".parse::<Ulid>(),
            Err(ParseUlidError::InvalidChar { ch: 'é', pos: 0 })
        );
    }

    #[test]
    fn parts_are_masked_and_recoverable() {
        let id = Ulid::from_parts(u64::MAX, u128::MAX);
        assert_eq!(id, Ulid::MAX);
        let id = Ulid::from_parts(1_700_000_000_000, 0xABCD);
        assert_eq!(id.timestamp_ms(), 1_700_000_000_000);
        assert_eq!(id.random(), 0xABCD);
        assert_eq!(
            Ulid::from_parts(1, 0).to_string(),
            "00000000010000000000000000"
        );
    }

    #[test]
    fn bytes_and_u128_round_trip_big_endian() {
        let id = Ulid::from_parts(0x0102_0304_0506, 0x0708_090A_0B0C_0D0E_0F10);
        let bytes = id.to_bytes();
        assert_eq!(
            bytes,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert_eq!(Ulid::from_bytes(bytes), id);
        assert_eq!(Ulid::from(<[u8; 16]>::from(id)), id);
        assert_eq!(Ulid::from(u128::from(id)), id);
        assert_eq!(Ulid::from_u128(id.to_u128()), id);
    }

    #[test]
    fn text_bytes_and_numeric_order_agree() {
        let mut rng = Rng::seed_from_u64(11);
        let mut ids: Vec<Ulid> = (0..500)
            .map(|_| {
                let raw =
                    Ulid::from_u128(u128::from(rng.next_u64()) << 64 | u128::from(rng.next_u64()));
                // Vary the timestamp magnitude so short and long prefixes mix.
                Ulid::from_parts(raw.timestamp_ms() >> rng.gen_range(0..48u32), raw.random())
            })
            .collect();
        ids.sort();
        let texts: Vec<String> = ids.iter().map(Ulid::to_string).collect();
        let bytes: Vec<[u8; 16]> = ids.iter().map(|id| id.to_bytes()).collect();
        assert!(texts.windows(2).all(|w| w[0] <= w[1]));
        assert!(bytes.windows(2).all(|w| w[0] <= w[1]));
        for (id, text) in ids.iter().zip(&texts) {
            assert_eq!(text.parse::<Ulid>(), Ok(*id));
        }
    }

    #[test]
    fn increment_stays_within_the_millisecond() {
        let id = Ulid::from_parts(5, 41);
        assert_eq!(id.increment().map(Ulid::random), Some(42));
        assert_eq!(Ulid::max_at(Timestamp::from_millis(5)).increment(), None);
        assert_eq!(Ulid::MAX.increment(), None);
    }

    #[test]
    fn negative_timestamps_clamp_to_zero() {
        let mut rng = Rng::seed_from_u64(1);
        let id = Ulid::with_random(Timestamp::from_millis(-5), &mut rng);
        assert_eq!(id.timestamp_ms(), 0);
        assert_eq!(Ulid::min_at(Timestamp::MIN), Ulid::nil());
    }

    #[test]
    fn far_future_timestamps_saturate() {
        assert_eq!(Ulid::MAX.timestamp(), Timestamp::MAX);
        let ts = Timestamp::MAX;
        assert_eq!(Ulid::min_at(ts).timestamp(), ts);
    }

    #[test]
    fn display_supports_padding_and_debug_is_tagged() {
        let id = Ulid::from_parts(1, 1);
        assert_eq!(format!("{id:>28}"), "  00000000010000000000000001");
        assert_eq!(format!("{id:?}"), "Ulid(00000000010000000000000001)");
    }

    #[test]
    fn encode_writes_into_a_caller_buffer() {
        let mut buf = [0u8; Ulid::TEXT_LEN];
        let id = Ulid::from_parts(1_469_918_176_385, 0);
        assert_eq!(id.encode(&mut buf), "01ARYZ6S410000000000000000");
    }
}
