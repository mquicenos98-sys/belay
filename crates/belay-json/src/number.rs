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
