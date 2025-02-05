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
