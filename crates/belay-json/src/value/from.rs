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
