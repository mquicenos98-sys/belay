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

impl fmt::Display for EventPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventPattern::All => f.write_str("*"),
            EventPattern::Category(c) => write!(f, "{c}.*"),
            EventPattern::Type(t) => f.write_str(t),
        }
    }
}

impl FromStr for EventPattern {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<EventPattern, ParseError> {
        if s == "*" {
            return Ok(EventPattern::All);
        }
        if let Some(category) = s.strip_suffix(".*") {
            return CATEGORIES
                .iter()
                .find(|c| **c == category)
                .map(|c| EventPattern::Category(c))
                .ok_or_else(|| ParseError::unknown("event category", category, CATEGORIES));
        }
        EventKind::TYPES
            .iter()
            .find(|t| **t == s)
            .map(|t| EventPattern::Type(t))
            .ok_or_else(|| ParseError::unknown("event type", s, EventKind::TYPES))
    }
}

text_json!(EventPattern, "event pattern");
