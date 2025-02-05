//! The document model: [`Value`] and the insertion-ordered [`Table`].

use std::collections::HashMap;
use std::fmt;

use crate::path::{KeyPath, Segment};

/// A configuration value.
///
/// Integers are 64-bit signed as TOML prescribes. Date-times are validated
/// but kept as the text that was written: interpreting them (and durations,
/// addresses and other domain types) is left to the consumer.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A string.
    String(String),
    /// A 64-bit signed integer.
    Integer(i64),
    /// A double-precision float; may be `inf` or `nan`.
    Float(f64),
    /// `true` or `false`.
    Boolean(bool),
    /// An offset date-time, local date-time, local date or local time,
    /// exactly as written (`1979-05-27T07:32:00Z`).
    Datetime(String),
    /// An array; elements may have mixed types.
    Array(Vec<Value>),
    /// A table.
    Table(Table),
}

impl Value {
    /// The TOML name of this value's type, as used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::String(_) => "string",
            Value::Integer(_) => "integer",
            Value::Float(_) => "float",
            Value::Boolean(_) => "boolean",
            Value::Datetime(_) => "datetime",
            Value::Array(_) => "array",
            Value::Table(_) => "table",
        }
    }

    /// The string, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The integer, if this is one.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// The float, if this is one. Integers are not converted.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// The boolean, if this is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// The date-time text, if this is a date-time.
    pub fn as_datetime(&self) -> Option<&str> {
        match self {
            Value::Datetime(s) => Some(s),
            _ => None,
        }
    }

    /// The elements, if this is an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Mutable access to the elements, if this is an array.
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The table, if this is one.
    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Mutable access to the table, if this is one.
    pub fn as_table_mut(&mut self) -> Option<&mut Table> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Follows `path` from this value. The root path yields `self`.
    pub fn lookup(&self, path: &KeyPath) -> Option<&Value> {
        path.segments().iter().try_fold(self, step)
    }

    /// Mutable variant of [`Value::lookup`].
    pub fn lookup_mut(&mut self, path: &KeyPath) -> Option<&mut Value> {
        path.segments().iter().try_fold(self, step_mut)
    }
}

fn step<'v>(v: &'v Value, seg: &Segment) -> Option<&'v Value> {
    match (v, seg) {
        (Value::Table(t), Segment::Key(k)) => t.get(k),
        (Value::Array(items), Segment::Index(i)) => items.get(*i),
        _ => None,
    }
}

pub(crate) fn step_mut<'v>(v: &'v mut Value, seg: &Segment) -> Option<&'v mut Value> {
    match (v, seg) {
        (Value::Table(t), Segment::Key(k)) => t.get_mut(k),
        (Value::Array(items), Segment::Index(i)) => items.get_mut(*i),
        _ => None,
    }
}

/// Renders the value in TOML inline syntax (`"text"`, `[1, 2]`, `{ a = 1 }`).
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        crate::ser::write_inline(&mut out, self);
        f.write_str(&out)
    }
}

macro_rules! value_from {
    ($($t:ty => $variant:ident($conv:expr)),* $(,)?) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Value {
                Value::$variant($conv(v))
            }
        }
    )*};
}

value_from! {
    &str => String(str::to_owned),
    String => String(String::from),
    i64 => Integer(i64::from),
    i32 => Integer(i64::from),
    u32 => Integer(i64::from),
    f64 => Float(f64::from),
    bool => Boolean(bool::from),
    Table => Table(Table::from),
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Value {
        Value::Array(items.into_iter().map(Into::into).collect())
    }
}
