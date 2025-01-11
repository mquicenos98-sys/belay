//! Strict reading of decoded query-string pairs, shared by the list and
//! stream filters so that every endpoint treats its parameters alike:
//!
//! - unknown parameters are rejected with a "did you mean" hint, because a
//!   silently ignored typo (`stat=dead`) would widen a query instead of
//!   failing it;
//! - single-valued parameters may appear once; an empty value counts as
//!   absent (what an HTML form sends for a blank field);
//! - list parameters accept repeats and commas alike: `state=a,b` equals
//!   `state=a&state=b`.
//!
//! Problems are reported with the parameter name as the path (`$.limit`).

use std::fmt;
use std::str::FromStr;

use belay_common::Timestamp;
use belay_json::FromJsonError;

use crate::error::{quote, ParseError};
use crate::ids::NameRule;
use crate::validate::{Validator, Violations};
use crate::wire::parse_timestamp;

pub(crate) struct QueryReader<'a> {
    pairs: Vec<(&'a str, &'a str)>,
    v: Validator,
}

impl<'a> QueryReader<'a> {
    /// Takes decoded pairs, rejecting keys not in `known`.
    pub(crate) fn new(
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
        known: &[&str],
    ) -> QueryReader<'a> {
        let pairs: Vec<(&str, &str)> = pairs.into_iter().collect();
        let mut v = Validator::new();
        let mut reported: Vec<&str> = Vec::new();
        for (key, _) in &pairs {
            if !known.contains(key) && !reported.contains(key) {
                reported.push(key);
                let e = FromJsonError::unknown_variant(key, "query parameter", known);
                v.report_here(e.message());
            }
        }
        QueryReader { pairs, v }
    }

    fn values(&self, key: &str) -> Vec<&'a str> {
        self.pairs
            .iter()
            .filter(|(k, _)| *k == key)
            .map(|(_, value)| *value)
            .collect()
    }

    /// The raw value of a single-valued parameter.
    fn raw(&mut self, key: &str) -> Option<&'a str> {
        let values = self.values(key);
        if values.len() > 1 {
            self.v.report(key, "may be given only once");
            return None;
        }
        values.first().copied().filter(|s| !s.is_empty())
    }

    /// A single value parsed by one of this crate's text types.
    pub(crate) fn text<T: FromStr<Err = ParseError>>(&mut self, key: &str) -> Option<T> {
        let raw = self.raw(key)?;
        match raw.parse() {
            Ok(value) => Some(value),
            Err(e) => {
                self.v.report(key, e);
                None
            }
        }
    }

    /// A single non-negative whole number.
    pub(crate) fn number<T>(&mut self, key: &str) -> Option<T>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        let raw = self.raw(key)?;
        match raw.parse() {
            Ok(value) => Some(value),
            Err(e) => {
                self.v
                    .report(key, format!("invalid number {}: {e}", quote(raw)));
                None
            }
        }
    }

    /// A single RFC 3339 timestamp.
    pub(crate) fn timestamp(&mut self, key: &str) -> Option<Timestamp> {
        let raw = self.raw(key)?;
        match parse_timestamp(raw) {
            Ok(ts) => Some(ts),
            Err(message) => {
                self.v.report(key, message);
                None
            }
        }
    }

    /// A single free-text value that must satisfy `rule`.
    pub(crate) fn name(&mut self, key: &str, rule: NameRule) -> Option<String> {
        let raw = self.raw(key)?;
        match rule.problem(raw) {
            None => Some(raw.to_owned()),
            Some(problem) => {
                self.v.report(key, problem);
                None
            }
        }
    }

    /// A list of values, from repeats and/or commas; blanks are dropped.
    pub(crate) fn list<T: FromStr<Err = ParseError>>(&mut self, key: &str) -> Vec<T> {
        let mut out = Vec::new();
        for raw in self.values(key) {
            for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match item.parse() {
                    Ok(value) => out.push(value),
                    Err(e) => self.v.report(key, e),
                }
            }
        }
        out
    }

    /// Records a problem found while combining parameters.
    pub(crate) fn report(&mut self, key: &str, message: impl fmt::Display) {
        self.v.report(key, message);
    }

    pub(crate) fn finish(self) -> Result<(), Violations> {
        self.v.finish()
    }
}
