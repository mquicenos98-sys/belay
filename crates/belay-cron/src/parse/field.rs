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
