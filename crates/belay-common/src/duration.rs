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
        let number_start = pos;
        let number = parse_number(bytes, &mut pos).map_err(fail)?;

        let unit_start = pos;
        while pos < bytes.len() && !bytes[pos].is_ascii_digit() && bytes[pos] != b'.' {
            pos += 1;
        }
        // `pos` only stops at ASCII bytes or the end, so this slice is on
        // character boundaries.
        let unit = &s[unit_start..pos];

        if unit.is_empty() {
            if number_start == 0 && pos == bytes.len() {
                // A lone number is seconds.
                total = number
                    .scaled(NANOS_PER_SEC)
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

impl fmt::Display for CompactDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rest = self.0.as_nanos();
        if rest == 0 {
            return f.write_str("0s");
        }
        // Skip "w": "14d" reads better than "2w" and both parse.
        for (name, size) in &UNITS[1..] {
            let count = rest / size;
            if count > 0 {
                write!(f, "{count}{name}")?;
                rest %= size;
            }
        }
        Ok(())
    }
}

/// A rough, human-friendly rendering using the largest unit that fits,
/// rounded down: `"3 minutes"`, `"2 hours"`, `"1 day"`, `"450 milliseconds"`.
///
/// ```
/// use belay_common::duration::humanize;
/// use std::time::Duration;
///
/// assert_eq!(humanize(Duration::from_secs(185)), "3 minutes");
/// assert_eq!(humanize(Duration::from_secs(3_600)), "1 hour");
/// ```
#[must_use]
pub fn humanize(d: Duration) -> String {
    const SCALES: [(&str, u128); 8] = [
        ("year", 365 * 86_400 * NANOS_PER_SEC),
        ("day", 86_400 * NANOS_PER_SEC),
        ("hour", 3_600 * NANOS_PER_SEC),
        ("minute", 60 * NANOS_PER_SEC),
        ("second", NANOS_PER_SEC),
        ("millisecond", 1_000_000),
        ("microsecond", 1_000),
        ("nanosecond", 1),
    ];
    let nanos = d.as_nanos();
    for (word, size) in SCALES {
        if nanos >= size {
            let count = u64::try_from(nanos / size).unwrap_or(u64::MAX);
            return plural(count, word);
        }
    }
    "0 seconds".to_owned()
}

/// Error returned by [`parse_duration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDurationError {
    input: String,
    kind: DurationErrorKind,
}

impl ParseDurationError {
    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &DurationErrorKind {
        &self.kind
    }

    /// The rejected input.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

/// The reason a duration failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurationErrorKind {
    /// The input was empty.
    Empty,
    /// The input started with a minus sign.
    Negative,
    /// A number was expected at byte `pos`.
    InvalidNumber {
        /// Byte offset.
        pos: usize,
    },
    /// A number other than a lone one was not followed by a unit.
    MissingUnit {
        /// Byte offset where the unit should start.
        pos: usize,
    },
    /// The unit is not one of `ns us ms s m h d w`.
    UnknownUnit(String),
    /// Units were not in decreasing order, or a unit repeated.
    UnitOrder {
        /// The offending unit.
        unit: &'static str,
        /// The unit that preceded it.
        previous: &'static str,
    },
    /// A character that cannot appear in a duration.
    UnexpectedChar {
        /// The character.
        ch: char,
        /// Byte offset.
        pos: usize,
    },
    /// The value does not fit in a `Duration`.
    Overflow,
}

impl fmt::Display for ParseDurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid duration {:?}: ", self.input)?;
        match &self.kind {
            DurationErrorKind::Empty => f.write_str("empty string"),
            DurationErrorKind::Negative => f.write_str("negative durations are not allowed"),
            DurationErrorKind::InvalidNumber { pos } => {
                write!(f, "expected a number at byte {pos}")
            }
            DurationErrorKind::MissingUnit { pos } => write!(
                f,
                "missing unit at byte {pos} (only a lone number may omit it)"
            ),
            DurationErrorKind::UnknownUnit(unit) => write!(
                f,
                "unknown unit {unit:?} (expected ns, us, ms, s, m, h, d or w)"
            ),
            DurationErrorKind::UnitOrder { unit, previous } => write!(
                f,
                "unit {unit:?} after {previous:?}; units must go from largest to smallest, each at most once"
            ),
            DurationErrorKind::UnexpectedChar { ch, pos } => {
                write!(f, "unexpected character {ch:?} at byte {pos}")
            }
            DurationErrorKind::Overflow => f.write_str("value is too large"),
        }
    }
}

impl std::error::Error for ParseDurationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::Rng;

    fn ok(s: &str) -> Duration {
        parse_duration(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn kind(s: &str) -> DurationErrorKind {
        match parse_duration(s) {
            Ok(d) => panic!("{s:?} unexpectedly parsed as {d:?}"),
            Err(e) => e.kind().clone(),
        }
    }

    #[test]
    fn parses_single_units() {
        assert_eq!(ok("15ns"), Duration::from_nanos(15));
        assert_eq!(ok("15us"), Duration::from_micros(15));
        assert_eq!(ok("15µs"), Duration::from_micros(15));
        assert_eq!(ok("15μs"), Duration::from_micros(15));
        assert_eq!(ok("250ms"), Duration::from_millis(250));
        assert_eq!(ok("15s"), Duration::from_secs(15));
        assert_eq!(ok("15m"), Duration::from_secs(900));
        assert_eq!(ok("2h"), Duration::from_secs(7_200));
        assert_eq!(ok("2d"), Duration::from_secs(172_800));
        assert_eq!(ok("1w"), Duration::from_secs(604_800));
    }

    #[test]
    fn parses_compound_durations() {
        assert_eq!(ok("1h30m15s"), Duration::from_secs(5_415));
        assert_eq!(ok("1d12h"), Duration::from_secs(129_600));
        assert_eq!(ok("1s500ms"), Duration::from_millis(1_500));
        assert_eq!(ok("1m1ms1ns"), Duration::new(60, 1_000_001));
    }

    #[test]
    fn bare_numbers_are_seconds() {
        assert_eq!(ok("90"), Duration::from_secs(90));
        assert_eq!(ok("0"), Duration::ZERO);
        assert_eq!(ok("1.25"), Duration::from_millis(1_250));
    }

    #[test]
    fn fractions_scale_by_their_unit() {
        assert_eq!(ok("1.5s"), Duration::from_millis(1_500));
        assert_eq!(ok("1.5h"), Duration::from_secs(5_400));
        assert_eq!(ok("0.5ms"), Duration::from_micros(500));
        assert_eq!(ok("2.000000001s"), Duration::new(2, 1));
        // Below a nanosecond is truncated, however many digits there are.
        assert_eq!(ok("1.00000000099999999999999s"), Duration::from_secs(1));
        assert_eq!(ok("0.1ns"), Duration::ZERO);
    }

    #[test]
    fn rejects_empty_and_negative() {
        assert_eq!(kind(""), DurationErrorKind::Empty);
        assert_eq!(kind("-5s"), DurationErrorKind::Negative);
        assert_eq!(
            kind("1h-5m"),
            DurationErrorKind::UnexpectedChar { ch: '-', pos: 2 }
        );
    }

    #[test]
    fn rejects_malformed_numbers() {
        assert_eq!(kind(".5s"), DurationErrorKind::InvalidNumber { pos: 0 });
        assert_eq!(kind("5.s"), DurationErrorKind::InvalidNumber { pos: 2 });
        assert_eq!(kind("s"), DurationErrorKind::InvalidNumber { pos: 0 });
        assert_eq!(kind("1..5s"), DurationErrorKind::InvalidNumber { pos: 2 });
        assert_eq!(kind("+5s"), DurationErrorKind::InvalidNumber { pos: 0 });
    }

    #[test]
    fn rejects_missing_and_unknown_units() {
        assert_eq!(kind("1h30"), DurationErrorKind::MissingUnit { pos: 4 });
        assert_eq!(kind("5x"), DurationErrorKind::UnknownUnit("x".into()));
        assert_eq!(kind("5sec"), DurationErrorKind::UnknownUnit("sec".into()));
        assert_eq!(kind("5S"), DurationErrorKind::UnknownUnit("S".into()));
        assert_eq!(kind("5mo"), DurationErrorKind::UnknownUnit("mo".into()));
    }

    #[test]
    fn rejects_whitespace() {
        assert_eq!(
            kind("1h 30m"),
            DurationErrorKind::UnexpectedChar { ch: ' ', pos: 2 }
        );
        assert_eq!(
            kind("5s "),
            DurationErrorKind::UnexpectedChar { ch: ' ', pos: 2 }
        );
        assert_eq!(kind(" 5s"), DurationErrorKind::InvalidNumber { pos: 0 });
    }

    #[test]
    fn rejects_out_of_order_or_repeated_units() {
        assert_eq!(
            kind("30m1h"),
            DurationErrorKind::UnitOrder {
                unit: "h",
                previous: "m"
            }
        );
        assert_eq!(
            kind("1s1s"),
            DurationErrorKind::UnitOrder {
                unit: "s",
                previous: "s"
            }
        );
        assert_eq!(
            kind("1ms1s"),
            DurationErrorKind::UnitOrder {
                unit: "s",
                previous: "ms"
            }
        );
    }

    #[test]
    fn rejects_values_that_overflow() {
        assert_eq!(
            kind("99999999999999999999999999999999999999999s"),
            DurationErrorKind::Overflow
        );
        assert_eq!(kind("30600000000000w"), DurationErrorKind::Overflow);
        assert_eq!(kind("18446744073709551616"), DurationErrorKind::Overflow);
        assert_eq!(ok("18446744073709551615"), Duration::from_secs(u64::MAX));
    }

    #[test]
    fn error_messages_quote_input_and_reason() {
        let e = parse_duration("5x").unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid duration \"5x\": unknown unit \"x\" (expected ns, us, ms, s, m, h, d or w)"
        );
        assert_eq!(e.input(), "5x");
        let e = parse_duration("30m1h").unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid duration \"30m1h\": unit \"h\" after \"m\"; units must go from largest to smallest, each at most once"
        );
    }

    #[test]
    fn formats_compactly() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration(Duration::from_secs(5_400)), "1h30m");
        assert_eq!(format_duration(Duration::from_secs(86_400 * 14)), "14d");
        assert_eq!(format_duration(Duration::from_millis(1_500)), "1s500ms");
        assert_eq!(format_duration(Duration::new(3_661, 1_001)), "1h1m1s1us1ns");
        assert_eq!(
            format_duration(Duration::MAX),
            "213503982334601d7h15s999ms999us999ns"
        );
    }

    #[test]
    fn format_parse_round_trip_is_exact() {
        let mut rng = Rng::seed_from_u64(7);
        let mut samples = vec![Duration::ZERO, Duration::MAX, Duration::from_nanos(1)];
        for _ in 0..2_000 {
            let secs = match rng.gen_range(0..4u32) {
                0 => rng.gen_range(0..60u64),
                1 => rng.gen_range(0..1_000_000u64),
                2 => rng.next_u64() >> 20,
                _ => rng.next_u64(),
            };
            let nanos = match rng.gen_range(0..3u32) {
                0 => 0,
                1 => rng.gen_range(0..1_000u32) * 1_000_000,
                _ => rng.gen_range(0..1_000_000_000u32),
            };
            samples.push(Duration::new(secs, nanos));
        }
        for d in samples {
            assert_eq!(parse_duration(&format_duration(d)), Ok(d), "{d:?}");
        }
    }

    #[test]
    fn humanize_uses_largest_whole_unit() {
        assert_eq!(humanize(Duration::ZERO), "0 seconds");
        assert_eq!(humanize(Duration::from_nanos(1)), "1 nanosecond");
        assert_eq!(humanize(Duration::from_micros(20)), "20 microseconds");
        assert_eq!(humanize(Duration::from_millis(450)), "450 milliseconds");
        assert_eq!(humanize(Duration::from_secs(1)), "1 second");
        assert_eq!(humanize(Duration::from_secs(59)), "59 seconds");
        assert_eq!(humanize(Duration::from_secs(185)), "3 minutes");
        assert_eq!(humanize(Duration::from_secs(7_199)), "1 hour");
        assert_eq!(humanize(Duration::from_secs(7_200)), "2 hours");
        assert_eq!(humanize(Duration::from_secs(86_400 * 45)), "45 days");
        assert_eq!(humanize(Duration::from_secs(86_400 * 800)), "2 years");
    }

    #[test]
    fn compact_duration_displays_inline() {
        let d = CompactDuration(Duration::from_secs(90));
        assert_eq!(format!("retry in {d}"), "retry in 1m30s");
    }
}
