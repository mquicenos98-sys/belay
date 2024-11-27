//! `From` conversions into [`Value`] and comparisons with Rust primitives.
//!
//! These cover literals and std types by value; domain types implement
//! [`ToJson`](crate::ToJson) instead, which works by reference.

use std::borrow::Cow;

use crate::{Map, Number, Value};

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

macro_rules! from_integer {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(n: $t) -> Value {
                Value::Number(Number::from(n))
            }
        }
    )*};
}

from_integer!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

impl From<f64> for Value {
    /// NaN and infinities have no JSON form and become `null`.
    fn from(f: f64) -> Value {
        Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

impl From<f32> for Value {
    /// Widens through the shortest decimal form of the `f32`, so `0.1f32`
    /// becomes `0.1` rather than `0.10000000149011612`. NaN and infinities
    /// become `null`.
    fn from(f: f32) -> Value {
        if !f.is_finite() {
            return Value::Null;
        }
        // `f32`'s Display is its shortest round-trip form, which always
        // parses as an `f64`; the fallback only exists to avoid a panic path.
        let widened = f.to_string().parse::<f64>().unwrap_or(f64::from(f));
        Value::from(widened)
    }
}

impl From<Number> for Value {
    fn from(n: Number) -> Value {
        Value::Number(n)
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::String(s)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(s.to_owned())
    }
}

impl From<&String> for Value {
    fn from(s: &String) -> Value {
        Value::String(s.clone())
    }
}

impl From<Cow<'_, str>> for Value {
    fn from(s: Cow<'_, str>) -> Value {
        Value::String(s.into_owned())
    }
}

impl From<char> for Value {
    fn from(c: char) -> Value {
        Value::String(c.to_string())
    }
}

impl From<Map> for Value {
    fn from(map: Map) -> Value {
        Value::Object(map)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Value {
        Value::Array(items.into_iter().map(Into::into).collect())
    }
}

impl<T: Clone + Into<Value>> From<&[T]> for Value {
    fn from(items: &[T]) -> Value {
        Value::Array(items.iter().cloned().map(Into::into).collect())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    /// `None` becomes `null`.
    fn from(opt: Option<T>) -> Value {
        opt.map_or(Value::Null, Into::into)
    }
}

impl From<()> for Value {
    fn from((): ()) -> Value {
        Value::Null
    }
}

impl<T: Into<Value>> FromIterator<T> for Value {
    /// Collects into an array.
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Value {
        Value::Array(iter.into_iter().map(Into::into).collect())
    }
}

// Comparisons with primitives follow `Number`'s rules: integers only equal
// integer values and floats only equal float values, so `json!(1) == 1` but
// `json!(1) != 1.0`. They exist mainly so tests and handlers can write
// `assert_eq!(v["priority"], 5)`.

impl PartialEq<bool> for Value {
    fn eq(&self, other: &bool) -> bool {
        self.as_bool() == Some(*other)
    }
}

impl PartialEq<str> for Value {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == Some(other)
    }
}

impl PartialEq<&str> for Value {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}

impl PartialEq<String> for Value {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == Some(other.as_str())
    }
}

impl PartialEq<Value> for str {
    fn eq(&self, other: &Value) -> bool {
        other == self
    }
}

impl PartialEq<Value> for &str {
    fn eq(&self, other: &Value) -> bool {
        other == self
    }
}

impl PartialEq<Value> for String {
    fn eq(&self, other: &Value) -> bool {
        other == self
    }
}

macro_rules! eq_signed {
    ($($t:ty),*) => {$(
        impl PartialEq<$t> for Value {
            fn eq(&self, other: &$t) -> bool {
                self.as_i64() == Some(i64::from(*other))
            }
        }
    )*};
}

macro_rules! eq_unsigned {
    ($($t:ty),*) => {$(
        impl PartialEq<$t> for Value {
            fn eq(&self, other: &$t) -> bool {
                self.as_u64() == Some(u64::from(*other))
            }
        }
    )*};
}

eq_signed!(i8, i16, i32, i64);
eq_unsigned!(u8, u16, u32, u64);

impl PartialEq<isize> for Value {
    fn eq(&self, other: &isize) -> bool {
        self.as_i64() == i64::try_from(*other).ok()
    }
}

impl PartialEq<usize> for Value {
    fn eq(&self, other: &usize) -> bool {
        self.as_u64() == u64::try_from(*other).ok()
    }
}

impl PartialEq<f64> for Value {
    fn eq(&self, other: &f64) -> bool {
        matches!(self, Value::Number(n) if n.is_f64() && n.as_f64() == *other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    #[test]
    fn non_finite_floats_become_null() {
        assert_eq!(Value::from(f64::NAN), Value::Null);
        assert_eq!(Value::from(f64::INFINITY), Value::Null);
        assert_eq!(Value::from(f32::NEG_INFINITY), Value::Null);
    }

    #[test]
    fn f32_widens_through_shortest_decimal() {
        assert_eq!(Value::from(0.1f32).to_string(), "0.1");
        assert_eq!(Value::from(16777216.0f32).to_string(), "16777216.0");
        assert_eq!(Value::from(-2.5e-10f32).to_string(), "-2.5e-10");
    }

    #[test]
    fn containers_and_options_convert() {
        assert_eq!(Value::from(vec![1, 2]), json!([1, 2]));
        assert_eq!(Value::from(&["a", "b"][..]), json!(["a", "b"]));
        assert_eq!(Value::from(Some("x")), json!("x"));
        assert_eq!(Value::from(None::<i32>), Value::Null);
        assert_eq!(Value::from(()), Value::Null);
        assert_eq!(Value::from('q'), json!("q"));
        assert_eq!(Value::from(Cow::Borrowed("c")), json!("c"));
        let collected: Value = (1..=3).collect();
        assert_eq!(collected, json!([1, 2, 3]));
    }

    #[test]
    fn primitive_comparisons_follow_number_rules() {
        assert_eq!(json!(5), 5);
        assert_eq!(json!(5), 5u8);
        assert_eq!(json!(-5), -5i64);
        assert_ne!(json!(-5), 5u64);
        assert_eq!(json!(5), 5usize);
        assert_eq!(json!(-5), -5isize);
        assert_ne!(json!(5), 5.0);
        assert_eq!(json!(5.0), 5.0);
        assert_ne!(json!(5.0), 5);
        assert_eq!(json!("s"), "s");
        assert_eq!("s", json!("s"));
        assert_eq!(json!("s"), String::from("s"));
        assert_eq!(json!(true), true);
        assert_ne!(json!("true"), true);
    }

    #[test]
    fn map_collects_into_object_value() {
        let map: Map = [("a", 1), ("b", 2)].into_iter().collect();
        assert_eq!(Value::from(map), json!({"a": 1, "b": 2}));
    }
}
