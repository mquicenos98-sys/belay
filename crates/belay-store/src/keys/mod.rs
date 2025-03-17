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
/// Follows [`ESCAPE`] at the end of a byte string.
const TERMINATOR: u8 = 0x01;

/// Builds an order-preserving composite key. Methods consume and return
/// the builder so keys read as one expression.
///
/// See the [module docs](self) for the encoding of each element.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyBuf {
    buf: Vec<u8>,
}

impl KeyBuf {
    /// An empty key.
    #[must_use]
    pub fn new() -> KeyBuf {
        KeyBuf::default()
    }

    /// A key starting with `prefix` verbatim, e.g. a namespace prefix.
    #[must_use]
    pub fn with_prefix(prefix: &[u8]) -> KeyBuf {
        KeyBuf {
            buf: prefix.to_vec(),
        }
    }

    /// Appends one byte.
    #[must_use]
    pub fn u8(mut self, v: u8) -> KeyBuf {
        self.buf.push(v);
        self
    }

    /// Appends a big-endian `u16`.
    #[must_use]
    pub fn u16(self, v: u16) -> KeyBuf {
        self.raw(&v.to_be_bytes())
    }

    /// Appends a big-endian `u32`.
    #[must_use]
    pub fn u32(self, v: u32) -> KeyBuf {
        self.raw(&v.to_be_bytes())
    }

    /// Appends a big-endian `u64`.
    #[must_use]
    pub fn u64(self, v: u64) -> KeyBuf {
        self.raw(&v.to_be_bytes())
    }

    /// Appends a `u64` that sorts in descending order, e.g. to scan the
    /// newest sequence numbers first without a reverse scan.
    #[must_use]
    pub fn u64_desc(self, v: u64) -> KeyBuf {
        self.u64(!v)
    }

    /// Appends an `i32` so that negative values sort before positive ones.
    #[must_use]
    pub fn i32(self, v: i32) -> KeyBuf {
        self.u32((v as u32) ^ (1 << 31))
    }

    /// Appends an `i64` so that negative values sort before positive ones.
    #[must_use]
    pub fn i64(self, v: i64) -> KeyBuf {
        self.u64((v as u64) ^ (1 << 63))
    }

    /// Appends a `u64` in 1 to 9 bytes, still in numeric order: a length
    /// byte, then the value's significant bytes big-endian. Compact for
    /// counters that are usually small.
    #[must_use]
    pub fn uvarint(mut self, v: u64) -> KeyBuf {
        let bytes = v.to_be_bytes();
        let skip = (v.leading_zeros() / 8) as usize;
        self.buf.push((8 - skip) as u8);
        self.buf.extend_from_slice(&bytes[skip..]);
        self
    }

    /// Appends `0` or `1`.
    #[must_use]
    pub fn bool(self, v: bool) -> KeyBuf {
        self.u8(u8::from(v))
    }

    /// Appends an escaped, terminated byte string.
    #[must_use]
    pub fn bytes(mut self, v: &[u8]) -> KeyBuf {
        escape_into(&mut self.buf, v);
        self.buf.extend_from_slice(&[ESCAPE, TERMINATOR]);
        self
    }

    /// Appends an escaped, terminated string.
    #[must_use]
    pub fn str(self, v: &str) -> KeyBuf {
        self.bytes(v.as_bytes())
    }

    /// Appends `v` *without* the terminator. Only useful as the
    /// last element of a scan prefix: `KeyBuf::new().str("tag").str_prefix("pri")`
    /// covers every key whose second element starts with `"pri"`.
    #[must_use]
    pub fn str_prefix(self, v: &str) -> KeyBuf {
        self.bytes_prefix(v.as_bytes())
    }

    /// Byte-string version of [`str_prefix`](Self::str_prefix).
    #[must_use]
    pub fn bytes_prefix(mut self, v: &[u8]) -> KeyBuf {
        self.buf.extend_from_slice(v);
        self
    }

    /// Appends a timestamp as signed milliseconds.
    #[must_use]
    pub fn timestamp(self, ts: Timestamp) -> KeyBuf {
        self.i64(ts.as_millis())
    }

    /// Appends a ULID's 16 big-endian bytes, which sort by creation time.
    #[must_use]
    pub fn ulid(self, id: Ulid) -> KeyBuf {
        self.raw(&id.to_bytes())
    }

    /// Appends bytes of a width that every key has at this position (a
    /// hash, a fixed-size id). Variable-width data belongs in
    /// [`bytes`](Self::bytes).
    #[must_use]
    pub fn fixed(self, v: &[u8]) -> KeyBuf {
        self.raw(v)
    }

    /// Appends bytes verbatim, with no ordering guarantees of its own.
    #[must_use]
    pub fn raw(mut self, v: &[u8]) -> KeyBuf {
        self.buf.extend_from_slice(v);
        self
    }

    /// The encoded key.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the builder, returning the encoded key.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// Encoded length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// See [`prefix_end`].
    #[must_use]
    pub fn prefix_end(&self) -> Option<Vec<u8>> {
        prefix_end(&self.buf)
    }

    /// The range of keys starting with this one.
    #[must_use]
    pub fn range(&self) -> KeyRange {
        KeyRange::prefix(&self.buf)
    }
}

fn escape_into(out: &mut Vec<u8>, v: &[u8]) {
    out.reserve(v.len() + 2);
    for &b in v {
        if b == ESCAPE {
            out.extend_from_slice(&[ESCAPE, ESCAPED_ZERO]);
        } else {
            out.push(b);
        }
    }
}

impl AsRef<[u8]> for KeyBuf {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

impl From<KeyBuf> for Vec<u8> {
    fn from(k: KeyBuf) -> Vec<u8> {
        k.buf
    }
}

impl fmt::Debug for KeyBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyBuf(\"{}\")", Printable(&self.buf))
    }
}

/// The smallest key greater than every key that starts with `prefix`, or
/// `None` if there is none (the prefix is empty or all `0xFF`), in which
/// case the range of keys with this prefix is unbounded above.
///
/// ```
/// use belay_store::keys::prefix_end;
///
/// assert_eq!(prefix_end(b"job/"), Some(b"job0".to_vec()));
/// assert_eq!(prefix_end(&[1, 0xFF]), Some(vec![2]));
/// assert_eq!(prefix_end(&[0xFF, 0xFF]), None);
/// ```
#[must_use]
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let last = prefix.iter().rposition(|&b| b != 0xFF)?;
    let mut end = prefix[..=last].to_vec();
    end[last] += 1;
    Some(end)
}

/// Displays a key with printable ASCII kept and other bytes as `\xNN`,
/// for logs and the inspection tools.
///
/// ```
/// use belay_store::keys::{KeyBuf, Printable};
///
/// let key = KeyBuf::new().str("job").u16(258).into_vec();
/// assert_eq!(Printable(&key).to_string(), r"job\x00\x01\x01\x02");
/// ```
#[derive(Clone, Copy)]
pub struct Printable<'a>(pub &'a [u8]);

impl fmt::Display for Printable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.0 {
            match b {
                b'\\' => f.write_str("\\\\")?,
                0x20..=0x7E => fmt::Write::write_char(f, char::from(b))?,
                _ => write!(f, "\\x{b:02x}")?,
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Printable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Rng;
    use std::cmp::Ordering;

    #[test]
    fn integers_use_big_endian_and_flip_the_sign_bit() {
        assert_eq!(KeyBuf::new().u16(0x0102).as_bytes(), &[1, 2]);
        assert_eq!(KeyBuf::new().u64(1).as_bytes(), &[0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            KeyBuf::new().i64(-1).as_bytes(),
            &[0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            KeyBuf::new().i64(0).as_bytes(),
            &[0x80, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(KeyBuf::new().i32(i32::MIN).as_bytes(), &[0, 0, 0, 0]);
        assert_eq!(KeyBuf::new().u64_desc(0).as_bytes(), &[0xFF; 8]);
    }

    #[test]
    fn uvarint_is_compact_and_ordered() {
        assert_eq!(KeyBuf::new().uvarint(0).as_bytes(), &[0]);
        assert_eq!(KeyBuf::new().uvarint(255).as_bytes(), &[1, 255]);
        assert_eq!(KeyBuf::new().uvarint(256).as_bytes(), &[2, 1, 0]);
        assert_eq!(KeyBuf::new().uvarint(u64::MAX).len(), 9);
        let mut values: Vec<u64> = (0..64)
            .flat_map(|shift| {
                let p = 1u64 << shift;
                [p - 1, p, p + 1]
            })
            .chain([u64::MAX - 1, u64::MAX])
            .collect();
        values.sort_unstable();
        values.dedup();
        for pair in values.windows(2) {
            let a = KeyBuf::new().uvarint(pair[0]).into_vec();
            let b = KeyBuf::new().uvarint(pair[1]).into_vec();
            assert!(a < b, "{} vs {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn prefix_end_edge_cases() {
        assert_eq!(prefix_end(b""), None);
        assert_eq!(prefix_end(&[0xFF]), None);
        assert_eq!(prefix_end(&[0x00]), Some(vec![0x01]));
        assert_eq!(prefix_end(&[7, 0xFF, 0xFF]), Some(vec![8]));
        let p = KeyBuf::new().str("job");
        let end = p.prefix_end().expect("bounded");
        let inside = KeyBuf::new().str("job").u64(u64::MAX).into_vec();
        let outside = KeyBuf::new().str("job\0").into_vec();
        assert!(inside.as_slice() < end.as_slice());
        assert!(outside.as_slice() >= end.as_slice());
        assert!(p.range().contains(&inside));
        assert!(!p.range().contains(&outside));
    }

    #[test]
    fn a_tuple_sorts_before_its_extensions_and_prefixes_match() {
        let short = KeyBuf::new().str("a").into_vec();
        let long = KeyBuf::new().str("a").u64(0).into_vec();
        assert!(short < long);
        assert!(long.starts_with(&short));
        // Element boundaries are respected: ("ab") is not under ("a").
        let other = KeyBuf::new().str("ab").into_vec();
        assert!(!other.starts_with(&short));
        // str_prefix matches partial strings.
        let partial = KeyBuf::new().str_prefix("a").into_vec();
        assert!(other.starts_with(&partial));
    }

    #[test]
    fn printable_escapes_non_ascii_and_backslashes() {
        assert_eq!(Printable(b"a\\b\x7f\n").to_string(), r"a\\b\x7f\x0a");
        assert_eq!(format!("{:?}", Printable(b"q")), "\"q\"");
        assert_eq!(
            format!("{:?}", KeyBuf::new().str("x")),
            r#"KeyBuf("x\x00\x01")"#
        );
    }

    // ---- property test: encoded order == tuple order -------------------

    #[derive(Clone, Copy, Debug)]
    enum Kind {
        U64,
        I64,
        Bytes,
        Str,
        Bool,
        Var,
        Desc,
        I32,
    }

    const KINDS: [Kind; 8] = [
        Kind::U64,
        Kind::I64,
        Kind::Bytes,
        Kind::Str,
        Kind::Bool,
        Kind::Var,
        Kind::Desc,
        Kind::I32,
    ];

    #[derive(Clone, Debug, PartialEq)]
    enum Elem {
        U64(u64),
        I64(i64),
        Bytes(Vec<u8>),
        Str(String),
        Bool(bool),
        Var(u64),
        Desc(u64),
        I32(i32),
    }

    fn cmp_elem(a: &Elem, b: &Elem) -> Ordering {
        match (a, b) {
            (Elem::U64(x), Elem::U64(y)) | (Elem::Var(x), Elem::Var(y)) => x.cmp(y),
            (Elem::Desc(x), Elem::Desc(y)) => y.cmp(x),
            (Elem::I64(x), Elem::I64(y)) => x.cmp(y),
            (Elem::I32(x), Elem::I32(y)) => x.cmp(y),
            (Elem::Bytes(x), Elem::Bytes(y)) => x.cmp(y),
            (Elem::Str(x), Elem::Str(y)) => x.cmp(y),
            (Elem::Bool(x), Elem::Bool(y)) => x.cmp(y),
            _ => panic!("tuples with different layouts: {a:?} vs {b:?}"),
        }
    }

    fn cmp_tuple(a: &[Elem], b: &[Elem]) -> Ordering {
        for (x, y) in a.iter().zip(b) {
            match cmp_elem(x, y) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        a.len().cmp(&b.len())
    }

    fn gen_u64(rng: &mut Rng) -> u64 {
        const EDGES: [u64; 6] = [0, 1, 255, 256, u64::MAX - 1, u64::MAX];
        match rng.gen_range(0..3u32) {
            0 => *rng.choose(&EDGES).expect("non-empty"),
            1 => rng.gen_range(0..1000u64),
            _ => rng.next_u64() >> rng.gen_range(0..64u32),
        }
    }

    fn gen_bytes(rng: &mut Rng) -> Vec<u8> {
        // A tiny alphabet around the escape bytes makes prefixes, embedded
        // zeros and 0xFF runs common.
        const ALPHABET: [u8; 6] = [0x00, 0x01, 0x02, b'a', 0xFE, 0xFF];
        let len = rng.gen_range(0..5usize);
        (0..len)
            .map(|_| *rng.choose(&ALPHABET).expect("non-empty"))
            .collect()
    }

    fn gen_elem(kind: Kind, rng: &mut Rng) -> Elem {
        match kind {
            Kind::U64 => Elem::U64(gen_u64(rng)),
            Kind::Var => Elem::Var(gen_u64(rng)),
            Kind::Desc => Elem::Desc(gen_u64(rng)),
            Kind::I64 => Elem::I64(gen_u64(rng) as i64),
            Kind::I32 => Elem::I32(gen_u64(rng) as i32),
            Kind::Bytes => Elem::Bytes(gen_bytes(rng)),
            Kind::Str => {
                let chars = ['\0', 'a', 'b', 'é', '\u{7f}'];
                let len = rng.gen_range(0..4usize);
                Elem::Str(
                    (0..len)
                        .map(|_| *rng.choose(&chars).expect("non-empty"))
                        .collect(),
                )
            }
            Kind::Bool => Elem::Bool(rng.gen_bool(0.5)),
        }
    }

    fn encode(tuple: &[Elem]) -> Vec<u8> {
        tuple
            .iter()
            .fold(KeyBuf::new(), |k, e| match e {
                Elem::U64(v) => k.u64(*v),
                Elem::Var(v) => k.uvarint(*v),
                Elem::Desc(v) => k.u64_desc(*v),
                Elem::I64(v) => k.i64(*v),
                Elem::I32(v) => k.i32(*v),
                Elem::Bytes(v) => k.bytes(v),
                Elem::Str(v) => k.str(v),
                Elem::Bool(v) => k.bool(*v),
            })
            .into_vec()
    }

    fn decode(key: &[u8], layout: &[Kind], len: usize) -> Vec<Elem> {
        let mut r = KeyReader::new(key);
        let out = layout[..len]
            .iter()
            .map(|kind| match kind {
                Kind::U64 => Elem::U64(r.u64().expect("u64")),
                Kind::Var => Elem::Var(r.uvarint().expect("uvarint")),
                Kind::Desc => Elem::Desc(r.u64_desc().expect("desc")),
                Kind::I64 => Elem::I64(r.i64().expect("i64")),
                Kind::I32 => Elem::I32(r.i32().expect("i32")),
                Kind::Bytes => Elem::Bytes(r.bytes().expect("bytes")),
                Kind::Str => Elem::Str(r.str().expect("str")),
                Kind::Bool => Elem::Bool(r.bool().expect("bool")),
            })
            .collect();
        r.expect_end().expect("fully consumed");
        out
    }

    #[test]
    fn encoded_order_matches_tuple_order() {
        let mut rng = Rng::seed_from_u64(0x6b65_7973);
        for _ in 0..60 {
            let layout: Vec<Kind> = (0..rng.gen_range(1..5usize))
                .map(|_| *rng.choose(&KINDS).expect("non-empty"))
                .collect();
            let tuples: Vec<Vec<Elem>> = (0..60)
                .map(|_| {
                    let len = rng.gen_range(0..=layout.len());
                    layout[..len]
                        .iter()
                        .map(|&k| gen_elem(k, &mut rng))
                        .collect()
                })
                .collect();
            let encoded: Vec<Vec<u8>> = tuples.iter().map(|t| encode(t)).collect();
            for (i, a) in tuples.iter().enumerate() {
                assert_eq!(decode(&encoded[i], &layout, a.len()), *a);
                for (j, b) in tuples.iter().enumerate() {
                    assert_eq!(
                        encoded[i].cmp(&encoded[j]),
                        cmp_tuple(a, b),
                        "layout {layout:?}: {a:?} vs {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn prefix_scans_select_exactly_the_matching_tuples() {
        let mut rng = Rng::seed_from_u64(0x7072_6566);
        let layout = [Kind::Str, Kind::Bytes, Kind::U64];
        for _ in 0..200 {
            let a: Vec<Elem> = layout.iter().map(|&k| gen_elem(k, &mut rng)).collect();
            let b: Vec<Elem> = layout.iter().map(|&k| gen_elem(k, &mut rng)).collect();
            for n in 0..=2 {
                let prefix = encode(&a[..n]);
                let shares = a[..n] == b[..n];
                assert_eq!(encode(&b).starts_with(&prefix), shares, "{a:?} {b:?} n={n}");
            }
        }
    }
}
