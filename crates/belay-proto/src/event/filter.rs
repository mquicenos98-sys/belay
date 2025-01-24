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

/// Which events a subscriber wants.
///
/// ```
/// use belay_proto::event::EventFilter;
///
/// let pairs = vec![
///     ("from".to_owned(), "120".to_owned()),
///     ("types".to_owned(), "job.*,schedule.fired".to_owned()),
/// ];
/// let filter = EventFilter::from_query(&pairs).unwrap();
/// assert_eq!(filter.from, Some(120));
/// assert_eq!(filter.to_query(), pairs);
///
/// let bad = vec![("types".to_owned(), "job.succeded".to_owned())];
/// assert_eq!(
///     EventFilter::from_query(&bad).unwrap_err().to_string(),
///     r#"$.types: unknown event type "job.succeded", did you mean "job.succeeded"?"#
/// );
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventFilter {
    /// First sequence number to deliver (inclusive). A client resuming
    /// after `Last-Event-ID: n` asks for `from = n + 1`.
    pub from: Option<u64>,
    /// Types to deliver; empty for all.
    pub types: Vec<EventPattern>,
    /// Only events about this queue. Events that concern no queue
    /// (schedules, workflows, workers) are then excluded.
    pub queue: Option<QueueName>,
}

impl EventFilter {
    /// Query parameters understood by [`from_query`](Self::from_query).
    pub const QUERY_KEYS: &'static [&'static str] = &["from", "types", "queue"];

    /// Everything, from the oldest retained event.
    #[must_use]
    pub fn new() -> EventFilter {
        EventFilter::default()
    }

    /// Starts at sequence number `seq`.
    #[must_use]
    pub fn from_seq(mut self, seq: u64) -> EventFilter {
        self.from = Some(seq);
        self
    }

    /// Adds a type pattern.
    #[must_use]
    pub fn with_type(mut self, pattern: EventPattern) -> EventFilter {
        self.types.push(pattern);
        self
    }

    /// Restricts to one queue.
    #[must_use]
    pub fn for_queue(mut self, queue: QueueName) -> EventFilter {
        self.queue = Some(queue);
        self
    }

    /// Whether `kind` passes the type and queue criteria.
    #[must_use]
    pub fn matches_kind(&self, kind: &EventKind) -> bool {
        (self.types.is_empty() || self.types.iter().any(|p| p.matches(kind)))
            && self
                .queue
                .as_ref()
                .map_or(true, |q| kind.queue() == Some(q))
    }

    /// Whether `event` should be delivered.
    #[must_use]
    pub fn matches(&self, event: &Event) -> bool {
        self.from.map_or(true, |from| event.seq > from) && self.matches_kind(&event.kind)
    }

    /// The decoded query pairs for this filter; empty criteria are left
    /// out.
    #[must_use]
    pub fn to_query(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        if let Some(from) = self.from {
            pairs.push(("from".to_owned(), from.to_string()));
        }
        if !self.types.is_empty() {
            let types: Vec<String> = self.types.iter().map(ToString::to_string).collect();
            pairs.push(("types".to_owned(), types.join(",")));
        }
        if let Some(queue) = &self.queue {
            pairs.push(("queue".to_owned(), queue.to_string()));
        }
        pairs
    }

    /// Parses decoded query pairs, strictly (see [`JobFilter::from_query`]
    /// for the rules).
    ///
    /// [`JobFilter::from_query`]: crate::api::JobFilter::from_query
    pub fn from_query(pairs: &[(String, String)]) -> Result<EventFilter, Violations> {
        EventFilter::from_pairs(borrow_pairs(pairs))
    }

    /// Like [`from_query`](Self::from_query) for borrowed pairs, such as an
    /// HTTP server's query map yields.
    pub fn from_pairs<'a>(
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<EventFilter, Violations> {
        let mut q = QueryReader::new(pairs, EventFilter::QUERY_KEYS);
        let filter = EventFilter {
            from: q.number("from"),
            types: q.list("types"),
            queue: q.text("queue"),
        };
        q.finish()?;
        Ok(filter)
    }
}

#[cfg(test)]
mod tests {
    use super::super::samples;
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn patterns_parse_display_and_match() {
        let kinds = samples::all();
        let succeeded = &kinds[3];
        for (text, expected) in [
            ("*", true),
            ("job.*", true),
            ("job.succeeded", true),
            ("queue.*", false),
            ("job.dead", false),
        ] {
            let p: EventPattern = text.parse().unwrap();
            assert_eq!(p.to_string(), text);
            assert_eq!(p.matches(succeeded), expected, "{text}");
        }
        let every_type: Vec<EventPattern> = EventKind::TYPES
            .iter()
            .map(|t| t.parse().unwrap())
            .collect();
        for kind in &kinds {
            assert_eq!(every_type.iter().filter(|p| p.matches(kind)).count(), 1);
        }
    }

    #[test]
    fn unknown_patterns_are_errors_with_hints() {
        assert_eq!(
            "jobs.*".parse::<EventPattern>().unwrap_err().to_string(),
            r#"unknown event category "jobs", did you mean "job"?"#
        );
        assert_eq!(
            "schedule.fire"
                .parse::<EventPattern>()
                .unwrap_err()
                .to_string(),
            r#"unknown event type "schedule.fire", did you mean "schedule.fired"?"#
        );
        assert!("job".parse::<EventPattern>().is_err());
        assert!(
            EventPattern::parse_list("job.*, ,worker.lost")
                .unwrap()
                .len()
                == 2
        );
    }

    #[test]
    fn query_round_trips_and_accepts_repeats() {
        let filter = EventFilter::new()
            .from_seq(5)
            .with_type(EventPattern::All)
            .for_queue(QueueName::from_static("q"));
        let q = filter.to_query();
        assert_eq!(q, pairs(&[("from", "5"), ("types", "*"), ("queue", "q")]));
        assert_eq!(EventFilter::from_query(&q), Ok(filter));
        let repeated =
            EventFilter::from_query(&pairs(&[("types", "job.*"), ("types", "worker.lost")]))
                .unwrap();
        assert_eq!(repeated.types.len(), 2);
        assert_eq!(EventFilter::from_query(&[]), Ok(EventFilter::new()));
    }

    #[test]
    fn query_errors_name_the_parameter() {
        let err =
            EventFilter::from_query(&pairs(&[("from", "x"), ("queue", "Q"), ("type", "job.*")]))
                .unwrap_err();
        let lines: Vec<String> = err.iter().map(ToString::to_string).collect();
        assert_eq!(
            lines,
            [
                r#"$: unknown query parameter "type", did you mean "types"?"#,
                r#"$.from: invalid number "x": invalid digit found in string"#,
                r#"$.queue: invalid queue name "Q": must start with a lowercase letter or digit, got 'Q'"#,
            ]
        );
    }
}
