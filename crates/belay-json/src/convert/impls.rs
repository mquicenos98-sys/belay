//! [`ToJson`]/[`FromJson`] for std types.
//!
//! Integers are range checked and also accept integral floats (`5.0` decodes
//! as `5_u8`), since some encoders write every number as a double. Maps
//! encode keys through `Display` and decode them through `FromStr`, so
//! `BTreeMap<String, _>` and maps keyed by identifier types both work; hash
//! containers are written in sorted order so output is deterministic.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::hash::{BuildHasher, Hash};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;

use super::{FromJson, FromJsonError, ToJson};
use crate::{Map, Number, Value};

impl ToJson for Value {
    fn to_json(&self) -> Value {
        self.clone()
    }
}

impl FromJson for Value {
    fn from_json(value: &Value) -> Result<Value, FromJsonError> {
        Ok(value.clone())
    }

    fn from_json_owned(value: Value) -> Result<Value, FromJsonError> {
        Ok(value)
    }
}

impl ToJson for Map {
    fn to_json(&self) -> Value {
        Value::Object(self.clone())
    }
}

impl FromJson for Map {
    fn from_json(value: &Value) -> Result<Map, FromJsonError> {
        value
            .as_object()
            .cloned()
            .ok_or_else(|| FromJsonError::expected("object", value))
    }

    fn from_json_owned(value: Value) -> Result<Map, FromJsonError> {
        match value {
            Value::Object(map) => Ok(map),
            other => Err(FromJsonError::expected("object", &other)),
        }
    }
}

impl ToJson for Number {
    fn to_json(&self) -> Value {
        Value::Number(*self)
    }
}

impl FromJson for Number {
    fn from_json(value: &Value) -> Result<Number, FromJsonError> {
        value
            .as_number()
            .copied()
            .ok_or_else(|| FromJsonError::expected("number", value))
    }
}

impl ToJson for bool {
    fn to_json(&self) -> Value {
        Value::Bool(*self)
    }
}

impl FromJson for bool {
    fn from_json(value: &Value) -> Result<bool, FromJsonError> {
        value
            .as_bool()
            .ok_or_else(|| FromJsonError::expected("boolean", value))
    }
}

macro_rules! integer_impls {
    ($($t:ty),*) => {$(
        impl ToJson for $t {
            fn to_json(&self) -> Value {
                Value::Number(Number::from(*self))
            }
        }

        impl FromJson for $t {
            fn from_json(value: &Value) -> Result<$t, FromJsonError> {
                value
                    .as_number()
                    .and_then(Number::as_i128_exact)
                    .and_then(|n| <$t>::try_from(n).ok())
                    .ok_or_else(|| {
                        FromJsonError::expected(
                            format_args!("integer in range {}..={}", <$t>::MIN, <$t>::MAX),
                            value,
                        )
                    })
            }
        }
    )*};
}

integer_impls!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

impl ToJson for f64 {
    /// NaN and infinities become `null`.
    fn to_json(&self) -> Value {
        Value::from(*self)
    }
}

impl FromJson for f64 {
    fn from_json(value: &Value) -> Result<f64, FromJsonError> {
        value
            .as_f64()
            .ok_or_else(|| FromJsonError::expected("number", value))
    }
}

impl ToJson for f32 {
    /// NaN and infinities become `null`.
    fn to_json(&self) -> Value {
        Value::from(*self)
    }
}

impl FromJson for f32 {
    fn from_json(value: &Value) -> Result<f32, FromJsonError> {
        let wide = f64::from_json(value)?;
        let narrow = wide as f32;
        if narrow.is_finite() {
            Ok(narrow)
        } else {
            Err(FromJsonError::expected("number within f32 range", value))
        }
    }
}

impl ToJson for str {
    fn to_json(&self) -> Value {
        Value::String(self.to_owned())
    }
}

impl ToJson for String {
    fn to_json(&self) -> Value {
        Value::String(self.clone())
    }
}

impl FromJson for String {
    fn from_json(value: &Value) -> Result<String, FromJsonError> {
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| FromJsonError::expected("string", value))
    }

    fn from_json_owned(value: Value) -> Result<String, FromJsonError> {
        match value {
            Value::String(s) => Ok(s),
            other => Err(FromJsonError::expected("string", &other)),
        }
    }
}

impl ToJson for char {
    fn to_json(&self) -> Value {
        Value::String(self.to_string())
    }
}

impl FromJson for char {
    fn from_json(value: &Value) -> Result<char, FromJsonError> {
        let mut chars = value.as_str().unwrap_or_default().chars();
        match (value.is_string(), chars.next(), chars.next()) {
            (true, Some(c), None) => Ok(c),
            _ => Err(FromJsonError::expected("single-character string", value)),
        }
    }
}

impl ToJson for () {
    fn to_json(&self) -> Value {
        Value::Null
    }
}

impl FromJson for () {
    fn from_json(value: &Value) -> Result<(), FromJsonError> {
        if value.is_null() {
            Ok(())
        } else {
            Err(FromJsonError::expected("null", value))
        }
    }
}

impl<T: ToJson + ?Sized> ToJson for &T {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}

impl<T: ToJson + ?Sized> ToJson for Box<T> {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}
