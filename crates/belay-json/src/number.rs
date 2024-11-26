//! JSON numbers: exact 64-bit integers alongside finite `f64`s.
//!
//! RFC 8259 §6 puts no bound on the range or precision of numbers, but notes
//! that interoperable documents stay within IEEE 754 doubles. belay keeps
//! integers exact whenever they fit in `u64`/`i64` so that sequence numbers,
//! millisecond timestamps and lease tokens survive a round trip bit for bit;
//! everything else is carried as an `f64`.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use crate::parse::ParseError;

/// A JSON number.
///
/// Every number has exactly one of three representations: non-negative
/// integers are stored as `u64`, negative integers as `i64`, and everything
/// else (fractions, exponents, integers wider than 64 bits) as a finite `f64`.
/// Because the integer forms are canonical, `Number::from(7u8) ==
/// Number::from(7i64)`.
///
/// # Equality
///
/// Integers compare equal to integers and floats to floats; an integer never
/// equals a float, so `1` and `1.0` are different numbers. This matches the
/// writer, which keeps the distinction in its output. Floats compare with IEEE
/// semantics, except that NaN can never be stored, which is what makes
/// `Number: Eq` sound. `0.0` and `-0.0` are equal and hash alike.
///
/// ```
/// use belay_json::Number;
///
/// assert_eq!(Number::from(7u8), Number::from(7i64));
/// assert_ne!(Number::from(1), Number::from_f64(1.0).unwrap());
/// assert!(Number::from_f64(f64::NAN).is_none());
/// ```
#[derive(Clone, Copy)]
pub struct Number(Repr);

#[derive(Clone, Copy)]
enum Repr {
    PosInt(u64),
    /// Always negative; non-negative integers use `PosInt`.
    NegInt(i64),
    /// Always finite.
    Float(f64),
}

impl Number {
    /// Wraps a finite float. Returns `None` for NaN and infinities, which JSON
    /// cannot represent.
    #[must_use]
    pub fn from_f64(f: f64) -> Option<Number> {
        f.is_finite().then_some(Number(Repr::Float(f)))
    }

    /// Wraps a float the caller has already checked to be finite.
    pub(crate) fn float_unchecked(f: f64) -> Number {
        debug_assert!(f.is_finite(), "non-finite float {f} in Number");
        Number(Repr::Float(f))
    }

    /// True for the two integer representations.
    #[must_use]
    pub fn is_integer(&self) -> bool {
        !matches!(self.0, Repr::Float(_))
    }

    /// True if the number is an integer that fits in `i64`.
    #[must_use]
    pub fn is_i64(&self) -> bool {
        self.as_i64().is_some()
    }

    /// True if the number is a non-negative integer (always fits in `u64`).
    #[must_use]
    pub fn is_u64(&self) -> bool {
        matches!(self.0, Repr::PosInt(_))
    }

    /// True if the number is stored as a float.
    #[must_use]
    pub fn is_f64(&self) -> bool {
        matches!(self.0, Repr::Float(_))
    }

    /// The value as `i64`, if it is an integer in range.
    ///
    /// Floats return `None` even when integral (`2.0`), because the
    /// document said "float"; [`FromJson`](crate::FromJson) for integer types
    /// is more forgiving and accepts integral floats.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self.0 {
            Repr::PosInt(u) => i64::try_from(u).ok(),
            Repr::NegInt(i) => Some(i),
            Repr::Float(_) => None,
        }
    }

    /// The value as `u64`, if it is a non-negative integer.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self.0 {
            Repr::PosInt(u) => Some(u),
            Repr::NegInt(_) | Repr::Float(_) => None,
        }
    }

    /// The value as `f64`. Integers above 2^53 lose precision.
    #[must_use]
    pub fn as_f64(&self) -> f64 {
        match self.0 {
            Repr::PosInt(u) => u as f64,
            Repr::NegInt(i) => i as f64,
            Repr::Float(f) => f,
        }
    }

    /// The exact integer value, if there is one: integers always, floats only
    /// when integral and within `i64::MIN..=u64::MAX`. Used for range-checked
    /// conversions into every Rust integer type.
    pub(crate) fn as_i128_exact(&self) -> Option<i128> {
        // 2^64 as f64 is exact; `f < 2^64` keeps `f as i128` lossless.
        const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
        match self.0 {
            Repr::PosInt(u) => Some(i128::from(u)),
            Repr::NegInt(i) => Some(i128::from(i)),
            Repr::Float(f) if f.fract() == 0.0 && f >= i64::MIN as f64 && f < TWO_POW_64 => {
                Some(f as i128)
            }
            Repr::Float(_) => None,
        }
    }
}

macro_rules! from_unsigned {
    ($($t:ty),*) => {$(
        impl From<$t> for Number {
            fn from(n: $t) -> Number {
                Number(Repr::PosInt(n as u64))
            }
        }
    )*};
}

macro_rules! from_signed {
    ($($t:ty),*) => {$(
        impl From<$t> for Number {
            fn from(n: $t) -> Number {
                let n = n as i64;
                if n < 0 {
                    Number(Repr::NegInt(n))
                } else {
                    Number(Repr::PosInt(n as u64))
                }
            }
        }
    )*};
}

// `usize`/`isize` are at most 64 bits on every platform Rust supports, so the
// `as` casts above are lossless.
from_unsigned!(u8, u16, u32, u64, usize);
from_signed!(i8, i16, i32, i64, isize);

impl PartialEq for Number {
    fn eq(&self, other: &Number) -> bool {
        match (self.0, other.0) {
            (Repr::PosInt(a), Repr::PosInt(b)) => a == b,
            (Repr::NegInt(a), Repr::NegInt(b)) => a == b,
            (Repr::Float(a), Repr::Float(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Number {}

impl Hash for Number {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.0 {
            Repr::PosInt(u) => {
                state.write_u8(0);
                state.write_u64(u);
            }
            Repr::NegInt(i) => {
                state.write_u8(1);
                state.write_i64(i);
            }
            Repr::Float(f) => {
                state.write_u8(2);
                // -0.0 == 0.0, so they must hash identically.
                let f = if f == 0.0 { 0.0 } else { f };
                state.write_u64(f.to_bits());
            }
        }
    }
}

impl fmt::Display for Number {
    /// Formats the number exactly as the JSON writer does.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Repr::PosInt(u) => write!(f, "{u}"),
            Repr::NegInt(i) => write!(f, "{i}"),
            Repr::Float(x) => write_f64(f, x),
        }
    }
}

impl fmt::Debug for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Number {
    type Err = ParseError;

    /// Parses a JSON number literal. The whole string must be the literal;
    /// surrounding whitespace is rejected.
    fn from_str(s: &str) -> Result<Number, ParseError> {
        crate::parse::parse_number_literal(s)
    }
}

/// Builds a number from an integer literal's digits (no sign, no leading
/// zeros). Returns `None` if the magnitude does not fit, in which case the
/// caller falls back to `f64`. `-0` also returns `None` so that it becomes the
/// float `-0.0` and keeps its sign.
pub(crate) fn integer_from_digits(digits: &[u8], negative: bool) -> Option<Number> {
    let mut magnitude: u64 = 0;
    for &d in digits {
        magnitude = magnitude
            .checked_mul(10)?
            .checked_add(u64::from(d - b'0'))?;
    }
    if !negative {
        return Some(Number(Repr::PosInt(magnitude)));
    }
    match magnitude {
        0 => None,
        m if m <= i64::MAX as u64 => Some(Number(Repr::NegInt(-(m as i64)))),
        m if m == i64::MIN.unsigned_abs() => Some(Number(Repr::NegInt(i64::MIN))),
        _ => None,
    }
}

/// Writes a finite float as the shortest decimal string that parses back to
/// the same `f64`, always marked as a float (`1.0`, never `1`).
///
/// The digits come from `{:e}`, which std guarantees to be the shortest
/// round-tripping representation; only the layout is decided here. Plain
/// decimal notation is used for exponents in `-7..21` (the same window
/// ECMAScript's `Number.prototype.toString` uses, so values look familiar to
/// JavaScript consumers) and scientific notation outside it.
pub(crate) fn write_f64<W: fmt::Write + ?Sized>(out: &mut W, f: f64) -> fmt::Result {
    debug_assert!(f.is_finite());
    if f == 0.0 {
        return out.write_str(if f.is_sign_negative() { "-0.0" } else { "0.0" });
    }
    let mut sci = StackBuf::new();
    fmt::write(&mut sci, format_args!("{:e}", f.abs()))?;
    let sci = sci.as_str();
    let (mantissa, exp) = sci.split_once('e').ok_or(fmt::Error)?;
    let exp: i32 = exp.parse().map_err(|_| fmt::Error)?;
    let (lead, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));

    if f < 0.0 {
        out.write_char('-')?;
    }
    if !(-7..21).contains(&exp) {
        out.write_str(lead)?;
        if !frac.is_empty() {
            out.write_char('.')?;
            out.write_str(frac)?;
        }
        return write!(out, "e{exp}");
    }
    if exp < 0 {
        out.write_str("0.")?;
        for _ in 0..(-exp - 1) {
            out.write_char('0')?;
        }
        out.write_str(lead)?;
        return out.write_str(frac);
    }
    // `exp` digits of `frac` belong before the decimal point.
    let exp = exp as usize;
    out.write_str(lead)?;
    if frac.len() > exp {
        out.write_str(&frac[..exp])?;
        out.write_char('.')?;
        out.write_str(&frac[exp..])
    } else {
        out.write_str(frac)?;
        for _ in frac.len()..exp {
            out.write_char('0')?;
        }
        out.write_str(".0")
    }
}

/// A fixed buffer for the `{:e}` rendering of one `f64`, so formatting a float
/// does not allocate. The longest rendering, e.g. `2.2250738585072014e-308`,
/// is 23 bytes.
struct StackBuf {
    bytes: [u8; 32],
    len: usize,
}
