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

/// A map from keys to values that remembers insertion order.
///
/// Iteration and serialization follow insertion order, so printed
/// configuration reads like the file it came from. Equality ignores order,
/// matching TOML's semantics where tables are unordered.
#[derive(Clone, Default)]
pub struct Table {
    entries: Vec<(String, Value)>,
    index: HashMap<String, usize>,
}

impl Table {
    /// An empty table.
    pub fn new() -> Table {
        Table::default()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value for `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.index.get(key).map(|&i| &self.entries[i].1)
    }

    /// Mutable access to the value for `key`.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        let i = *self.index.get(key)?;
        Some(&mut self.entries[i].1)
    }

    /// Whether `key` is present.
    pub fn contains_key(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    /// Inserts or replaces a value. A replaced key keeps its position.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();
        if let Some(&i) = self.index.get(&key) {
            return Some(std::mem::replace(&mut self.entries[i].1, value));
        }
        self.index.insert(key.clone(), self.entries.len());
        self.entries.push((key, value));
        None
    }

    /// Removes a key, preserving the order of the remaining entries.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let i = self.index.remove(key)?;
        let (_, value) = self.entries.remove(i);
        for (k, _) in &self.entries[i..] {
            if let Some(slot) = self.index.get_mut(k) {
                *slot -= 1;
            }
        }
        Some(value)
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> Iter<'_> {
        Iter(self.entries.iter())
    }

    /// Keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// Follows `path` from this table; the root path yields `None` because
    /// the table itself is not a [`Value`].
    pub fn lookup(&self, path: &KeyPath) -> Option<&Value> {
        let (first, rest) = path.segments().split_first()?;
        let Segment::Key(k) = first else { return None };
        rest.iter().try_fold(self.get(k)?, step)
    }

    /// Mutable variant of [`Table::lookup`].
    pub fn lookup_mut(&mut self, path: &KeyPath) -> Option<&mut Value> {
        let (first, rest) = path.segments().split_first()?;
        let Segment::Key(k) = first else { return None };
        rest.iter().try_fold(self.get_mut(k)?, step_mut)
    }
}

impl PartialEq for Table {
    fn eq(&self, other: &Table) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl fmt::Debug for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Table {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Table {
        let mut t = Table::new();
        for (k, v) in iter {
            t.insert(k, v);
        }
        t
    }
}

impl<'a> IntoIterator for &'a Table {
    type Item = (&'a str, &'a Value);
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

/// Iterator over a [`Table`]'s entries in insertion order.
#[derive(Debug, Clone)]
pub struct Iter<'a>(std::slice::Iter<'a, (String, Value)>);
