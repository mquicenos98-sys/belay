//! Expression text to [`CronExpr`]: nicknames, field splitting and layout,
//! then per-field lexing and parsing.

mod field;
mod lexer;

pub(crate) use field::DAY_NAMES;

use field::{parse_field, Domain, ParsedField, Special};

use crate::error::{CronError, Field, Result};
use crate::expr::{CronExpr, DomSpec, DowSpec, Fields, YearSet};

/// Which cron flavour to accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    /// Vixie cron and cronie: five fields, optionally preceded by seconds
    /// (six) and followed by a year (seven). Day-of-week runs 0-7 with both
    /// 0 and 7 meaning Sunday.
    #[default]
    Vixie,
    /// Quartz: seconds are required (six or seven fields) and day-of-week
    /// runs 1-7 with 1 meaning Sunday.
    Quartz,
}

/// How the day-of-month and day-of-week fields combine when both are
/// restricted (neither is `*` or `?`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DayMatch {
    /// A day matches when either field matches, as in Vixie cron:
    /// `0 0 13 * FRI` fires on every 13th and on every Friday.
    #[default]
    Either,
    /// A day matches only when both fields match: `0 0 13 * FRI` fires on
    /// Friday the 13th. This is Quartz's behaviour.
    Both,
}

/// Options for [`CronExpr::parse_with`].
///
/// ```
/// use belay_cron::{CronExpr, DayMatch, Dialect, ParseOptions};
///
/// let opts = ParseOptions::new().with_day_match(DayMatch::Both);
/// let friday_13th = CronExpr::parse_with("0 0 13 * FRI", opts)?;
/// assert_eq!(friday_13th.options().day_match(), DayMatch::Both);
///
/// let quartz = CronExpr::parse_with("0 15 10 ? * 2-6", ParseOptions::quartz())?;
/// assert_eq!(quartz.options().dialect(), Dialect::Quartz);
/// # Ok::<(), belay_cron::CronError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ParseOptions {
    dialect: Dialect,
    day_match: DayMatch,
}

impl ParseOptions {
    /// Vixie dialect with [`DayMatch::Either`], the same as `default()`.
    #[must_use]
    pub const fn new() -> Self {
        ParseOptions {
            dialect: Dialect::Vixie,
            day_match: DayMatch::Either,
        }
    }

    /// Quartz dialect with [`DayMatch::Both`].
    #[must_use]
    pub const fn quartz() -> Self {
        ParseOptions {
            dialect: Dialect::Quartz,
            day_match: DayMatch::Both,
        }
    }

    /// Replaces the dialect, keeping the day-match rule.
    #[must_use]
    pub const fn with_dialect(self, dialect: Dialect) -> Self {
        ParseOptions { dialect, ..self }
    }

    /// Replaces the day-match rule.
    #[must_use]
    pub const fn with_day_match(self, day_match: DayMatch) -> Self {
        ParseOptions { day_match, ..self }
    }

    /// The dialect.
    #[must_use]
    pub const fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The day-match rule.
    #[must_use]
    pub const fn day_match(&self) -> DayMatch {
        self.day_match
    }
}

/// Nicknames and their five-field expansions.
const NICKNAMES: [(&str, &str); 7] = [
    ("@yearly", "0 0 1 1 *"),
    ("@annually", "0 0 1 1 *"),
    ("@monthly", "0 0 1 * *"),
    ("@weekly", "0 0 * * 0"),
    ("@daily", "0 0 * * *"),
    ("@midnight", "0 0 * * *"),
    ("@hourly", "0 * * * *"),
];

const STANDARD_LAYOUT: [Field; 7] = [
    Field::Second,
    Field::Minute,
    Field::Hour,
    Field::DayOfMonth,
    Field::Month,
    Field::DayOfWeek,
    Field::Year,
];

pub(crate) fn parse(input: &str, options: ParseOptions) -> Result<CronExpr> {
    let fields = split_fields(input);
    let Some(&(first, _)) = fields.first() else {
        return Err(CronError::Empty);
    };
    if first.starts_with('@') {
        let name = input.trim();
        let expansion = NICKNAMES
            .iter()
            .find(|(nick, _)| nick.eq_ignore_ascii_case(name))
            .map(|(_, expansion)| *expansion)
            .ok_or_else(|| CronError::UnknownNickname {
                name: name.to_string(),
            })?;
        return match options.dialect {
            Dialect::Vixie => parse(expansion, options),
            Dialect::Quartz => parse(&format!("0 {expansion}"), options),
        };
    }
    let layout: &[Field] = match (fields.len(), options.dialect) {
        (5, Dialect::Vixie) => &STANDARD_LAYOUT[1..6],
        (6, _) => &STANDARD_LAYOUT[..6],
        (7, _) => &STANDARD_LAYOUT,
        (found, Dialect::Vixie) => {
            return Err(CronError::FieldCount {
                found,
                expected: "5, 6 or 7",
            })
        }
        (found, Dialect::Quartz) => {
            return Err(CronError::FieldCount {
                found,
                expected: "6 or 7",
            })
        }
    };
    let mut out = Fields::default();
    for (&(text, column), &field) in fields.iter().zip(layout) {
        let domain = domain(field, options.dialect);
        let tokens = lexer::tokenize(text, field, column)?;
        let parsed = parse_field(&tokens, &domain, column + text.chars().count())?;
        apply(&mut out, field, parsed);
    }
    Ok(CronExpr::from_fields(out, options))
}

/// Whitespace-separated fields with their 1-based starting columns.
fn split_fields(input: &str) -> Vec<(&str, usize)> {
    let mut fields = Vec::with_capacity(7);
    let mut start: Option<(usize, usize)> = None;
    let mut column = 0;
    for (byte, c) in input.char_indices() {
        column += 1;
        match (c.is_whitespace(), start) {
            (true, Some((s, col))) => {
                fields.push((&input[s..byte], col));
                start = None;
            }
            (false, None) => start = Some((byte, column)),
            _ => {}
        }
    }
    if let Some((s, col)) = start {
        fields.push((&input[s..], col));
    }
    fields
}

fn domain(field: Field, dialect: Dialect) -> Domain {
    match field {
        Field::Second => Domain::SECOND,
        Field::Minute => Domain::MINUTE,
        Field::Hour => Domain::HOUR,
        Field::DayOfMonth => Domain::DAY_OF_MONTH,
        Field::Month => Domain::MONTH,
        Field::DayOfWeek => match dialect {
            Dialect::Vixie => Domain::DAY_OF_WEEK,
            Dialect::Quartz => Domain::DAY_OF_WEEK_QUARTZ,
        },
        Field::Year => Domain::YEAR,
    }
}

/// Folds a parsed field into the bit sets.
fn apply(out: &mut Fields, field: Field, parsed: ParsedField) {
    // Years are stored in their own set; every other field fits in 64 bits.
    let mask: u64 = if field == Field::Year {
        0
    } else {
        parsed.values.iter().fold(0, |m, &v| m | 1 << v)
    };
    match field {
        Field::Second => out.seconds = mask,
        Field::Minute => out.minutes = mask,
        Field::Hour => out.hours = mask as u32,
        Field::Month => out.months = mask as u16,
        Field::Year => {
            let mut years = YearSet::EMPTY;
            for &y in &parsed.values {
                years.insert(y);
            }
            out.years = years;
        }
        Field::DayOfMonth => {
            let mut dom = DomSpec {
                days: mask as u32,
                last: false,
                before_last: 0,
                nearest: 0,
                last_weekday: false,
            };
            for special in parsed.specials {
                match special {
                    Special::LastDay => dom.last = true,
                    Special::LastDayOffset(n) => dom.before_last |= 1 << n,
                    Special::NearestWeekday(n) => dom.nearest |= 1 << n,
                    Special::LastWeekday => dom.last_weekday = true,
                    Special::LastOfWeekday(_) | Special::NthWeekday(..) => {}
                }
            }
            out.dom = dom;
        }
        Field::DayOfWeek => {
            let mut dow = DowSpec {
                days: mask as u8,
                last: 0,
                nth: [0; 7],
            };
            for special in parsed.specials {
                match special {
                    Special::LastOfWeekday(d) => dow.last |= 1 << d,
                    Special::NthWeekday(d, n) => dow.nth[d as usize] |= 1 << n,
                    _ => {}
                }
            }
            out.dow = dow;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_fields_and_tracks_columns() {
        assert_eq!(
            split_fields("  */5  1 * *\tMON "),
            vec![("*/5", 3), ("1", 8), ("*", 10), ("*", 12), ("MON", 14)]
        );
        assert!(split_fields(" \t ").is_empty());
    }

    #[test]
    fn five_six_and_seven_field_layouts() {
        let five = parse("30 9 * * *", ParseOptions::new()).unwrap();
        assert_eq!(five.seconds, 1);
        let six = parse("15 30 9 * * *", ParseOptions::new()).unwrap();
        assert_eq!(six.seconds, 1 << 15);
        assert_eq!(six.minutes, 1 << 30);
        let seven = parse("0 30 9 * * * 2030", ParseOptions::new()).unwrap();
        assert!(seven.years.contains(2030));
        assert!(!seven.years.contains(2031));
    }

    #[test]
    fn field_count_errors_depend_on_dialect() {
        assert_eq!(
            parse("* * * *", ParseOptions::new())
                .unwrap_err()
                .to_string(),
            "expected 5, 6 or 7 fields, found 4"
        );
        assert_eq!(
            parse("* * * * *", ParseOptions::quartz())
                .unwrap_err()
                .to_string(),
            "expected 6 or 7 fields, found 5"
        );
        assert_eq!(
            parse("* * * * * * * *", ParseOptions::new()).unwrap_err(),
            CronError::FieldCount {
                found: 8,
                expected: "5, 6 or 7"
            }
        );
        assert_eq!(
            parse("   ", ParseOptions::new()).unwrap_err(),
            CronError::Empty
        );
    }

    #[test]
    fn columns_in_errors_count_from_the_expression_start() {
        let err = parse("0 9 * * MON-FRY", ParseOptions::new()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown name \"FRY\" in day-of-week field at column 13"
        );
        let err = parse("0 0 25 * * *", ParseOptions::new()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid value 25 in hour field (0-23) at column 5"
        );
    }

    #[test]
    fn options_builders_compose() {
        let o = ParseOptions::quartz().with_day_match(DayMatch::Either);
        assert_eq!(o.dialect(), Dialect::Quartz);
        assert_eq!(o.day_match(), DayMatch::Either);
        assert_eq!(ParseOptions::default(), ParseOptions::new());
        assert_eq!(
            ParseOptions::new()
                .with_dialect(Dialect::Quartz)
                .day_match(),
            DayMatch::Either
        );
    }
}
