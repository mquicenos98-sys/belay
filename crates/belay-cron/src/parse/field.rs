//! Parses the tokens of one field into plain values and special items.
//!
//! ```text
//! field   := item (',' item)*
//! item    := ('*' | '?') ['/' step]
//!          | value ['-' value] ['/' step]
//!          | 'L' ['-' n] | 'LW' | n 'W'              (day-of-month only)
//!          | value 'L' | NAME 'L' | value '#' n      (day-of-week only)
//! ```
//!
//! A range whose end is below its start wraps around the field (`FRI-MON`,
//! `22-2`, `NOV-FEB`), except in the year field where it is an error. `a/n`
//! without an end runs to the top of the field, as in Quartz.

use super::lexer::{Token, TokenKind};
use crate::error::{CronError, Field, FieldProblem, Result};

pub(crate) const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

pub(crate) const DAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// The values a field accepts and how they map onto the canonical numbering
/// the rest of the crate uses (weekdays 0-6 from Sunday).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Domain {
    pub field: Field,
    /// Smallest value that may be written.
    pub min: u32,
    /// Largest value that may be written.
    pub max: u32,
    /// Top of the range that `*`, open steps and wrapping cover. It differs
    /// from `max` only for the standard day-of-week field, where 7 is an alias
    /// of Sunday and must not be visited twice.
    pub cycle_max: u32,
    pub names: &'static [&'static str],
    /// Value of `names[0]`.
    pub name_base: u32,
}

impl Domain {
    pub(crate) const SECOND: Domain = Domain::numeric(Field::Second, 0, 59);
    pub(crate) const MINUTE: Domain = Domain::numeric(Field::Minute, 0, 59);
    pub(crate) const HOUR: Domain = Domain::numeric(Field::Hour, 0, 23);
    pub(crate) const DAY_OF_MONTH: Domain = Domain::numeric(Field::DayOfMonth, 1, 31);
    pub(crate) const YEAR: Domain = Domain::numeric(Field::Year, crate::MIN_YEAR, crate::MAX_YEAR);
    pub(crate) const MONTH: Domain = Domain {
        field: Field::Month,
        min: 1,
        max: 12,
        cycle_max: 12,
        names: &MONTH_NAMES,
        name_base: 1,
    };
    /// Vixie numbering: 0-7, both 0 and 7 are Sunday.
    pub(crate) const DAY_OF_WEEK: Domain = Domain {
        field: Field::DayOfWeek,
        min: 0,
        max: 7,
        cycle_max: 6,
        names: &DAY_NAMES,
        name_base: 0,
    };
    /// Quartz numbering: 1-7, 1 is Sunday.
    pub(crate) const DAY_OF_WEEK_QUARTZ: Domain = Domain {
        field: Field::DayOfWeek,
        min: 1,
        max: 7,
        cycle_max: 7,
        names: &DAY_NAMES,
        name_base: 1,
    };

    const fn numeric(field: Field, min: u32, max: u32) -> Domain {
        Domain {
            field,
            min,
            max,
            cycle_max: max,
            names: &[],
            name_base: 0,
        }
    }

    /// Maps a written value onto the canonical numbering.
    fn canonical(&self, v: u32) -> u32 {
        if self.field == Field::DayOfWeek {
            (v - self.min) % 7
        } else {
            v
        }
    }

    fn span(&self) -> u32 {
        self.cycle_max - self.min + 1
    }
}

/// A day-of-month or day-of-week item that is not a plain value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Special {
    /// `L`: the last day of the month.
    LastDay,
    /// `L-n`: `n` days before the last day.
    LastDayOffset(u32),
    /// `nW`: the weekday (Mon-Fri) nearest day `n`, within the month.
    NearestWeekday(u32),
    /// `LW`: the last weekday (Mon-Fri) of the month.
    LastWeekday,
    /// `dL`: the last weekday `d` (0-6) of the month.
    LastOfWeekday(u32),
    /// `d#n`: the `n`th weekday `d` (0-6) of the month.
    NthWeekday(u32, u32),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedField {
    /// Plain values in canonical numbering, possibly repeated.
    pub values: Vec<u32>,
    pub specials: Vec<Special>,
}

/// Parses a non-empty field. `end_column` is the column just past its text.
pub(crate) fn parse_field(
    tokens: &[Token],
    domain: &Domain,
    end_column: usize,
) -> Result<ParsedField> {
    FieldParser {
        tokens,
        pos: 0,
        domain,
        end_column,
        out: ParsedField::default(),
    }
    .run()
}
