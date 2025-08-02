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
