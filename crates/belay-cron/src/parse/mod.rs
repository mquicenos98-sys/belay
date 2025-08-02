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
