//! The crate's error type.
//!
//! One enum covers expression, time zone and date-time parsing so callers
//! validating a schedule (`cron` + `timezone`) handle a single type. Every
//! message is lowercase and names the offending field or input; errors that
//! come from a specific character also carry its 1-based column so a CLI can
//! draw a caret under it.

use std::fmt;

/// Result alias used throughout the crate.
pub type Result<T, E = CronError> = std::result::Result<T, E>;

/// One of the (up to) seven fields of a cron expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    /// Seconds, 0-59 (6- and 7-field forms only).
    Second,
    /// Minutes, 0-59.
    Minute,
    /// Hours, 0-23.
    Hour,
    /// Day of the month, 1-31.
    DayOfMonth,
    /// Month, 1-12 or `JAN`-`DEC`.
    Month,
    /// Day of the week, 0-7 (0 and 7 are Sunday) or `SUN`-`SAT`.
    DayOfWeek,
    /// Year, 1970-2199 (7-field form only).
    Year,
}

impl Field {
    /// The lowercase name used in error messages and descriptions.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Field::Second => "second",
            Field::Minute => "minute",
            Field::Hour => "hour",
            Field::DayOfMonth => "day-of-month",
            Field::Month => "month",
            Field::DayOfWeek => "day-of-week",
            Field::Year => "year",
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What went wrong inside a single field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldProblem {
    /// A number outside the field's range.
    OutOfRange {
        /// The value as written.
        value: u32,
        /// Smallest accepted value.
        min: u32,
        /// Largest accepted value.
        max: u32,
    },
    /// A number with too many digits to be meaningful anywhere.
    NumberTooLarge,
    /// A character that cannot start or continue an item here.
    UnexpectedChar(char),
    /// The field ended in the middle of an item (`5-`, `*/`).
    UnexpectedEnd,
    /// Two commas in a row, or a leading/trailing comma.
    EmptyItem,
    /// A name that is not a month or weekday abbreviation valid here.
    UnknownName(String),
    /// A step of zero or one larger than the field's span.
    InvalidStep {
        /// The step as written.
        step: u32,
        /// Largest accepted step.
        max: u32,
    },
    /// Syntax that exists but is not allowed in this field (`L` in hours).
    NotAllowed(&'static str),
    /// A year range whose end precedes its start (other fields wrap around).
    ReversedRange {
        /// Range start.
        start: u32,
        /// Range end.
        end: u32,
    },
    /// `#n` with `n` outside 1-5.
    InvalidOccurrence(u32),
}

/// Errors produced by this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronError {
    /// The expression was empty or only whitespace.
    Empty,
    /// The expression had the wrong number of whitespace-separated fields.
    FieldCount {
        /// Fields found.
        found: usize,
        /// Human-readable list of accepted counts, e.g. `"5, 6 or 7"`.
        expected: &'static str,
    },
    /// An `@name` that is not a supported nickname.
    UnknownNickname {
        /// The nickname as written, including the `@`.
        name: String,
    },
    /// A problem inside one field of the expression.
    Field {
        /// The field that failed.
        field: Field,
        /// 1-based character column in the whole expression.
        column: usize,
        /// What was wrong.
        problem: FieldProblem,
    },
    /// A time zone string that is neither a known name, a fixed offset nor a
    /// valid POSIX TZ rule.
    InvalidTz {
        /// The input as given.
        input: String,
        /// 1-based character column of the problem.
        column: usize,
        /// Short description.
        reason: &'static str,
    },
    /// A time zone name that is not in the embedded table.
    UnknownZone {
        /// The name as given.
        name: String,
        /// A close match from the table, if any.
        suggestion: Option<&'static str>,
    },
    /// A local date-time string that is malformed or names an impossible date.
    InvalidDateTime {
        /// The input as given.
        input: String,
        /// Short description.
        reason: &'static str,
    },
}

impl CronError {
    /// The 1-based column the error points at, when it points at one.
    #[must_use]
    pub fn column(&self) -> Option<usize> {
        match self {
            CronError::Field { column, .. } | CronError::InvalidTz { column, .. } => Some(*column),
            _ => None,
        }
    }

    pub(crate) fn field(field: Field, column: usize, problem: FieldProblem) -> Self {
        CronError::Field {
            field,
            column,
            problem,
        }
    }
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronError::Empty => f.write_str("empty cron expression"),
            CronError::FieldCount { found, expected } => {
                write!(f, "expected {expected} fields, found {found}")
            }
            CronError::UnknownNickname { name } => write!(
                f,
                "unknown nickname \"{name}\" (expected @yearly, @annually, @monthly, \
                 @weekly, @daily, @midnight or @hourly)"
            ),
            CronError::Field {
                field,
                column,
                problem,
            } => {
                match problem {
                    FieldProblem::OutOfRange { value, min, max } => {
                        write!(f, "invalid value {value} in {field} field ({min}-{max})")?
                    }
                    FieldProblem::NumberTooLarge => write!(f, "number too large in {field} field")?,
                    FieldProblem::UnexpectedChar(c) => {
                        write!(f, "unexpected character '{c}' in {field} field")?
                    }
                    FieldProblem::UnexpectedEnd => write!(f, "unexpected end of {field} field")?,
                    FieldProblem::EmptyItem => write!(f, "empty list item in {field} field")?,
                    FieldProblem::UnknownName(name) => {
                        write!(f, "unknown name \"{name}\" in {field} field")?
                    }
                    FieldProblem::InvalidStep { step, max } => {
                        write!(f, "invalid step {step} in {field} field (1-{max})")?
                    }
                    FieldProblem::NotAllowed(what) => {
                        write!(f, "{what} is not allowed in the {field} field")?
                    }
                    FieldProblem::ReversedRange { start, end } => {
                        write!(f, "reversed range {start}-{end} in {field} field")?
                    }
                    FieldProblem::InvalidOccurrence(n) => {
                        write!(f, "invalid occurrence #{n} in {field} field (1-5)")?
                    }
                }
                write!(f, " at column {column}")
            }
            CronError::InvalidTz {
                input,
                column,
                reason,
            } => write!(
                f,
                "invalid time zone \"{input}\" at column {column}: {reason}"
            ),
            CronError::UnknownZone { name, suggestion } => {
                write!(f, "unknown time zone \"{name}\"")?;
                if let Some(s) = suggestion {
                    write!(f, " (did you mean \"{s}\"?)")?;
                }
                Ok(())
            }
            CronError::InvalidDateTime { input, reason } => {
                write!(f, "invalid local date-time \"{input}\": {reason}")
            }
        }
    }
}
