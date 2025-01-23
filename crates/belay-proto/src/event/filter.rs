//! Choosing events: type patterns (`job.*`, `schedule.fired`) and the
//! stream filter behind `GET /v1/events?from=&types=&queue=`.

use std::fmt;
use std::str::FromStr;

use super::{Event, EventKind, CATEGORIES};
use crate::error::ParseError;
use crate::ids::QueueName;
use crate::query::{borrow_pairs, QueryReader};
use crate::validate::Violations;
use crate::wire::text_json;

/// Matches event types: every type (`*`), a category (`job.*`) or one
/// type (`job.succeeded`). Only existing categories and types parse, so a
/// typo is an error rather than a filter that never matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventPattern {
    /// `*`
    All,
    /// `<category>.*`
    Category(&'static str),
    /// An exact type name.
    Type(&'static str),
}

impl EventPattern {
    /// Whether `kind` matches.
    #[must_use]
    pub fn matches(&self, kind: &EventKind) -> bool {
        match self {
            EventPattern::All => true,
            EventPattern::Category(c) => kind.category() == *c,
            EventPattern::Type(t) => kind.type_name() == *t,
        }
    }

    /// Parses a comma-separated list such as `job.*,schedule.fired`.
    pub fn parse_list(s: &str) -> Result<Vec<EventPattern>, ParseError> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::parse)
            .collect()
    }
}
