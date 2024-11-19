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

impl<T: FromJson, const N: usize> FromJson for [T; N] {
    fn from_json(value: &Value) -> Result<[T; N], FromJsonError> {
        let expected = || FromJsonError::expected(array_of(N), value);
        if value.as_array().map(Vec::len) != Some(N) {
            return Err(expected());
        }
        let items: Vec<T> = decode_elements(value)?.collect::<Result<_, _>>()?;
        items.try_into().map_err(|_| expected())
    }
}

impl<K: Display, V: ToJson> ToJson for BTreeMap<K, V> {
    fn to_json(&self) -> Value {
        Value::Object(
            self.iter()
                .map(|(k, v)| (k.to_string(), v.to_json()))
                .collect(),
        )
    }
}

impl<K: Display, V: ToJson, S> ToJson for HashMap<K, V, S> {
    /// Keys are written in sorted order so output is deterministic.
    fn to_json(&self) -> Value {
        let mut entries: Vec<(String, &V)> = self.iter().map(|(k, v)| (k.to_string(), v)).collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Value::Object(entries.into_iter().map(|(k, v)| (k, v.to_json())).collect())
    }
}

/// Decodes every member of an object, parsing keys with `FromStr` and
/// attaching the key to errors.
fn decode_members<'v, K, V>(
    value: &'v Value,
) -> Result<impl Iterator<Item = Result<(K, V), FromJsonError>> + 'v, FromJsonError>
where
    K: FromStr,
    K::Err: Display,
    V: FromJson,
{
    let map = value
        .as_object()
        .ok_or_else(|| FromJsonError::expected("object", value))?;
    Ok(map.iter().map(|(k, v)| {
        let key = k.parse::<K>().map_err(|e| {
            FromJsonError::new(format!("invalid key {}: {e}", super::quote(k))).at_key(k)
        })?;
        let value = V::from_json(v).map_err(|e| e.at_key(k))?;
        Ok((key, value))
    }))
}

impl<K, V> FromJson for BTreeMap<K, V>
where
    K: FromStr + Ord,
    K::Err: Display,
    V: FromJson,
{
    fn from_json(value: &Value) -> Result<BTreeMap<K, V>, FromJsonError> {
        decode_members(value)?.collect()
    }
}

impl<K, V, S> FromJson for HashMap<K, V, S>
where
    K: FromStr + Eq + Hash,
    K::Err: Display,
    V: FromJson,
    S: BuildHasher + Default,
{
    fn from_json(value: &Value) -> Result<HashMap<K, V, S>, FromJsonError> {
        decode_members(value)?.collect()
    }
}

macro_rules! tuple_impls {
    ($len:literal => $($name:ident $idx:tt),+) => {
        impl<$($name: ToJson),+> ToJson for ($($name,)+) {
            fn to_json(&self) -> Value {
                Value::Array(vec![$(self.$idx.to_json()),+])
            }
        }

        impl<$($name: FromJson),+> FromJson for ($($name,)+) {
            fn from_json(value: &Value) -> Result<Self, FromJsonError> {
                match value.as_array() {
                    Some(items) if items.len() == $len => Ok(($(
                        $name::from_json(&items[$idx]).map_err(|e| e.at_index($idx))?,
                    )+)),
                    _ => Err(FromJsonError::expected(array_of($len), value)),
                }
            }
        }
    };
}

tuple_impls!(1 => A 0);
tuple_impls!(2 => A 0, B 1);
tuple_impls!(3 => A 0, B 1, C 2);
tuple_impls!(4 => A 0, B 1, C 2, D 3);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn decode<T: FromJson>(v: Value) -> Result<T, String> {
        T::from_json(&v).map_err(|e| e.to_string())
    }

    #[test]
    fn integers_are_range_checked_with_precise_messages() {
        assert_eq!(decode::<i16>(json!(-32768)), Ok(-32768));
        assert_eq!(
            decode::<i16>(json!(40000)),
            Err("$: expected integer in range -32768..=32767, found 40000".into())
        );
        assert_eq!(
            decode::<u8>(json!(-1)),
            Err("$: expected integer in range 0..=255, found -1".into())
        );
        assert_eq!(
            decode::<u32>(json!("7")),
            Err(r#"$: expected integer in range 0..=4294967295, found string "7""#.into())
        );
        assert_eq!(decode::<u64>(json!(u64::MAX)), Ok(u64::MAX));
        assert_eq!(decode::<i64>(json!(i64::MIN)), Ok(i64::MIN));
        assert!(decode::<i64>(json!(u64::MAX)).is_err());
    }

    #[test]
    fn integers_accept_integral_floats_only() {
        assert_eq!(decode::<u8>(json!(5.0)), Ok(5));
        assert_eq!(decode::<i32>(json!(-2e3)), Ok(-2000));
        assert_eq!(
            decode::<u8>(json!(1.5)),
            Err("$: expected integer in range 0..=255, found 1.5".into())
        );
        assert!(decode::<u64>(json!(1e20)).is_err());
    }

    #[test]
    fn floats_accept_any_number_and_f32_checks_range() {
        assert_eq!(decode::<f64>(json!(3)), Ok(3.0));
        assert_eq!(decode::<f32>(json!(0.1)), Ok(0.1f32));
        assert_eq!(
            decode::<f32>(json!(1e300)),
            Err("$: expected number within f32 range, found 1e300".into())
        );
        assert_eq!(
            decode::<f64>(json!(null)),
            Err("$: expected number, found null".into())
        );
        assert_eq!(f64::NAN.to_json(), Value::Null);
        assert_eq!(0.1f32.to_json(), json!(0.1));
    }

    #[test]
    fn strings_chars_and_unit() {
        assert_eq!(decode::<String>(json!("s")), Ok("s".into()));
        assert_eq!(String::from_json_owned(json!("own")), Ok("own".into()));
        assert_eq!(decode::<char>(json!("é")), Ok('é'));
        assert_eq!(
            decode::<char>(json!("ab")),
            Err(r#"$: expected single-character string, found string "ab""#.into())
        );
        assert!(decode::<char>(json!("")).is_err());
        assert!(decode::<char>(json!(1)).is_err());
        assert_eq!(decode::<()>(json!(null)), Ok(()));
        assert!(decode::<()>(json!(0)).is_err());
        assert_eq!('x'.to_json(), json!("x"));
        assert_eq!("str".to_json(), json!("str"));
    }

    #[test]
    fn options_map_null_to_none() {
        assert_eq!(decode::<Option<u8>>(json!(null)), Ok(None));
        assert_eq!(decode::<Option<u8>>(json!(3)), Ok(Some(3)));
        assert!(decode::<Option<u8>>(json!("3")).is_err());
        assert_eq!(Some(1).to_json(), json!(1));
        assert_eq!(None::<u8>.to_json(), Value::Null);
        assert_eq!(
            Option::<String>::from_json_owned(json!("x")),
            Ok(Some("x".into()))
        );
    }

    #[test]
    fn sequences_attach_indices_to_errors() {
        assert_eq!(decode::<Vec<u8>>(json!([1, 2])), Ok(vec![1, 2]));
        assert_eq!(
            decode::<Vec<Vec<u8>>>(json!([[1], [2, 300]])),
            Err("$[1][1]: expected integer in range 0..=255, found 300".into())
        );
        assert_eq!(
            Vec::<u8>::from_json_owned(json!([1, "x"])).map_err(|e| e.to_string()),
            Err(r#"$[1]: expected integer in range 0..=255, found string "x""#.into())
        );
        assert_eq!(
            decode::<Vec<u8>>(json!({})),
            Err("$: expected array, found empty object".into())
        );
        assert_eq!(decode::<VecDeque<u8>>(json!([3])), Ok(VecDeque::from([3])));
        assert_eq!(
            decode::<BTreeSet<u8>>(json!([3, 1, 3])),
            Ok(BTreeSet::from([1, 3]))
        );
        let set: HashSet<String> = decode(json!(["a", "b"])).unwrap();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn fixed_arrays_require_exact_length() {
        assert_eq!(decode::<[u8; 2]>(json!([1, 2])), Ok([1, 2]));
        assert_eq!(
            decode::<[u8; 2]>(json!([1, 2, 3])),
            Err("$: expected array of 2 elements, found array of 3 elements".into())
        );
        assert_eq!(
            decode::<[u8; 2]>(json!([1, -2])),
            Err("$[1]: expected integer in range 0..=255, found -2".into())
        );
        assert_eq!([1u8, 2].to_json(), json!([1, 2]));
    }

    #[test]
    fn hash_containers_serialize_deterministically() {
        let set: HashSet<&str> = ["c", "a", "b"].into_iter().collect();
        assert_eq!(set.to_json(), json!(["a", "b", "c"]));
        let map: HashMap<String, u8> = (0..20u8).map(|i| (format!("k{i:02}"), i)).collect();
        let keys: Vec<String> = map
            .to_json()
            .as_object()
            .unwrap()
            .keys()
            .map(String::from)
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn maps_parse_keys_with_from_str() {
        let v = json!({"1": "one", "20": "twenty"});
        let by_num: BTreeMap<u32, String> = decode(v.clone()).unwrap();
        assert_eq!(by_num.get(&20).map(String::as_str), Some("twenty"));
        assert_eq!(by_num.to_json(), v);
        let bad = decode::<BTreeMap<u32, String>>(json!({"x1": "?"}));
        assert_eq!(
            bad,
            Err(r#"$.x1: invalid key "x1": invalid digit found in string"#.into())
        );
        let nested = decode::<HashMap<String, Vec<u8>>>(json!({"ok": [1], "bad key": [1, 256]}));
        assert_eq!(
            nested,
            Err(r#"$["bad key"][1]: expected integer in range 0..=255, found 256"#.into())
        );
    }

    #[test]
    fn tuples_check_arity() {
        assert_eq!(decode::<(u8, String)>(json!([1, "a"])), Ok((1, "a".into())));
        assert_eq!(
            decode::<(u8, String)>(json!([1])),
            Err("$: expected array of 2 elements, found array of 1 element".into())
        );
        assert_eq!(
            decode::<(u8, bool, u8)>(json!([1, true, "x"])),
            Err(r#"$[2]: expected integer in range 0..=255, found string "x""#.into())
        );
        assert_eq!((1, "a", true, 2.5).to_json(), json!([1, "a", true, 2.5]));
        assert_eq!(decode::<(u8,)>(json!([9])), Ok((9,)));
        assert_eq!(
            decode::<(u8,)>(json!([])),
            Err("$: expected array of 1 element, found empty array".into())
        );
    }

    #[test]
    fn smart_pointers_delegate() {
        assert_eq!(decode::<Box<u8>>(json!(1)), Ok(Box::new(1)));
        assert_eq!(decode::<Arc<String>>(json!("a")).unwrap().as_str(), "a");
        assert_eq!(decode::<Rc<bool>>(json!(true)).map(|b| *b), Ok(true));
        assert_eq!(Box::new(5u8).to_json(), json!(5));
        let shared: Arc<str> = Arc::from("s");
        assert_eq!(shared.to_json(), json!("s"));
        assert_eq!(Cow::Borrowed("c").to_json(), json!("c"));
    }

    #[test]
    fn value_map_and_number_pass_through() {
        let v = json!({"a": [1]});
        assert_eq!(decode::<Value>(v.clone()), Ok(v.clone()));
        assert_eq!(decode::<Map>(v.clone()).map(|m| m.len()), Ok(1));
        assert_eq!(Map::from_json_owned(v.clone()).map(|m| m.len()), Ok(1));
        assert!(decode::<Map>(json!([])).is_err());
        assert_eq!(decode::<Number>(json!(2)), Ok(Number::from(2)));
        assert!(decode::<Number>(json!("2")).is_err());
    }
}
