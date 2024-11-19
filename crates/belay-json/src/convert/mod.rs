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
