//! Conversion between Rust types and [`Value`]: the [`ToJson`] and
//! [`FromJson`] traits, their error type, and helpers for hand-written impls.
//!
//! Decoding errors carry the path to the offending value. Paths are built
//! while unwinding: the leaf creates the error, and each container on the way
//! out prepends its key or index, so no bookkeeping happens on the success
//! path.

mod impls;
mod object;

pub use object::{ObjectBuilder, ObjectReader};

use std::fmt;
use std::str::FromStr;

use crate::write::write_string;
use crate::Value;

/// Types that can be rendered as a JSON [`Value`].
///
/// ```
/// use belay_json::{ObjectBuilder, ToJson, Value};
///
/// struct Lease { worker: String, ttl_ms: u64, note: Option<String> }
///
/// impl ToJson for Lease {
///     fn to_json(&self) -> Value {
///         ObjectBuilder::new()
///             .field("worker", &self.worker)
///             .field("ttl_ms", self.ttl_ms)
///             .opt_field("note", &self.note)
///             .build()
///     }
/// }
///
/// let lease = Lease { worker: "w1".into(), ttl_ms: 30_000, note: None };
/// assert_eq!(lease.to_json().to_string(), r#"{"worker":"w1","ttl_ms":30000}"#);
/// ```
pub trait ToJson {
    /// Builds the JSON representation.
    fn to_json(&self) -> Value;
}

/// Types that can be decoded from a JSON [`Value`].
///
/// ```
/// use belay_json::{FromJson, FromJsonError, ObjectReader, Value, json};
///
/// #[derive(Debug, PartialEq)]
/// struct Lease { worker: String, ttl_ms: u64, note: Option<String> }
///
/// impl FromJson for Lease {
///     fn from_json(value: &Value) -> Result<Self, FromJsonError> {
///         let r = ObjectReader::new(value, "Lease")?;
///         let lease = Lease {
///             worker: r.req("worker")?,
///             ttl_ms: r.or("ttl_ms", 30_000)?,
///             note: r.opt("note")?,
///         };
///         r.deny_unknown_fields()?;
///         Ok(lease)
///     }
/// }
///
/// let lease = Lease::from_json(&json!({"worker": "w1"})).unwrap();
/// assert_eq!(lease.ttl_ms, 30_000);
///
/// let err = Vec::<Lease>::from_json(&json!([{"worker": "w1"}, {"worker": "w2", "ttl_ms": -1}]))
///     .unwrap_err();
/// assert_eq!(
///     err.to_string(),
///     "$[1].ttl_ms: expected integer in range 0..=18446744073709551615, found -1"
/// );
/// ```
pub trait FromJson: Sized {
    /// Decodes a borrowed value.
    fn from_json(value: &Value) -> Result<Self, FromJsonError>;

    /// Decodes an owned value. The default borrows; types that can reuse the
    /// value's allocations (strings, arrays, `Value` itself) override it.
    fn from_json_owned(value: Value) -> Result<Self, FromJsonError> {
        Self::from_json(&value)
    }
}

/// One step of the path to a value inside a document.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSegment {
    /// An object member.
    Key(String),
    /// An array element.
    Index(usize),
}

/// A value that could not be decoded, with the path to it.
///
/// `Display` reads like `$.jobs[2].priority: expected integer in range
/// -32768..=32767, found 40000`. The path uses `$` for the root, `.key` for
/// identifier-like keys, `["odd key"]` for anything else, and `[n]` for
/// array positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromJsonError {
    path: Vec<PathSegment>,
    message: String,
}

impl FromJsonError {
    /// An error with a free-form message at the current position.
    #[must_use]
    pub fn new(message: impl Into<String>) -> FromJsonError {
        FromJsonError {
            path: Vec::new(),
            message: message.into(),
        }
    }

    /// `expected {what}, found {description of value}`.
    ///
    /// ```
    /// use belay_json::{json, FromJsonError};
    ///
    /// let err = FromJsonError::expected("a queue name", &json!(["q"]));
    /// assert_eq!(err.message(), "expected a queue name, found array of 1 element");
    /// ```
    #[must_use]
    pub fn expected(what: impl fmt::Display, found: &Value) -> FromJsonError {
        FromJsonError::new(format!("expected {what}, found {}", describe(found)))
    }

    /// A required field is absent from an object of type `type_name`.
    #[must_use]
    pub fn missing_field(field: &str, type_name: &str) -> FromJsonError {
        FromJsonError::new(format!(
            "missing required field {} in {type_name}",
            quote(field)
        ))
    }

    /// An object of type `type_name` has a field it does not define. Suggests
    /// the closest of `known` when one is plausibly a typo.
    #[must_use]
    pub fn unknown_field(field: &str, type_name: &str, known: &[&str]) -> FromJsonError {
        let mut message = format!("unknown field {} in {type_name}", quote(field));
        append_hint(&mut message, field, known);
        FromJsonError::new(message)
    }

    /// A string does not name any variant of the enum `type_name`.
    ///
    /// ```
    /// use belay_json::FromJsonError;
    ///
    /// let err = FromJsonError::unknown_variant("fire_onse", "MisfirePolicy", &["skip", "fire_once"]);
    /// assert_eq!(
    ///     err.message(),
    ///     r#"unknown MisfirePolicy "fire_onse", did you mean "fire_once"?"#
    /// );
    /// ```
    #[must_use]
    pub fn unknown_variant(found: &str, type_name: &str, known: &[&str]) -> FromJsonError {
        let mut message = format!("unknown {type_name} {}", quote(found));
        append_hint(&mut message, found, known);
        FromJsonError::new(message)
    }

    /// Prepends an object key to the path.
    #[must_use]
    pub fn at_key(mut self, key: impl Into<String>) -> FromJsonError {
        self.path.insert(0, PathSegment::Key(key.into()));
        self
    }

    /// Prepends an array position to the path.
    #[must_use]
    pub fn at_index(mut self, index: usize) -> FromJsonError {
        self.path.insert(0, PathSegment::Index(index));
        self
    }

    /// Path from the document root to the offending value.
    #[must_use]
    pub fn path(&self) -> &[PathSegment] {
        &self.path
    }

    /// The path in `$.a[0]["b c"]` notation.
    #[must_use]
    pub fn path_string(&self) -> String {
        let mut out = String::from("$");
        for segment in &self.path {
            match segment {
                PathSegment::Index(i) => out.push_str(&format!("[{i}]")),
                PathSegment::Key(k) if is_identifier(k) => {
                    out.push('.');
                    out.push_str(k);
                }
                PathSegment::Key(k) => {
                    out.push('[');
                    out.push_str(&quote(k));
                    out.push(']');
                }
            }
        }
        out
    }

    /// The path as an RFC 6901 JSON Pointer (`/jobs/2/priority`), e.g. for
    /// machine-readable API error details.
    #[must_use]
    pub fn pointer(&self) -> String {
        let mut pointer = crate::Pointer::root();
        for segment in &self.path {
            match segment {
                PathSegment::Key(k) => pointer.push(k),
                PathSegment::Index(i) => pointer.push(i),
            }
        }
        pointer.to_string()
    }

    /// The message without the path.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FromJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path_string(), self.message)
    }
}

impl std::error::Error for FromJsonError {}

/// Decodes a JSON string through `T`'s `FromStr`, for types with a canonical
/// text form (identifiers, enums with lowercase names, durations).
///
/// `what` names the type in messages: `expected queue name string, found 5`,
/// `invalid queue name "Bad Name": must be lowercase`.
///
/// ```
/// use std::net::Ipv4Addr;
/// use belay_json::{json, parse_str_as};
///
/// let ip: Ipv4Addr = parse_str_as(&json!("10.0.0.1"), "IPv4 address").unwrap();
/// assert_eq!(ip, Ipv4Addr::new(10, 0, 0, 1));
/// let err = parse_str_as::<Ipv4Addr>(&json!("10.0.0"), "IPv4 address").unwrap_err();
/// assert_eq!(err.message(), r#"invalid IPv4 address "10.0.0": invalid IPv4 address syntax"#);
/// ```
pub fn parse_str_as<T>(value: &Value, what: &str) -> Result<T, FromJsonError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    let s = value
        .as_str()
        .ok_or_else(|| FromJsonError::expected(format_args!("{what} string"), value))?;
    s.parse()
        .map_err(|e| FromJsonError::new(format!("invalid {what} {}: {e}", quote(s))))
}

/// Short description of a value for "found ..." messages. Scalars are shown
/// literally (long strings truncated); containers by type and size.
pub(crate) fn describe(value: &Value) -> String {
    const MAX_CHARS: usize = 40;
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) if s.chars().count() > MAX_CHARS => {
            let head: String = s.chars().take(MAX_CHARS - 3).collect();
            format!("string {}", quote(&(head + "...")))
        }
        Value::String(s) => format!("string {}", quote(s)),
        Value::Array(items) => match items.len() {
            0 => "empty array".to_owned(),
            1 => "array of 1 element".to_owned(),
            n => format!("array of {n} elements"),
        },
        Value::Object(map) if map.is_empty() => "empty object".to_owned(),
        Value::Object(_) => "object".to_owned(),
    }
}

/// Quotes a string the way JSON does, so control characters stay visible.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    // Writing to a String cannot fail.
    let _ = write_string(&mut out, s);
    out
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Adds `, did you mean "x"?` or `, expected one of: ...` to a message about
/// an unrecognised name.
fn append_hint(message: &mut String, found: &str, known: &[&str]) {
    if known.is_empty() {
        return;
    }
    if let Some(best) = closest(found, known) {
        message.push_str(&format!(", did you mean {}?", quote(best)));
        return;
    }
    let mut unique: Vec<&str> = Vec::with_capacity(known.len());
    for k in known {
        if !unique.contains(k) {
            unique.push(k);
        }
    }
    message.push_str(", expected one of: ");
    message.push_str(&unique.join(", "));
}

/// The known name most likely meant by `found`: equal after ignoring case and
/// `_`/`-` (so `maxAttempts` finds `max_attempts`), or within a small edit
/// distance.
fn closest<'k>(found: &str, known: &[&'k str]) -> Option<&'k str> {
    let normalize = |s: &str| -> String {
        s.chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect()
    };
    let wanted = normalize(found);
    if let Some(k) = known.iter().find(|k| normalize(k) == wanted) {
        return Some(k);
    }
    // Scale the tolerance with length, and never suggest a name that shares
    // nothing with the input (any 1-char key is 1 edit from any other).
    let len = found.chars().count();
    let limit = match len {
        0..=3 => 1,
        4..=8 => 2,
        _ => 3,
    };
    known
        .iter()
        .map(|k| (edit_distance(found, k), *k))
        .filter(|(d, _)| *d <= limit && *d < len)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Levenshtein distance over chars, with a two-row table.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != *cb);
            cur[j + 1] = substitution.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    #[test]
    fn path_notation_quotes_non_identifiers() {
        let err = FromJsonError::new("bad")
            .at_key("priority")
            .at_index(2)
            .at_key("jobs");
        assert_eq!(err.to_string(), "$.jobs[2].priority: bad");
        let odd = FromJsonError::new("bad")
            .at_key("belay.schedule")
            .at_key("meta");
        assert_eq!(odd.path_string(), r#"$.meta["belay.schedule"]"#);
        let empty = FromJsonError::new("bad").at_key("").at_key("9lives");
        assert_eq!(empty.path_string(), r#"$["9lives"][""]"#);
        assert_eq!(FromJsonError::new("bad").to_string(), "$: bad");
    }

    #[test]
    fn pointer_form_escapes_tokens() {
        let err = FromJsonError::new("x")
            .at_key("a/b")
            .at_index(0)
            .at_key("m~n");
        assert_eq!(err.pointer(), "/m~0n/0/a~1b");
        assert_eq!(FromJsonError::new("x").pointer(), "");
    }

    #[test]
    fn describes_found_values() {
        assert_eq!(describe(&json!(null)), "null");
        assert_eq!(describe(&json!(false)), "false");
        assert_eq!(describe(&json!(40000)), "40000");
        assert_eq!(describe(&json!(1.5)), "1.5");
        assert_eq!(describe(&json!("hi\n")), r#"string "hi\n""#);
        assert_eq!(describe(&json!([])), "empty array");
        assert_eq!(describe(&json!([1, 2])), "array of 2 elements");
        assert_eq!(describe(&json!({})), "empty object");
        assert_eq!(describe(&json!({"a": 1})), "object");
        let long = "x".repeat(100);
        assert_eq!(
            describe(&json!(long)),
            format!("string \"{}...\"", "x".repeat(37))
        );
    }

    #[test]
    fn suggestions_catch_typos_and_case_styles() {
        let known = ["queue", "kind", "max_attempts", "run_at"];
        assert_eq!(closest("qeueu", &known), Some("queue"));
        assert_eq!(closest("maxAttempts", &known), Some("max_attempts"));
        assert_eq!(closest("run-at", &known), Some("run_at"));
        assert_eq!(closest("knd", &known), Some("kind"));
        assert_eq!(closest("payload", &known), None);
        assert_eq!(closest("x", &["y", "zz"]), None);
        assert_eq!(closest("ab", &["ac"]), Some("ac"));
    }

    #[test]
    fn unknown_field_lists_known_names_without_duplicates() {
        let err = FromJsonError::unknown_field("colour", "Style", &["size", "weight", "size"]);
        assert_eq!(
            err.message(),
            r#"unknown field "colour" in Style, expected one of: size, weight"#
        );
        let err = FromJsonError::unknown_field("sise", "Style", &["size"]);
        assert_eq!(
            err.message(),
            r#"unknown field "sise" in Style, did you mean "size"?"#
        );
        let err = FromJsonError::unknown_field("x", "Empty", &[]);
        assert_eq!(err.message(), r#"unknown field "x" in Empty"#);
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("héllo", "hello"), 1);
    }

    #[test]
    fn parse_str_as_reports_type_and_parse_errors() {
        let err = parse_str_as::<u16>(&json!(5), "port").unwrap_err();
        assert_eq!(err.message(), "expected port string, found 5");
        let err = parse_str_as::<u16>(&json!("99999"), "port").unwrap_err();
        assert_eq!(
            err.message(),
            r#"invalid port "99999": number too large to fit in target type"#
        );
        assert_eq!(parse_str_as::<u16>(&json!("8080"), "port"), Ok(8080));
    }
}
