//! Decoded query strings as an ordered multi-map.

use std::fmt;
use std::str::FromStr;

use crate::percent::{decode_form, encode, EncodeSet};

/// The decoded `key=value` pairs of a query string, in order, duplicates
/// kept.
///
/// Parsing is deliberately lenient (malformed escapes are kept literally,
/// pieces without `=` get an empty value): a query string is client input,
/// and rejecting it wholesale would hide which parameter was wrong. Typed
/// access through [`Query::parse_value`] reports errors per parameter.
///
/// ```
/// use belay_http::Query;
///
/// let query = Query::parse("queue=emails&state=ready&state=running&limit=50");
/// assert_eq!(query.get("queue"), Some("emails"));
/// assert_eq!(query.get_all("state").collect::<Vec<_>>(), ["ready", "running"]);
/// assert_eq!(query.parse_value::<u32>("limit").unwrap(), Some(50));
/// assert!(query.parse_value::<u32>("queue").is_err());
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// An empty query.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses a raw query string; a leading `?` is ignored.
    pub fn parse(raw: &str) -> Query {
        let raw = raw.strip_prefix('?').unwrap_or(raw);
        let pairs = raw
            .split('&')
            .filter(|piece| !piece.is_empty())
            .map(|piece| {
                let (key, value) = piece.split_once('=').unwrap_or((piece, ""));
                (decode_form(key), decode_form(value))
            })
            .collect();
        Query { pairs }
    }

    /// The first value for `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every value for `key`, in order.
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Values for `key` treated as a list: repeated keys and comma-separated
    /// values are both accepted (`types=a,b&types=c`), empties dropped.
    pub fn list<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.get_all(key)
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    /// Whether `key` is present (possibly with an empty value).
    pub fn contains(&self, key: &str) -> bool {
        self.pairs.iter().any(|(k, _)| k == key)
    }

    /// Parses the first value for `key`; `Ok(None)` when it is absent or
    /// empty.
    pub fn parse_value<T>(&self, key: &str) -> Result<Option<T>, QueryError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        match self.get(key) {
            None | Some("") => Ok(None),
            Some(raw) => raw.parse().map(Some).map_err(|e: T::Err| QueryError {
                key: key.to_owned(),
                kind: QueryErrorKind::Invalid {
                    value: raw.to_owned(),
                    reason: e.to_string(),
                },
            }),
        }
    }

    /// Parses the first value for `key`, which must be present.
    pub fn require<T>(&self, key: &str) -> Result<T, QueryError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        self.parse_value(key)?.ok_or_else(|| QueryError {
            key: key.to_owned(),
            kind: QueryErrorKind::Missing,
        })
    }

    /// Adds a pair, keeping existing values for `key`.
    pub fn append(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.pairs.push((key.into(), value.into()));
    }

    /// Sets `key` to a single value.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        self.pairs.retain(|(k, _)| *k != key);
        self.pairs.push((key, value.into()));
    }

    /// Removes every pair for `key`.
    pub fn remove(&mut self, key: &str) {
        self.pairs.retain(|(k, _)| k != key);
    }

    /// Iterates over decoded `(key, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Number of pairs.
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether there are no pairs.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Encodes the pairs back into a query string (without the leading `?`).
    pub fn to_query_string(&self) -> String {
        let mut out = String::new();
        for (i, (key, value)) in self.pairs.iter().enumerate() {
            if i > 0 {
                out.push('&');
            }
            out.push_str(&encode(key, EncodeSet::QueryValue));
            out.push('=');
            out.push_str(&encode(value, EncodeSet::QueryValue));
        }
        out
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_query_string())
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Query {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Query {
            pairs: iter
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

/// A query parameter that was missing or failed to parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    /// The parameter name.
    pub key: String,
    /// What went wrong.
    pub kind: QueryErrorKind,
}

/// The two ways a typed query lookup fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryErrorKind {
    /// The parameter was required but absent.
    Missing,
    /// The value did not parse.
    Invalid {
        /// The raw (decoded) value.
        value: String,
        /// The parser's explanation.
        reason: String,
    },
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            QueryErrorKind::Missing => write!(f, "missing query parameter `{}`", self.key),
            QueryErrorKind::Invalid { value, reason } => write!(
                f,
                "invalid value {value:?} for query parameter `{}`: {reason}",
                self.key
            ),
        }
    }
}

impl std::error::Error for QueryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_in_order_with_duplicates() {
        let query = Query::parse("?a=1&b=2&a=3");
        let pairs: Vec<_> = query.iter().collect();
        assert_eq!(pairs, [("a", "1"), ("b", "2"), ("a", "3")]);
    }

    #[test]
    fn keys_without_values_and_empty_pieces() {
        let query = Query::parse("flag&&x=&=orphan");
        assert!(query.contains("flag"));
        assert_eq!(query.get("flag"), Some(""));
        assert_eq!(query.get("x"), Some(""));
        assert_eq!(query.get(""), Some("orphan"));
        assert_eq!(query.len(), 3);
    }

    #[test]
    fn values_are_form_decoded() {
        let query = Query::parse("q=a+b%26c&name=caf%C3%A9&bad=%zz");
        assert_eq!(query.get("q"), Some("a b&c"));
        assert_eq!(query.get("name"), Some("café"));
        assert_eq!(query.get("bad"), Some("%zz"));
    }

    #[test]
    fn value_containing_equals_splits_on_first() {
        let query = Query::parse("expr=a=b");
        assert_eq!(query.get("expr"), Some("a=b"));
    }

    #[test]
    fn typed_lookup_errors_name_the_parameter() {
        let query = Query::parse("limit=ten&n=");
        let err = query.parse_value::<u32>("limit").expect_err("not a number");
        assert_eq!(err.key, "limit");
        assert_eq!(
            err.to_string(),
            "invalid value \"ten\" for query parameter `limit`: invalid digit found in string"
        );
        assert_eq!(query.parse_value::<u32>("n"), Ok(None));
        assert_eq!(
            query
                .require::<u32>("cursor")
                .expect_err("absent")
                .to_string(),
            "missing query parameter `cursor`"
        );
    }

    #[test]
    fn list_accepts_commas_and_repeats() {
        let query = Query::parse("types=job.succeeded,job.failed&types=queue.paused&types=");
        let types: Vec<_> = query.list("types").collect();
        assert_eq!(types, ["job.succeeded", "job.failed", "queue.paused"]);
    }

    #[test]
    fn set_and_remove() {
        let mut query = Query::parse("a=1&a=2&b=3");
        query.set("a", "9");
        query.remove("b");
        query.append("c", "x y");
        assert_eq!(query.to_query_string(), "a=9&c=x%20y");
    }

    #[test]
    fn encoding_round_trips_through_parse() {
        let original: Query = [("k&y", "v=1 +2"), ("empty", ""), ("uni", "\u{e9}")]
            .into_iter()
            .collect();
        let reparsed = Query::parse(&original.to_string());
        assert_eq!(reparsed, original);
    }
}
