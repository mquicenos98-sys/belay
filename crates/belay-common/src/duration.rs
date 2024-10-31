//! Human-writable durations: `"1h30m"`, `"250ms"`, `"1.5s"`, `"2d"`.
//!
//! The grammar is a sequence of `<number><unit>` components with units in
//! strictly decreasing order (`1h30m`, not `30m1h`). Numbers may have a
//! fractional part (`1.5h`). A lone number without a unit means seconds, which
//! is what people usually mean in config files (`timeout = "90"`).
//!
//! Units: `ns`, `us` (also `µs`), `ms`, `s`, `m`, `h`, `d` (24 hours) and `w`
//! (7 days). Calendar units such as months are deliberately absent because
//! they do not have a fixed length.
//!
//! [`format_duration`] produces the same grammar and round-trips exactly:
//! `parse_duration(&format_duration(d)) == Ok(d)` for every `Duration`.

use std::fmt;
use std::time::Duration;

use crate::text::plural;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Unit table, largest first. The index doubles as the ordering rank.
const UNITS: [(&str, u128); 8] = [
    ("w", 7 * 86_400 * NANOS_PER_SEC),
    ("d", 86_400 * NANOS_PER_SEC),
    ("h", 3_600 * NANOS_PER_SEC),
    ("m", 60 * NANOS_PER_SEC),
    ("s", NANOS_PER_SEC),
    ("ms", 1_000_000),
    ("us", 1_000),
    ("ns", 1),
];

/// Fraction digits beyond this cannot change the result at nanosecond
/// resolution for any unit up to a week, and capping them keeps the
/// arithmetic inside `u128`.
const MAX_FRACTION_DIGITS: u32 = 18;

/// Parses a duration such as `"1h30m15s"`, `"250ms"`, `"1.5s"` or `"90"`.
///
/// ```
/// use belay_common::duration::parse_duration;
/// use std::time::Duration;
///
/// assert_eq!(parse_duration("1h30m"), Ok(Duration::from_secs(5_400)));
/// assert_eq!(parse_duration("1.5s"), Ok(Duration::from_millis(1_500)));
/// assert_eq!(parse_duration("90"), Ok(Duration::from_secs(90)));
/// assert!(parse_duration("30m1h").is_err());
/// assert!(parse_duration("1h 30m").is_err());
/// ```
pub fn parse_duration(s: &str) -> Result<Duration, ParseDurationError> {
    let fail = |kind| ParseDurationError {
        input: s.to_owned(),
        kind,
    };
    if s.is_empty() {
        return Err(fail(DurationErrorKind::Empty));
    }
    if s.starts_with('-') {
        return Err(fail(DurationErrorKind::Negative));
    }

    let bytes = s.as_bytes();
    let mut pos = 0;
    let mut total: u128 = 0;
    let mut previous_rank: Option<usize> = None;

    while pos < bytes.len() {
        let number = parse_number(bytes, &mut pos).map_err(fail)?;

        let unit_start = pos;
        while pos < bytes.len() && !bytes[pos].is_ascii_digit() && bytes[pos] != b'.' {
            pos += 1;
        }
        // `pos` only stops at ASCII bytes or the end, so this slice is on
        // character boundaries.
        let unit = &s[unit_start..pos];

        if unit.is_empty() {
            if pos == bytes.len() {
                // A number at the end without a unit is seconds.
                total = number
                    .scaled(NANOS_PER_SEC)
                    .and_then(|n| total.checked_add(n))
                    .ok_or_else(|| fail(DurationErrorKind::Overflow))?;
                break;
            }
            return Err(fail(DurationErrorKind::MissingUnit { pos }));
        }
        let rank = unit_rank(unit).ok_or_else(|| fail(unknown_unit(unit, unit_start)))?;
        if let Some(prev) = previous_rank {
            if rank <= prev {
                return Err(fail(DurationErrorKind::UnitOrder {
                    unit: UNITS[rank].0,
                    previous: UNITS[prev].0,
                }));
            }
        }
        previous_rank = Some(rank);
        total = number
            .scaled(UNITS[rank].1)
            .and_then(|n| total.checked_add(n))
            .ok_or_else(|| fail(DurationErrorKind::Overflow))?;
    }

    let secs =
        u64::try_from(total / NANOS_PER_SEC).map_err(|_| fail(DurationErrorKind::Overflow))?;
    Ok(Duration::new(secs, (total % NANOS_PER_SEC) as u32))
}

/// A decimal number split into integer and fractional parts.
struct Number {
    int: u128,
    frac: u128,
    frac_digits: u32,
}

impl Number {
    /// `self * unit_nanos`, truncating below one nanosecond.
    fn scaled(&self, unit_nanos: u128) -> Option<u128> {
        let whole = self.int.checked_mul(unit_nanos)?;
        let part = self.frac * unit_nanos / 10u128.pow(self.frac_digits);
        whole.checked_add(part)
    }
}

fn parse_number(bytes: &[u8], pos: &mut usize) -> Result<Number, DurationErrorKind> {
    let start = *pos;
    let mut int: u128 = 0;
    while let Some(&b) = bytes.get(*pos).filter(|b| b.is_ascii_digit()) {
        int = int
            .checked_mul(10)
            .and_then(|v| v.checked_add(u128::from(b - b'0')))
            .ok_or(DurationErrorKind::Overflow)?;
        *pos += 1;
    }
    if *pos == start {
        return Err(DurationErrorKind::InvalidNumber { pos: start });
    }
    let mut number = Number {
        int,
        frac: 0,
        frac_digits: 0,
    };
    if bytes.get(*pos) == Some(&b'.') {
        *pos += 1;
        let frac_start = *pos;
        while let Some(&b) = bytes.get(*pos).filter(|b| b.is_ascii_digit()) {
            if number.frac_digits < MAX_FRACTION_DIGITS {
                number.frac = number.frac * 10 + u128::from(b - b'0');
                number.frac_digits += 1;
            }
            *pos += 1;
        }
        if *pos == frac_start {
            return Err(DurationErrorKind::InvalidNumber { pos: frac_start });
        }
    }
    Ok(number)
}

fn unit_rank(unit: &str) -> Option<usize> {
    let canonical = match unit {
        "µs" | "μs" => "us",
        other => other,
    };
    UNITS.iter().position(|(name, _)| *name == canonical)
}

fn unknown_unit(unit: &str, pos: usize) -> DurationErrorKind {
    // A unit run swallows everything up to the next digit, so a stray
    // space or sign shows up here; report it as a character problem.
    match unit.char_indices().find(|(_, c)| !c.is_alphabetic()) {
        Some((offset, ch)) => DurationErrorKind::UnexpectedChar {
            ch,
            pos: pos + offset,
        },
        None => DurationErrorKind::UnknownUnit(unit.to_owned()),
    }
}

/// Formats a duration in the compact, exact form accepted by
/// [`parse_duration`]: `1h30m`, `250ms`, `1d2h`, `1s500ms`, `0s`.
///
/// Days are the largest unit used; weeks are accepted on input only.
#[must_use]
pub fn format_duration(d: Duration) -> String {
    CompactDuration(d).to_string()
}

/// `Display` adapter for [`format_duration`] that avoids an intermediate
/// `String`, e.g. inside `format!` or log fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactDuration(pub Duration);
