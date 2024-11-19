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

impl<T: FromJson> FromJson for Box<T> {
    fn from_json(value: &Value) -> Result<Box<T>, FromJsonError> {
        T::from_json(value).map(Box::new)
    }

    fn from_json_owned(value: Value) -> Result<Box<T>, FromJsonError> {
        T::from_json_owned(value).map(Box::new)
    }
}

impl<T: ToJson + ?Sized> ToJson for Arc<T> {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}

impl<T: FromJson> FromJson for Arc<T> {
    fn from_json(value: &Value) -> Result<Arc<T>, FromJsonError> {
        T::from_json(value).map(Arc::new)
    }

    fn from_json_owned(value: Value) -> Result<Arc<T>, FromJsonError> {
        T::from_json_owned(value).map(Arc::new)
    }
}

impl<T: ToJson + ?Sized> ToJson for Rc<T> {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}

impl<T: FromJson> FromJson for Rc<T> {
    fn from_json(value: &Value) -> Result<Rc<T>, FromJsonError> {
        T::from_json(value).map(Rc::new)
    }
}

impl<T: ToJson + ToOwned + ?Sized> ToJson for Cow<'_, T> {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}

impl<T: ToJson> ToJson for Option<T> {
    /// `None` becomes `null`. To omit the field instead, use
    /// [`ObjectBuilder::opt_field`](crate::ObjectBuilder::opt_field).
    fn to_json(&self) -> Value {
        self.as_ref().map_or(Value::Null, ToJson::to_json)
    }
}

impl<T: FromJson> FromJson for Option<T> {
    /// `null` decodes as `None`; anything else must decode as `T`.
    fn from_json(value: &Value) -> Result<Option<T>, FromJsonError> {
        match value {
            Value::Null => Ok(None),
            other => T::from_json(other).map(Some),
        }
    }

    fn from_json_owned(value: Value) -> Result<Option<T>, FromJsonError> {
        match value {
            Value::Null => Ok(None),
            other => T::from_json_owned(other).map(Some),
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: ToJson, const N: usize> ToJson for [T; N] {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}

impl<T: ToJson> ToJson for Vec<T> {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}

impl<T: ToJson> ToJson for VecDeque<T> {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: ToJson> ToJson for BTreeSet<T> {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: ToJson + Ord, S> ToJson for HashSet<T, S> {
    /// Elements are written in sorted order so output is deterministic.
    fn to_json(&self) -> Value {
        let mut items: Vec<&T> = self.iter().collect();
        items.sort_unstable();
        Value::Array(items.into_iter().map(ToJson::to_json).collect())
    }
}

/// "array of 1 element" / "array of 3 elements", for length mismatches.
fn array_of(len: usize) -> String {
    if len == 1 {
        "array of 1 element".to_owned()
    } else {
        format!("array of {len} elements")
    }
}

/// Decodes every element of an array, attaching the index to errors.
fn decode_elements<'v, T: FromJson>(
    value: &'v Value,
) -> Result<impl Iterator<Item = Result<T, FromJsonError>> + 'v, FromJsonError> {
    let items = value
        .as_array()
        .ok_or_else(|| FromJsonError::expected("array", value))?;
    Ok(items
        .iter()
        .enumerate()
        .map(|(i, item)| T::from_json(item).map_err(|e| e.at_index(i))))
}

impl<T: FromJson> FromJson for Vec<T> {
    fn from_json(value: &Value) -> Result<Vec<T>, FromJsonError> {
        decode_elements(value)?.collect()
    }

    fn from_json_owned(value: Value) -> Result<Vec<T>, FromJsonError> {
        match value {
            Value::Array(items) => items
                .into_iter()
                .enumerate()
                .map(|(i, item)| T::from_json_owned(item).map_err(|e| e.at_index(i)))
                .collect(),
            other => Err(FromJsonError::expected("array", &other)),
        }
    }
}

impl<T: FromJson> FromJson for VecDeque<T> {
    fn from_json(value: &Value) -> Result<VecDeque<T>, FromJsonError> {
        decode_elements(value)?.collect()
    }
}

impl<T: FromJson + Ord> FromJson for BTreeSet<T> {
    fn from_json(value: &Value) -> Result<BTreeSet<T>, FromJsonError> {
        decode_elements(value)?.collect()
    }
}

impl<T, S> FromJson for HashSet<T, S>
where
    T: FromJson + Eq + Hash,
    S: BuildHasher + Default,
{
    fn from_json(value: &Value) -> Result<HashSet<T, S>, FromJsonError> {
        decode_elements(value)?.collect()
    }
}
