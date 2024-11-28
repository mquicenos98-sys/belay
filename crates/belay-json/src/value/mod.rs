//! The [`Value`] tree, its accessors and indexing.

mod from;

use std::fmt;
use std::mem;
use std::ops;
use std::str::FromStr;

use crate::parse::ParseError;
use crate::write::{self, WriteOptions};
use crate::{Map, Number};

/// Any JSON value.
///
/// `Display` writes compact JSON; the alternate flag (`{:#}`) writes the
/// two-space pretty form. Equality is structural, with the number semantics
/// described on [`Number`] and order-insensitive objects (see [`Map`]).
///
/// ```
/// use belay_json::{json, Value};
///
/// let job = json!({"queue": "emails", "attempts": [1, 2], "tag": null});
/// assert_eq!(job["queue"], "emails");
/// assert_eq!(job["attempts"][1], 2);
/// assert!(job["tag"].is_null());
/// assert!(job["no such key"].is_null());
/// assert_eq!(job.to_string(), r#"{"queue":"emails","attempts":[1,2],"tag":null}"#);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Value {
    /// `null`.
    #[default]
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Any number; see [`Number`] for how integers and floats are kept.
    Number(Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Value>),
    /// An object, in document order.
    Object(Map),
}

/// The type of a [`Value`], for messages like "expected string, found array".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `null`
    Null,
    /// `true` / `false`
    Bool,
    /// numbers
    Number,
    /// strings
    String,
    /// arrays
    Array,
    /// objects
    Object,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Null => "null",
            Kind::Bool => "boolean",
            Kind::Number => "number",
            Kind::String => "string",
            Kind::Array => "array",
            Kind::Object => "object",
        })
    }
}

/// Shared target for indexing misses, so `value["missing"]` can return a
/// reference instead of panicking.
static NULL: Value = Value::Null;

impl Value {
    /// The value's type.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Value::Null => Kind::Null,
            Value::Bool(_) => Kind::Bool,
            Value::Number(_) => Kind::Number,
            Value::String(_) => Kind::String,
            Value::Array(_) => Kind::Array,
            Value::Object(_) => Kind::Object,
        }
    }

    /// True for `null`.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// True for booleans.
    #[must_use]
    pub fn is_bool(&self) -> bool {
        matches!(self, Value::Bool(_))
    }

    /// True for numbers of any representation.
    #[must_use]
    pub fn is_number(&self) -> bool {
        matches!(self, Value::Number(_))
    }

    /// True for integers that fit in `i64`.
    #[must_use]
    pub fn is_i64(&self) -> bool {
        self.as_i64().is_some()
    }

    /// True for non-negative integers.
    #[must_use]
    pub fn is_u64(&self) -> bool {
        self.as_u64().is_some()
    }

    /// True for numbers stored as floats.
    #[must_use]
    pub fn is_f64(&self) -> bool {
        matches!(self, Value::Number(n) if n.is_f64())
    }

    /// True for strings.
    #[must_use]
    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }

    /// True for arrays.
    #[must_use]
    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    /// True for objects.
    #[must_use]
    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    /// The boolean, if this is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The number, if this is one.
    #[must_use]
    pub fn as_number(&self) -> Option<&Number> {
        match self {
            Value::Number(n) => Some(n),
            _ => None,
        }
    }

    /// The value as `i64`, if it is an integer in range (see
    /// [`Number::as_i64`]).
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        self.as_number().and_then(Number::as_i64)
    }

    /// The value as `u64`, if it is a non-negative integer.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        self.as_number().and_then(Number::as_u64)
    }

    /// The value as `f64`, for any number.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        self.as_number().map(Number::as_f64)
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The elements, if this is an array.
    #[must_use]
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The elements, mutably, if this is an array.
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The map, if this is an object.
    #[must_use]
    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// The map, mutably, if this is an object.
    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Looks up an object key (`&str`) or array position (`usize`).
    ///
    /// ```
    /// use belay_json::json;
    ///
    /// let v = json!({"steps": ["fetch", "store"]});
    /// assert_eq!(v.get("steps").and_then(|s| s.get(1)), Some(&json!("store")));
    /// assert_eq!(v.get(0), None);
    /// ```
    #[must_use]
    pub fn get<I: ValueIndex>(&self, index: I) -> Option<&Value> {
        index.lookup(self)
    }

    /// Mutable variant of [`get`](Value::get).
    pub fn get_mut<I: ValueIndex>(&mut self, index: I) -> Option<&mut Value> {
        index.lookup_mut(self)
    }

    /// Resolves an RFC 6901 JSON Pointer such as `/steps/0/result`.
    ///
    /// Returns `None` if the pointer is malformed or any step is missing.
    /// The empty pointer refers to the whole value. See
    /// [`Pointer`](crate::Pointer) for a parsed, reusable form.
    ///
    /// ```
    /// use belay_json::json;
    ///
    /// let v = json!({"a/b": [10, {"~k": true}]});
    /// assert_eq!(v.pointer("/a~1b/1/~0k"), Some(&json!(true)));
    /// assert_eq!(v.pointer("/a~1b/2"), None);
    /// ```
    #[must_use]
    pub fn pointer(&self, pointer: &str) -> Option<&Value> {
        crate::pointer::resolve(self, pointer)
    }

    /// Mutable variant of [`pointer`](Value::pointer).
    pub fn pointer_mut(&mut self, pointer: &str) -> Option<&mut Value> {
        crate::pointer::resolve_mut(self, pointer)
    }

    /// Replaces the value with `null` and returns the old one.
    pub fn take(&mut self) -> Value {
        mem::take(self)
    }

    /// Serializes with explicit options; `to_string()` (via `Display`) is the
    /// compact default.
    #[must_use]
    pub fn to_string_with(&self, options: &WriteOptions) -> String {
        write::to_string_with(self, options)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let options = if f.alternate() {
            WriteOptions::pretty()
        } else {
            WriteOptions::compact()
        };
        write::write_fmt(f, self, &options)
    }
}

impl FromStr for Value {
    type Err = ParseError;

    /// Parses a document with the default (strict) options.
    fn from_str(s: &str) -> Result<Value, ParseError> {
        crate::parse(s)
    }
}

/// Types that can index into a [`Value`]: `str`/`String` keys for objects and
/// `usize` positions for arrays. Sealed.
pub trait ValueIndex: private::Sealed {
    #[doc(hidden)]
    fn lookup<'v>(&self, value: &'v Value) -> Option<&'v Value>;

    #[doc(hidden)]
    fn lookup_mut<'v>(&self, value: &'v mut Value) -> Option<&'v mut Value>;

    /// Used by `IndexMut`: inserts missing object keys, panics otherwise.
    #[doc(hidden)]
    fn lookup_or_insert<'v>(&self, value: &'v mut Value) -> &'v mut Value;
}

mod private {
    pub trait Sealed {}
    impl Sealed for usize {}
    impl Sealed for str {}
    impl Sealed for String {}
    impl<T: Sealed + ?Sized> Sealed for &T {}
}

impl ValueIndex for usize {
    fn lookup<'v>(&self, value: &'v Value) -> Option<&'v Value> {
        value.as_array()?.get(*self)
    }

    fn lookup_mut<'v>(&self, value: &'v mut Value) -> Option<&'v mut Value> {
        value.as_array_mut()?.get_mut(*self)
    }

    fn lookup_or_insert<'v>(&self, value: &'v mut Value) -> &'v mut Value {
        match value {
            Value::Array(items) => {
                let len = items.len();
                items.get_mut(*self).unwrap_or_else(|| {
                    panic!("cannot index array of length {len} with position {self}")
                })
            }
            other => panic!("cannot index {} with position {self}", other.kind()),
        }
    }
}
