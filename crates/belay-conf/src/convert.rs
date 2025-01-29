//! Conversions from [`Value`] into Rust types, used by the typed getters of
//! [`Document`](crate::Document) and [`Reader`](crate::Reader).
//!
//! Conversion errors carry a path relative to the converted value (for an
//! element of a list, `[3]`); the caller joins it with the value's own path
//! and attaches the source location.

use std::collections::{BTreeMap, HashMap};

use crate::path::{KeyPath, Segment};
use crate::value::{Table, Value};

/// Why a [`Value`] could not be converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertErrorKind {
    /// The value has the wrong type.
    Type {
        /// What the target type needs, e.g. `"integer"`.
        expected: &'static str,
        /// What the value is.
        found: &'static str,
    },
    /// The type fits but the value does not (range, format, validation).
    Invalid(String),
}

/// A failed conversion, located relative to the converted value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError {
    path: KeyPath,
    kind: ConvertErrorKind,
}

impl ConvertError {
    /// The value is a `found` where an `expected` was needed.
    pub fn mismatch(expected: &'static str, found: &Value) -> ConvertError {
        ConvertError {
            path: KeyPath::root(),
            kind: ConvertErrorKind::Type {
                expected,
                found: found.type_name(),
            },
        }
    }

    /// The value is unacceptable; `message` says why, e.g. "must be at least 1, got 0".
    pub fn invalid(message: impl Into<String>) -> ConvertError {
        ConvertError {
            path: KeyPath::root(),
            kind: ConvertErrorKind::Invalid(message.into()),
        }
    }

    /// Marks the error as coming from the element or entry `segment` of the
    /// converted value. Container conversions call this on their children.
    #[must_use]
    pub fn within(mut self, segment: Segment) -> ConvertError {
        let mut path = KeyPath::root();
        path.push(segment);
        self.path = path.join(&self.path);
        self
    }

    /// Where inside the converted value the problem is.
    pub fn path(&self) -> &KeyPath {
        &self.path
    }

    /// What went wrong.
    pub fn kind(&self) -> &ConvertErrorKind {
        &self.kind
    }
}

impl From<String> for ConvertError {
    fn from(message: String) -> ConvertError {
        ConvertError::invalid(message)
    }
}

impl From<&str> for ConvertError {
    fn from(message: &str) -> ConvertError {
        ConvertError::invalid(message)
    }
}

/// Types that can be extracted from a [`Value`].
///
/// Implement it for your own configuration types to read them with
/// [`Reader::req`](crate::Reader::req) and friends:
///
/// ```
/// use belay_conf::{ConvertError, FromValue, Value};
///
/// enum Format { Logfmt, Json }
///
/// impl FromValue for Format {
///     fn from_value(v: &Value) -> Result<Format, ConvertError> {
///         match v.as_str() {
///             Some("logfmt") => Ok(Format::Logfmt),
///             Some("json") => Ok(Format::Json),
///             Some(other) => Err(format!("expected `logfmt` or `json`, got `{other}`").into()),
///             None => Err(ConvertError::mismatch("string", v)),
///         }
///     }
/// }
/// ```
pub trait FromValue: Sized {
    /// Converts `value`, or explains why it cannot be.
    fn from_value(value: &Value) -> Result<Self, ConvertError>;
}

impl FromValue for Value {
    fn from_value(value: &Value) -> Result<Value, ConvertError> {
        Ok(value.clone())
    }
}

/// Unquoted date-times are accepted too: they are strings to the consumer.
impl FromValue for String {
    fn from_value(value: &Value) -> Result<String, ConvertError> {
        match value {
            Value::String(s) | Value::Datetime(s) => Ok(s.clone()),
            other => Err(ConvertError::mismatch("string", other)),
        }
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Result<bool, ConvertError> {
        value
            .as_bool()
            .ok_or_else(|| ConvertError::mismatch("boolean", value))
    }
}

/// Integers are accepted where a float is expected: `ratio = 1` is fine.
impl FromValue for f64 {
    fn from_value(value: &Value) -> Result<f64, ConvertError> {
        match value {
            Value::Float(f) => Ok(*f),
            Value::Integer(i) => Ok(*i as f64),
            other => Err(ConvertError::mismatch("float", other)),
        }
    }
}

macro_rules! from_value_int {
    ($($t:ty),*) => {$(
        impl FromValue for $t {
            fn from_value(value: &Value) -> Result<$t, ConvertError> {
                let i = value
                    .as_integer()
                    .ok_or_else(|| ConvertError::mismatch("integer", value))?;
                <$t>::try_from(i).map_err(|_| {
                    let message = if i < 0 && <$t>::MIN == 0 {
                        format!("must not be negative, got {i}")
                    } else {
                        format!("must be between {} and {}, got {i}", <$t>::MIN, <$t>::MAX)
                    };
                    ConvertError::invalid(message)
                })
            }
        }
    )*};
}

from_value_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl FromValue for Table {
    fn from_value(value: &Value) -> Result<Table, ConvertError> {
        value
            .as_table()
            .cloned()
            .ok_or_else(|| ConvertError::mismatch("table", value))
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(value: &Value) -> Result<Vec<T>, ConvertError> {
        let items = value
            .as_array()
            .ok_or_else(|| ConvertError::mismatch("array", value))?;
        items
            .iter()
            .enumerate()
            .map(|(i, v)| T::from_value(v).map_err(|e| e.within(Segment::Index(i))))
            .collect()
    }
}

impl<T: FromValue> FromValue for BTreeMap<String, T> {
    fn from_value(value: &Value) -> Result<BTreeMap<String, T>, ConvertError> {
        entries(value)
    }
}

impl<T: FromValue> FromValue for HashMap<String, T> {
    fn from_value(value: &Value) -> Result<HashMap<String, T>, ConvertError> {
        entries(value)
    }
}

fn entries<T, C>(value: &Value) -> Result<C, ConvertError>
where
    T: FromValue,
    C: FromIterator<(String, T)>,
{
    let table = value
        .as_table()
        .ok_or_else(|| ConvertError::mismatch("table", value))?;
    table
        .iter()
        .map(|(k, v)| {
            T::from_value(v)
                .map(|t| (k.to_owned(), t))
                .map_err(|e| e.within(Segment::Key(k.to_owned())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(message: &str) -> ConvertErrorKind {
        ConvertErrorKind::Invalid(message.into())
    }

    #[test]
    fn integers_are_range_checked() {
        assert_eq!(u16::from_value(&Value::Integer(7700)), Ok(7700));
        let err = |r: Result<(), ConvertError>| r.unwrap_err().kind().clone();
        let too_big = u16::from_value(&Value::Integer(70_000)).map(drop);
        assert_eq!(
            err(too_big),
            invalid("must be between 0 and 65535, got 70000")
        );
        let negative = u32::from_value(&Value::Integer(-1)).map(drop);
        assert_eq!(err(negative), invalid("must not be negative, got -1"));
        let too_small = i8::from_value(&Value::Integer(-200)).map(drop);
        assert_eq!(
            err(too_small),
            invalid("must be between -128 and 127, got -200")
        );
    }

    #[test]
    fn type_mismatches_name_both_types() {
        let err = bool::from_value(&Value::from("yes")).unwrap_err();
        let expected = ConvertErrorKind::Type {
            expected: "boolean",
            found: "string",
        };
        assert_eq!(err.kind(), &expected);
        assert_eq!(f64::from_value(&Value::Integer(2)), Ok(2.0));
        let date = String::from_value(&Value::Datetime("2025-01-01".into()));
        assert_eq!(date.as_deref(), Ok("2025-01-01"));
    }

    #[test]
    fn container_errors_point_at_the_element() {
        let v = Value::from(vec![Value::from("a"), Value::Integer(3)]);
        let err = Vec::<String>::from_value(&v).unwrap_err();
        assert_eq!(err.path().to_string(), "[1]");

        let t: Table = [("team", Value::from("ops")), ("tier", Value::Boolean(true))]
            .into_iter()
            .collect();
        let err = BTreeMap::<String, String>::from_value(&Value::Table(t.clone())).unwrap_err();
        assert_eq!(err.path().to_string(), "tier");
        let ok = HashMap::<String, Value>::from_value(&Value::Table(t)).unwrap();
        assert_eq!(ok.len(), 2);
        assert!(BTreeMap::<String, String>::from_value(&Value::Integer(1)).is_err());
    }
}
